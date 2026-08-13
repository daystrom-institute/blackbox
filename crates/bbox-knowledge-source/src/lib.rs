//! Dependency-clean contracts for remote knowledge and gap source transport.

use std::collections::{BTreeMap, BTreeSet};

use bbox_corpus_core::identity::PublishedScope;
use bro_core::WorkspaceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_SOURCE_FILES_PER_LANE: u64 = 100_000;
pub const MAX_SOURCE_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_SOURCE_LANE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_GRAPHS_PER_LANE: u64 = 1_024;
pub const MAX_GRAPH_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_GRAPH_ROWS_PER_FILE: u64 = 1_000_000;
/// The only filename the evidence lane admits. Held here rather than imported
/// from the graph kernel so this crate stays a dependency-clean wire and
/// validation leaf; `bbox_project_graph::EVIDENCE_BINDINGS_FILENAME` is the
/// same string and a contract test pins them together.
pub const EVIDENCE_BINDINGS_FILENAME: &str = "bindings.json";
pub const MAX_ANCESTRY_NODES: u64 = 2_000_000;
pub const MAX_ANCESTRY_EDGES: u64 = 8_000_000;
pub const MAX_ANCESTRY_PAGE_NODES: u64 = 2_000;
pub const MAX_ANCESTRY_PAGE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_MANIFEST_PAGE_ENTRIES: u64 = 2_000;
pub const MAX_MANIFEST_PAGE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_MANIFEST_PAGES: u64 = 100_000;
pub const MAX_OPEN_UPLOADS: u64 = 1_024;
pub const MAX_GENERATION_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_REPOSITORY_RELATIVE_FILENAME_BYTES: usize = 4096;
pub const MAX_FULL_REF_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnowledgeSourceLimits {
    pub max_files_per_lane: u64,
    pub max_file_bytes: u64,
    pub max_lane_bytes: u64,
    pub max_graphs_per_lane: u64,
    pub max_graph_bytes: u64,
    pub max_graph_rows_per_file: u64,
    pub max_ancestry_nodes: u64,
    pub max_ancestry_edges: u64,
    pub max_ancestry_page_nodes: u64,
    pub max_ancestry_page_bytes: u64,
    pub max_manifest_page_entries: u64,
    pub max_manifest_page_bytes: u64,
    pub max_manifest_pages: u64,
    pub max_open_uploads: u64,
    pub max_generation_bytes: u64,
}

impl Default for KnowledgeSourceLimits {
    fn default() -> Self {
        Self {
            max_files_per_lane: MAX_SOURCE_FILES_PER_LANE,
            max_file_bytes: MAX_SOURCE_FILE_BYTES,
            max_lane_bytes: MAX_SOURCE_LANE_BYTES,
            max_graphs_per_lane: MAX_GRAPHS_PER_LANE,
            max_graph_bytes: MAX_GRAPH_BYTES,
            max_graph_rows_per_file: MAX_GRAPH_ROWS_PER_FILE,
            max_ancestry_nodes: MAX_ANCESTRY_NODES,
            max_ancestry_edges: MAX_ANCESTRY_EDGES,
            max_ancestry_page_nodes: MAX_ANCESTRY_PAGE_NODES,
            max_ancestry_page_bytes: MAX_ANCESTRY_PAGE_BYTES,
            max_manifest_page_entries: MAX_MANIFEST_PAGE_ENTRIES,
            max_manifest_page_bytes: MAX_MANIFEST_PAGE_BYTES,
            max_manifest_pages: MAX_MANIFEST_PAGES,
            max_open_uploads: MAX_OPEN_UPLOADS,
            max_generation_bytes: MAX_GENERATION_BYTES,
        }
    }
}

impl KnowledgeSourceLimits {
    pub fn validate(self) -> Result<(), ContractError> {
        validate_limit(self.max_files_per_lane, MAX_SOURCE_FILES_PER_LANE)?;
        validate_limit(self.max_file_bytes, MAX_SOURCE_FILE_BYTES)?;
        validate_limit(self.max_lane_bytes, MAX_SOURCE_LANE_BYTES)?;
        validate_limit(self.max_graphs_per_lane, MAX_GRAPHS_PER_LANE)?;
        validate_limit(self.max_graph_bytes, MAX_GRAPH_BYTES)?;
        validate_limit(self.max_graph_rows_per_file, MAX_GRAPH_ROWS_PER_FILE)?;
        validate_limit(self.max_ancestry_nodes, MAX_ANCESTRY_NODES)?;
        validate_limit(self.max_ancestry_edges, MAX_ANCESTRY_EDGES)?;
        validate_limit(self.max_ancestry_page_nodes, MAX_ANCESTRY_PAGE_NODES)?;
        validate_limit(self.max_ancestry_page_bytes, MAX_ANCESTRY_PAGE_BYTES)?;
        validate_limit(self.max_manifest_page_entries, MAX_MANIFEST_PAGE_ENTRIES)?;
        validate_limit(self.max_manifest_page_bytes, MAX_MANIFEST_PAGE_BYTES)?;
        validate_limit(self.max_manifest_pages, MAX_MANIFEST_PAGES)?;
        validate_limit(self.max_open_uploads, MAX_OPEN_UPLOADS)?;
        validate_limit(self.max_generation_bytes, MAX_GENERATION_BYTES)?;
        Ok(())
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    #[error("unsupported knowledge-source schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid published scope")]
    InvalidScope,
    #[error("invalid full publisher ref")]
    InvalidFullRef,
    #[error("invalid Git object id")]
    InvalidObjectId,
    #[error("invalid sha256 digest")]
    InvalidDigest,
    #[error("invalid producer id")]
    InvalidProducerId,
    #[error("knowledge-source input is internally inconsistent")]
    InvalidInput,
    #[error("invalid repository-relative source filename")]
    InvalidSourceFilename,
    #[error("knowledge-source limit is zero or exceeds its hard ceiling")]
    InvalidLimit,
    #[error("source manifest is not strictly sorted")]
    ManifestOutOfOrder,
    #[error("source manifest count or byte total does not match its descriptor")]
    ManifestCountMismatch,
    #[error("source manifest commitment does not match its descriptor")]
    ManifestCommitmentMismatch,
    #[error("source manifest exceeds an enforced limit")]
    ManifestLimitExceeded,
    #[error("source manifest page is invalid")]
    InvalidManifestPage,
    #[error("ancestry witness is not strictly sorted")]
    AncestryOutOfOrder,
    #[error("ancestry witness contains an invalid or duplicate parent")]
    InvalidAncestryParent,
    #[error("ancestry witness is incomplete")]
    AncestryIncomplete,
    #[error("ancestry witness contains unreachable nodes")]
    AncestryUnreachable,
    #[error("ancestry witness contains a cycle")]
    AncestryCycle,
    #[error("claimed merge base is not reachable from both commits")]
    InvalidMergeBase,
    #[error("ancestry count does not match its descriptor")]
    AncestryCountMismatch,
    #[error("ancestry commitment does not match its descriptor")]
    AncestryCommitmentMismatch,
    #[error("ancestry witness exceeds an enforced limit")]
    AncestryLimitExceeded,
    #[error("generation exceeds its enforced byte limit")]
    GenerationLimitExceeded,
    #[error("graph source path is not exactly <graph-id>/<known-file>")]
    InvalidGraphSourcePath,
    #[error(
        "graph source manifest does not contain exactly one schema and two fact files per graph"
    )]
    IncompleteGraphSource,
    #[error("graph source exceeds an enforced graph limit")]
    GraphLimitExceeded,
    #[error("evidence source path is not exactly the single bindings document")]
    InvalidEvidenceSourcePath,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitObjectFormatV1 {
    Sha1,
    Sha256,
}

impl GitObjectFormatV1 {
    pub fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Sha1 => 1,
            Self::Sha256 => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SourceLaneV1 {
    Knowledge,
    Gaps,
    Graphs,
    /// Tenant-owned cross-entity evidence bindings
    /// (`.bbox/evidence/bindings.json`). A single-document lane: it carries at
    /// most one file, because one complete valid document replaces one
    /// project's accepted binding set.
    Evidence,
}

impl SourceLaneV1 {
    fn directory(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Gaps => "gaps",
            Self::Graphs => "graphs",
            Self::Evidence => "evidence",
        }
    }

    /// Domain separator for this lane's manifest digest. Tags are permanent:
    /// changing one silently invalidates every stored manifest commitment.
    fn tag(self) -> u8 {
        match self {
            Self::Knowledge => 1,
            Self::Gaps => 2,
            Self::Graphs => 3,
            Self::Evidence => 4,
        }
    }
}

/// Which lanes a stored generation identity or working-pair commitment was
/// minted over.
///
/// Every rung is the exact preimage some shipped binary used, so this ladder
/// is APPEND-ONLY: a new lane adds a rung at the top and never edits one
/// below. Editing a lower rung orphans every resource that binary wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneVintage {
    /// Before the graphs lane: knowledge and gaps only.
    PreGraphs,
    /// Before the evidence lane: knowledge, gaps, graphs.
    PreEvidence,
    /// Current: knowledge, gaps, graphs, evidence.
    Current,
}

impl LaneVintage {
    fn includes_graphs(self) -> bool {
        matches!(self, Self::PreEvidence | Self::Current)
    }

    fn includes_evidence(self) -> bool {
        matches!(self, Self::Current)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotClassV1 {
    Baseline,
    Working,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceManifestDescriptorV1 {
    pub manifest_sha256: String,
    pub file_count: u64,
    pub logical_bytes: u64,
    pub page_count: u64,
}

impl Default for SourceManifestDescriptorV1 {
    fn default() -> Self {
        Self {
            manifest_sha256: source_manifest_sha256(SourceLaneV1::Graphs, &[]),
            file_count: 0,
            logical_bytes: 0,
            page_count: 0,
        }
    }
}

impl SourceManifestDescriptorV1 {
    /// True when this descriptor is exactly the canonical empty lane, which is
    /// also what a lane absent from stored state decodes to. State written
    /// before the graphs lane existed carries no graph descriptor at all, so
    /// this is the only shape a pre-graphs resource can present for a graph
    /// lane, and it is the discriminator every legacy tolerance keys on.
    pub fn is_absent_lane(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate_header(&self, limits: KnowledgeSourceLimits) -> Result<(), ContractError> {
        limits.validate()?;
        validate_sha256(&self.manifest_sha256)?;
        let minimum_pages = self.file_count.div_ceil(limits.max_manifest_page_entries);
        if self.file_count > limits.max_files_per_lane
            || self.logical_bytes > limits.max_lane_bytes
            || self.page_count > limits.max_manifest_pages
            || self.page_count > self.file_count
            || self.page_count < minimum_pages
            || (self.file_count == 0) != (self.page_count == 0)
        {
            return Err(ContractError::ManifestLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceFileManifestEntryV1 {
    pub repository_relative_filename: String,
    pub encoded_bytes: u64,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceManifestPageV1 {
    pub page_index: u64,
    pub entries: Vec<SourceFileManifestEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationCandidateDescriptorV1 {
    pub schema_version: u32,
    pub scope: PublishedScope,
    pub full_ref: String,
    pub publisher_commit: String,
    pub object_format: GitObjectFormatV1,
    pub knowledge: SourceManifestDescriptorV1,
    pub gaps: SourceManifestDescriptorV1,
    #[serde(default)]
    pub graphs: SourceManifestDescriptorV1,
    /// Absent in every candidate written before the evidence lane existed;
    /// `default` decodes those to the canonical empty lane.
    #[serde(default)]
    pub evidence: SourceManifestDescriptorV1,
}

impl PublicationCandidateDescriptorV1 {
    pub fn validate_header(&self, limits: KnowledgeSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        validate_scope(&self.scope)?;
        validate_full_ref(&self.full_ref)?;
        validate_object_id(&self.publisher_commit, self.object_format)?;
        self.knowledge.validate_header(limits)?;
        self.gaps.validate_header(limits)?;
        self.graphs.validate_header(limits)?;
        self.evidence.validate_header(limits)?;
        validate_total_bytes(
            [
                self.knowledge.logical_bytes,
                self.gaps.logical_bytes,
                self.graphs.logical_bytes,
                self.evidence.logical_bytes,
            ],
            limits.max_generation_bytes,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AncestryCommitV1 {
    pub commit_oid: String,
    pub parent_oids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AncestryDescriptorV1 {
    pub ancestry_sha256: String,
    pub node_count: u64,
    pub edge_count: u64,
    pub page_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AncestryPageV1 {
    pub page_index: u64,
    pub nodes: Vec<AncestryCommitV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StableCaptureV1 {
    pub transaction_pending_before: bool,
    pub transaction_pending_after: bool,
    pub first_working_pair_sha256: String,
    pub second_working_pair_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalWorkspaceDescriptorV1 {
    pub schema_version: u32,
    pub scope: PublishedScope,
    pub workspace_id: WorkspaceId,
    pub sequence: u64,
    pub accepted_generation: String,
    pub accepted_commit: String,
    pub checkout_head: String,
    pub merge_base: String,
    pub object_format: GitObjectFormatV1,
    pub ancestry: AncestryDescriptorV1,
    pub capture: StableCaptureV1,
    pub baseline_knowledge: SourceManifestDescriptorV1,
    pub baseline_gaps: SourceManifestDescriptorV1,
    #[serde(default)]
    pub baseline_graphs: SourceManifestDescriptorV1,
    /// Absent in every workspace captured before the evidence lane existed.
    #[serde(default)]
    pub baseline_evidence: SourceManifestDescriptorV1,
    pub working_knowledge: SourceManifestDescriptorV1,
    pub working_gaps: SourceManifestDescriptorV1,
    #[serde(default)]
    pub working_graphs: SourceManifestDescriptorV1,
    /// Absent in every workspace captured before the evidence lane existed.
    #[serde(default)]
    pub working_evidence: SourceManifestDescriptorV1,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceGenerationStateV1 {
    ReceivingManifest,
    MissingBlobs,
    Ready,
    Superseded,
    Retired,
    Expired,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginSourceUploadResponseV1 {
    pub upload_id: String,
    pub max_manifest_page_entries: u64,
    pub max_manifest_page_bytes: u64,
    pub max_ancestry_page_nodes: u64,
    pub max_ancestry_page_bytes: u64,
    pub max_blob_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationProbeRequestV1 {
    pub scope: PublishedScope,
    pub full_ref: String,
    pub publisher_commit: String,
    pub object_format: GitObjectFormatV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationProbeResponseV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<PublicationCandidateStatusV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginPublicationUploadRequestV1 {
    pub descriptor: PublicationCandidateDescriptorV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalProbeRequestV1 {
    pub scope: PublishedScope,
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalProbeResponseV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ProvisionalWorkspaceStatusV1>,
    /// The next durable sequence for this workspace. This remains monotonic
    /// after the current lease expires or its pointer is explicitly retired.
    pub next_sequence: u64,
}

/// Daemon-authenticated inputs a workspace owner must pin before capturing a
/// provisional generation. The project id is deliberately absent: the
/// workspace binding already selects it, while the source descriptor carries
/// only the portable published scope and accepted content identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalCaptureContextV1 {
    pub scope: PublishedScope,
    pub accepted_generation: String,
    pub accepted_commit: String,
    pub lease_ttl_secs: u64,
}

impl ProvisionalCaptureContextV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_scope(&self.scope)?;
        validate_sha256(&self.accepted_generation)?;
        if self.lease_ttl_secs == 0 {
            return Err(ContractError::InvalidLimit);
        }
        match self.accepted_commit.len() {
            40 => validate_object_id(&self.accepted_commit, GitObjectFormatV1::Sha1),
            64 => validate_object_id(&self.accepted_commit, GitObjectFormatV1::Sha256),
            _ => Err(ContractError::InvalidObjectId),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginProvisionalUploadRequestV1 {
    pub descriptor: ProvisionalWorkspaceDescriptorV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinalizeProvisionalUploadRequestV1 {
    pub lease_ttl_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenewProvisionalGenerationRequestV1 {
    pub lease_ttl_secs: u64,
}

impl PublicationProbeRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_scope(&self.scope)?;
        validate_full_ref(&self.full_ref)?;
        validate_object_id(&self.publisher_commit, self.object_format)
    }
}

impl ProvisionalProbeRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_scope(&self.scope)?;
        WorkspaceId::parse(self.workspace_id.as_str()).map_err(|_| ContractError::InvalidInput)?;
        Ok(())
    }
}

impl ProvisionalProbeResponseV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.next_sequence == 0
            || self
                .current
                .as_ref()
                .is_some_and(|current| current.sequence >= self.next_sequence)
        {
            return Err(ContractError::InvalidInput);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MissingSourceBlobsPageV1 {
    pub source_generation_id: String,
    pub hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FinalizeSourceUploadResponseV1 {
    pub source_generation_id: String,
    pub status_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationCandidateStatusV1 {
    pub source_generation_id: String,
    pub state: SourceGenerationStateV1,
    pub producer_id: String,
    pub full_ref: String,
    pub publisher_commit: String,
    pub object_format: GitObjectFormatV1,
    pub observed_at_unix_secs: u64,
    pub knowledge_manifest_sha256: String,
    pub gap_manifest_sha256: String,
    #[serde(default)]
    pub graph_manifest_sha256: String,
    #[serde(default)]
    pub evidence_manifest_sha256: String,
    pub knowledge_files: u64,
    pub gap_files: u64,
    #[serde(default)]
    pub graph_files: u64,
    #[serde(default)]
    pub evidence_files: u64,
    pub logical_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvisionalWorkspaceStatusV1 {
    pub source_generation_id: String,
    pub state: SourceGenerationStateV1,
    pub workspace_id: WorkspaceId,
    pub sequence: u64,
    pub accepted_generation: String,
    pub checkout_head: String,
    pub observed_at_unix_secs: u64,
    pub baseline_knowledge_manifest_sha256: String,
    pub baseline_gap_manifest_sha256: String,
    #[serde(default)]
    pub baseline_graph_manifest_sha256: String,
    #[serde(default)]
    pub baseline_evidence_manifest_sha256: String,
    pub working_knowledge_manifest_sha256: String,
    pub working_gap_manifest_sha256: String,
    #[serde(default)]
    pub working_graph_manifest_sha256: String,
    #[serde(default)]
    pub working_evidence_manifest_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_unix_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl ProvisionalWorkspaceDescriptorV1 {
    pub fn validate_header(&self, limits: KnowledgeSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        validate_scope(&self.scope)?;
        validate_sha256(&self.accepted_generation)?;
        validate_object_id(&self.accepted_commit, self.object_format)?;
        validate_object_id(&self.checkout_head, self.object_format)?;
        validate_object_id(&self.merge_base, self.object_format)?;
        validate_ancestry_descriptor(&self.ancestry, limits)?;
        self.capture.validate()?;
        self.baseline_knowledge.validate_header(limits)?;
        self.baseline_gaps.validate_header(limits)?;
        self.baseline_graphs.validate_header(limits)?;
        self.baseline_evidence.validate_header(limits)?;
        self.working_knowledge.validate_header(limits)?;
        self.working_gaps.validate_header(limits)?;
        self.working_graphs.validate_header(limits)?;
        self.working_evidence.validate_header(limits)?;
        // A workspace commits to a working pair over the lanes its own binary
        // knew about, so admission walks the vintage ladder rather than a
        // single legacy special case. Each older rung is admissible only for
        // the exact absent-lane shape a binary of that vintage could have
        // produced: a capture carrying real evidence content has no
        // pre-evidence vintage, and one carrying graph content has no
        // pre-graphs vintage.
        if !self
            .admissible_working_pair_vintages()
            .into_iter()
            .any(|vintage| {
                self.capture.first_working_pair_sha256 == self.working_pair_for_vintage(vintage)
            })
        {
            return Err(ContractError::InvalidInput);
        }
        validate_total_bytes(
            [
                self.baseline_knowledge.logical_bytes,
                self.baseline_gaps.logical_bytes,
                self.baseline_graphs.logical_bytes,
                self.baseline_evidence.logical_bytes,
                self.working_knowledge.logical_bytes,
                self.working_gaps.logical_bytes,
                self.working_graphs.logical_bytes,
                self.working_evidence.logical_bytes,
            ],
            limits.max_generation_bytes,
        )
    }

    /// The vintages this descriptor's shape could legitimately have been
    /// minted by, newest first.
    fn admissible_working_pair_vintages(&self) -> Vec<LaneVintage> {
        let mut vintages = vec![LaneVintage::Current];
        if self.working_evidence.is_absent_lane() {
            vintages.push(LaneVintage::PreEvidence);
            if self.working_graphs.is_absent_lane() {
                vintages.push(LaneVintage::PreGraphs);
            }
        }
        vintages
    }

    fn working_pair_for_vintage(&self, vintage: LaneVintage) -> String {
        working_pair_for_vintage(
            &self.working_knowledge,
            &self.working_gaps,
            &self.working_graphs,
            &self.working_evidence,
            vintage,
        )
    }
}

impl StableCaptureV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_sha256(&self.first_working_pair_sha256)?;
        validate_sha256(&self.second_working_pair_sha256)?;
        if self.transaction_pending_before
            || self.transaction_pending_after
            || self.first_working_pair_sha256 != self.second_working_pair_sha256
        {
            return Err(ContractError::InvalidInput);
        }
        Ok(())
    }
}

pub fn source_file_blob_sha256(bytes: &[u8]) -> String {
    sha256(bytes)
}

/// The stable-capture commitment over every working lane.
///
/// Evidence participates because `.bbox/evidence/bindings.json` is
/// checkout-plane state an author can edit mid-capture; leaving it out would
/// let an evidence-only edit slip through the stability re-read undetected.
pub fn working_pair_sha256(
    knowledge: &SourceManifestDescriptorV1,
    gaps: &SourceManifestDescriptorV1,
    graphs: &SourceManifestDescriptorV1,
    evidence: &SourceManifestDescriptorV1,
) -> String {
    working_pair_for_vintage(knowledge, gaps, graphs, evidence, LaneVintage::Current)
}

/// The working-pair commitment exactly as it was computed before the evidence
/// lane existed: knowledge, gaps, and graphs. Never mint with this; it exists
/// so stored pre-evidence captures stay readable.
pub fn pre_evidence_working_pair_sha256(
    knowledge: &SourceManifestDescriptorV1,
    gaps: &SourceManifestDescriptorV1,
    graphs: &SourceManifestDescriptorV1,
) -> String {
    working_pair_for_vintage(
        knowledge,
        gaps,
        graphs,
        &SourceManifestDescriptorV1::default(),
        LaneVintage::PreEvidence,
    )
}

/// The working-pair commitment exactly as it was computed before the graphs
/// lane existed: knowledge and gaps only. Never mint with this; it exists so
/// stored pre-graphs captures stay readable.
pub fn legacy_working_pair_sha256(
    knowledge: &SourceManifestDescriptorV1,
    gaps: &SourceManifestDescriptorV1,
) -> String {
    working_pair_for_vintage(
        knowledge,
        gaps,
        &SourceManifestDescriptorV1::default(),
        &SourceManifestDescriptorV1::default(),
        LaneVintage::PreGraphs,
    )
}

/// One preimage builder for every rung of the ladder. Older vintages simply
/// stop hashing earlier, which is exactly what their binaries did.
fn working_pair_for_vintage(
    knowledge: &SourceManifestDescriptorV1,
    gaps: &SourceManifestDescriptorV1,
    graphs: &SourceManifestDescriptorV1,
    evidence: &SourceManifestDescriptorV1,
    vintage: LaneVintage,
) -> String {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-knowledge-working-pair-v1");
    hash_manifest_descriptor(&mut encoded, knowledge);
    hash_manifest_descriptor(&mut encoded, gaps);
    if vintage.includes_graphs() {
        hash_manifest_descriptor(&mut encoded, graphs);
    }
    if vintage.includes_evidence() {
        hash_manifest_descriptor(&mut encoded, evidence);
    }
    sha256(&encoded)
}

pub fn validate_source_blob(
    entry: &SourceFileManifestEntryV1,
    bytes: &[u8],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    limits.validate()?;
    validate_sha256(&entry.content_sha256)?;
    if (!graph_source_may_be_empty(&entry.repository_relative_filename) && entry.encoded_bytes == 0)
        || entry.encoded_bytes > limits.max_file_bytes
        || bytes.len() as u64 != entry.encoded_bytes
    {
        return Err(ContractError::ManifestCountMismatch);
    }
    if source_file_blob_sha256(bytes) != entry.content_sha256 {
        return Err(ContractError::ManifestCommitmentMismatch);
    }
    Ok(())
}

pub fn source_manifest_sha256(lane: SourceLaneV1, entries: &[SourceFileManifestEntryV1]) -> String {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-knowledge-source-manifest-v1");
    encoded.push(lane.tag());
    encoded.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for entry in entries {
        push_field(&mut encoded, entry.repository_relative_filename.as_bytes());
        encoded.extend_from_slice(&entry.encoded_bytes.to_be_bytes());
        push_field(&mut encoded, entry.content_sha256.as_bytes());
    }
    sha256(&encoded)
}

pub fn validate_manifest_page(
    descriptor: &SourceManifestDescriptorV1,
    page: &SourceManifestPageV1,
    encoded_page_bytes: u64,
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    descriptor.validate_header(limits)?;
    if page.entries.is_empty()
        || page.page_index >= descriptor.page_count
        || page.entries.len() as u64 > limits.max_manifest_page_entries
        || encoded_page_bytes == 0
        || encoded_page_bytes > limits.max_manifest_page_bytes
    {
        return Err(ContractError::InvalidManifestPage);
    }
    Ok(())
}

pub fn validate_ancestry_page(
    descriptor: &AncestryDescriptorV1,
    page: &AncestryPageV1,
    encoded_page_bytes: u64,
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    validate_ancestry_descriptor(descriptor, limits)?;
    if page.nodes.is_empty()
        || page.page_index >= descriptor.page_count
        || page.nodes.len() as u64 > limits.max_ancestry_page_nodes
        || encoded_page_bytes == 0
        || encoded_page_bytes > limits.max_ancestry_page_bytes
    {
        return Err(ContractError::InvalidManifestPage);
    }
    Ok(())
}

pub fn validate_source_manifest(
    scope: &PublishedScope,
    lane: SourceLaneV1,
    descriptor: &SourceManifestDescriptorV1,
    entries: &[SourceFileManifestEntryV1],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    validate_scope(scope)?;
    descriptor.validate_header(limits)?;
    if entries.len() as u64 != descriptor.file_count {
        return Err(ContractError::ManifestCountMismatch);
    }
    let mut previous: Option<&str> = None;
    let mut logical_bytes = 0_u64;
    for entry in entries {
        if previous.is_some_and(|prior| prior >= entry.repository_relative_filename.as_str()) {
            return Err(ContractError::ManifestOutOfOrder);
        }
        validate_source_filename(scope, lane, &entry.repository_relative_filename)?;
        validate_sha256(&entry.content_sha256)?;
        if (!graph_source_may_be_empty(&entry.repository_relative_filename)
            && entry.encoded_bytes == 0)
            || entry.encoded_bytes > limits.max_file_bytes
        {
            return Err(ContractError::ManifestLimitExceeded);
        }
        logical_bytes = logical_bytes
            .checked_add(entry.encoded_bytes)
            .ok_or(ContractError::ManifestLimitExceeded)?;
        previous = Some(&entry.repository_relative_filename);
    }
    if logical_bytes != descriptor.logical_bytes {
        return Err(ContractError::ManifestCountMismatch);
    }
    if source_manifest_sha256(lane, entries) != descriptor.manifest_sha256 {
        return Err(ContractError::ManifestCommitmentMismatch);
    }
    Ok(())
}

pub fn validate_publication_candidate(
    descriptor: &PublicationCandidateDescriptorV1,
    knowledge: &[SourceFileManifestEntryV1],
    gaps: &[SourceFileManifestEntryV1],
    graphs: &[SourceFileManifestEntryV1],
    evidence: &[SourceFileManifestEntryV1],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    descriptor.validate_header(limits)?;
    validate_source_manifest(
        &descriptor.scope,
        SourceLaneV1::Knowledge,
        &descriptor.knowledge,
        knowledge,
        limits,
    )?;
    validate_source_manifest(
        &descriptor.scope,
        SourceLaneV1::Gaps,
        &descriptor.gaps,
        gaps,
        limits,
    )?;
    validate_source_manifest(
        &descriptor.scope,
        SourceLaneV1::Graphs,
        &descriptor.graphs,
        graphs,
        limits,
    )?;
    validate_source_manifest(
        &descriptor.scope,
        SourceLaneV1::Evidence,
        &descriptor.evidence,
        evidence,
        limits,
    )?;
    validate_graph_source_manifest(&descriptor.scope, graphs, limits)?;
    validate_evidence_source_manifest(evidence)
}

pub fn ancestry_sha256(format: GitObjectFormatV1, nodes: &[AncestryCommitV1]) -> String {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-knowledge-source-ancestry-v1");
    encoded.push(format.tag());
    encoded.extend_from_slice(&(nodes.len() as u64).to_be_bytes());
    for node in nodes {
        push_field(&mut encoded, node.commit_oid.as_bytes());
        encoded.extend_from_slice(&(node.parent_oids.len() as u64).to_be_bytes());
        for parent in &node.parent_oids {
            push_field(&mut encoded, parent.as_bytes());
        }
    }
    sha256(&encoded)
}

pub fn validate_ancestry(
    descriptor: &AncestryDescriptorV1,
    nodes: &[AncestryCommitV1],
    object_format: GitObjectFormatV1,
    checkout_head: &str,
    accepted_commit: &str,
    merge_base: &str,
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    validate_ancestry_descriptor(descriptor, limits)?;
    validate_object_id(checkout_head, object_format)?;
    validate_object_id(accepted_commit, object_format)?;
    validate_object_id(merge_base, object_format)?;
    if nodes.len() as u64 != descriptor.node_count {
        return Err(ContractError::AncestryCountMismatch);
    }

    let mut by_oid = BTreeMap::new();
    let mut previous: Option<&str> = None;
    let mut edge_count = 0_u64;
    for node in nodes {
        validate_object_id(&node.commit_oid, object_format)?;
        if previous.is_some_and(|prior| prior >= node.commit_oid.as_str()) {
            return Err(ContractError::AncestryOutOfOrder);
        }
        let mut parents = BTreeSet::new();
        for parent in &node.parent_oids {
            validate_object_id(parent, object_format)?;
            if parent == &node.commit_oid || !parents.insert(parent.as_str()) {
                return Err(ContractError::InvalidAncestryParent);
            }
        }
        edge_count = edge_count
            .checked_add(node.parent_oids.len() as u64)
            .ok_or(ContractError::AncestryLimitExceeded)?;
        by_oid.insert(node.commit_oid.as_str(), node);
        previous = Some(&node.commit_oid);
    }
    if edge_count != descriptor.edge_count {
        return Err(ContractError::AncestryCountMismatch);
    }
    if edge_count > limits.max_ancestry_edges {
        return Err(ContractError::AncestryLimitExceeded);
    }
    for node in nodes {
        if node
            .parent_oids
            .iter()
            .any(|parent| !by_oid.contains_key(parent.as_str()))
        {
            return Err(ContractError::AncestryIncomplete);
        }
    }
    if !by_oid.contains_key(checkout_head)
        || !by_oid.contains_key(accepted_commit)
        || !by_oid.contains_key(merge_base)
    {
        return Err(ContractError::AncestryIncomplete);
    }

    let from_head = reachable_ancestors(checkout_head, &by_oid);
    let from_accepted = reachable_ancestors(accepted_commit, &by_oid);
    if !from_head.contains(merge_base) || !from_accepted.contains(merge_base) {
        return Err(ContractError::InvalidMergeBase);
    }
    let union = from_head
        .union(&from_accepted)
        .copied()
        .collect::<BTreeSet<_>>();
    if union.len() != nodes.len() {
        return Err(ContractError::AncestryUnreachable);
    }
    validate_acyclic(nodes, &by_oid)?;
    let common = from_head
        .intersection(&from_accepted)
        .copied()
        .collect::<BTreeSet<_>>();
    let best = best_common_ancestors(&by_oid, &common);
    if best.len() != 1 || !best.contains(merge_base) {
        return Err(ContractError::InvalidMergeBase);
    }
    if ancestry_sha256(object_format, nodes) != descriptor.ancestry_sha256 {
        return Err(ContractError::AncestryCommitmentMismatch);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn validate_provisional_workspace(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
    ancestry: &[AncestryCommitV1],
    baseline_knowledge: &[SourceFileManifestEntryV1],
    baseline_gaps: &[SourceFileManifestEntryV1],
    baseline_graphs: &[SourceFileManifestEntryV1],
    baseline_evidence: &[SourceFileManifestEntryV1],
    working_knowledge: &[SourceFileManifestEntryV1],
    working_gaps: &[SourceFileManifestEntryV1],
    working_graphs: &[SourceFileManifestEntryV1],
    working_evidence: &[SourceFileManifestEntryV1],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    descriptor.validate_header(limits)?;
    validate_ancestry(
        &descriptor.ancestry,
        ancestry,
        descriptor.object_format,
        &descriptor.checkout_head,
        &descriptor.accepted_commit,
        &descriptor.merge_base,
        limits,
    )?;
    for (lane, manifest, entries) in [
        (
            SourceLaneV1::Knowledge,
            &descriptor.baseline_knowledge,
            baseline_knowledge,
        ),
        (SourceLaneV1::Gaps, &descriptor.baseline_gaps, baseline_gaps),
        (
            SourceLaneV1::Graphs,
            &descriptor.baseline_graphs,
            baseline_graphs,
        ),
        (
            SourceLaneV1::Evidence,
            &descriptor.baseline_evidence,
            baseline_evidence,
        ),
        (
            SourceLaneV1::Knowledge,
            &descriptor.working_knowledge,
            working_knowledge,
        ),
        (SourceLaneV1::Gaps, &descriptor.working_gaps, working_gaps),
        (
            SourceLaneV1::Graphs,
            &descriptor.working_graphs,
            working_graphs,
        ),
        (
            SourceLaneV1::Evidence,
            &descriptor.working_evidence,
            working_evidence,
        ),
    ] {
        validate_source_manifest(&descriptor.scope, lane, manifest, entries, limits)?;
    }
    validate_graph_source_manifest(&descriptor.scope, baseline_graphs, limits)?;
    validate_graph_source_manifest(&descriptor.scope, working_graphs, limits)?;
    validate_evidence_source_manifest(baseline_evidence)?;
    validate_evidence_source_manifest(working_evidence)?;
    Ok(())
}

pub fn publication_candidate_generation_id(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
) -> Result<String, ContractError> {
    validate_producer_id(producer_id)?;
    descriptor.validate_header(KnowledgeSourceLimits::default())?;
    Ok(publication_generation_id_for(
        producer_id,
        descriptor,
        LaneVintage::Current,
    ))
}

/// The publication generation identity exactly as it was minted before the
/// evidence lane existed: the evidence descriptor contributed nothing.
///
/// A re-derivation of an identity already durable on disk, so it does not
/// re-validate the header; callers validate the descriptor first and use this
/// only to recognize stored state. Never mint with it.
pub fn pre_evidence_publication_candidate_generation_id(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
) -> String {
    publication_generation_id_for(producer_id, descriptor, LaneVintage::PreEvidence)
}

/// The publication generation identity exactly as it was minted before the
/// graphs lane existed: neither the graph nor the evidence descriptor
/// contributed. Same re-derivation contract as the pre-evidence variant.
pub fn legacy_publication_candidate_generation_id(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
) -> String {
    publication_generation_id_for(producer_id, descriptor, LaneVintage::PreGraphs)
}

fn publication_generation_id_for(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
    vintage: LaneVintage,
) -> String {
    let mut encoded = Vec::new();
    push_field(
        &mut encoded,
        b"bbox-knowledge-publication-source-generation-v1",
    );
    push_field(&mut encoded, producer_id.as_bytes());
    hash_scope(&mut encoded, &descriptor.scope);
    push_field(&mut encoded, descriptor.full_ref.as_bytes());
    push_field(&mut encoded, descriptor.publisher_commit.as_bytes());
    encoded.push(descriptor.object_format.tag());
    hash_manifest_descriptor(&mut encoded, &descriptor.knowledge);
    hash_manifest_descriptor(&mut encoded, &descriptor.gaps);
    if vintage.includes_graphs() {
        hash_manifest_descriptor(&mut encoded, &descriptor.graphs);
    }
    if vintage.includes_evidence() {
        hash_manifest_descriptor(&mut encoded, &descriptor.evidence);
    }
    format!("kps_{}", sha256(&encoded))
}

/// Whether `stored_id` is an identity this descriptor can legitimately carry.
///
/// The current identity always matches. Older rungs are admissible only for
/// the exact absent-lane shape a binary of that vintage could have produced:
/// a candidate carrying evidence content has no pre-evidence vintage, and one
/// carrying graph content has no pre-graphs vintage.
pub fn publication_generation_id_matches(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
    stored_id: &str,
) -> Result<bool, ContractError> {
    if publication_candidate_generation_id(producer_id, descriptor)? == stored_id {
        return Ok(true);
    }
    Ok(admissible_publication_vintages(descriptor)
        .into_iter()
        .any(|vintage| {
            publication_generation_id_for(producer_id, descriptor, vintage) == stored_id
        }))
}

/// Older vintages this candidate's lane shape could have been minted by.
fn admissible_publication_vintages(
    descriptor: &PublicationCandidateDescriptorV1,
) -> Vec<LaneVintage> {
    let mut vintages = Vec::new();
    if descriptor.evidence.is_absent_lane() {
        vintages.push(LaneVintage::PreEvidence);
        if descriptor.graphs.is_absent_lane() {
            vintages.push(LaneVintage::PreGraphs);
        }
    }
    vintages
}

pub fn validate_publication_generation_id(value: &str) -> Result<(), ContractError> {
    validate_prefixed_generation_id(value, "kps_")
}

pub fn provisional_workspace_generation_id(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> Result<String, ContractError> {
    descriptor.validate_header(KnowledgeSourceLimits::default())?;
    Ok(provisional_generation_id_for(
        descriptor,
        LaneVintage::Current,
    ))
}

/// The provisional generation identity exactly as it was minted before the
/// evidence lane existed. A re-derivation for recognizing stored state, never
/// a minting path.
pub fn pre_evidence_provisional_workspace_generation_id(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> String {
    provisional_generation_id_for(descriptor, LaneVintage::PreEvidence)
}

/// The provisional generation identity exactly as it was minted before the
/// graphs lane existed. Same contract as the pre-evidence variant.
pub fn legacy_provisional_workspace_generation_id(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> String {
    provisional_generation_id_for(descriptor, LaneVintage::PreGraphs)
}

fn provisional_generation_id_for(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
    vintage: LaneVintage,
) -> String {
    let mut encoded = Vec::new();
    push_field(
        &mut encoded,
        b"bbox-knowledge-provisional-source-generation-v1",
    );
    hash_scope(&mut encoded, &descriptor.scope);
    push_field(&mut encoded, descriptor.workspace_id.as_str().as_bytes());
    encoded.extend_from_slice(&descriptor.sequence.to_be_bytes());
    push_field(&mut encoded, descriptor.accepted_generation.as_bytes());
    push_field(&mut encoded, descriptor.accepted_commit.as_bytes());
    push_field(&mut encoded, descriptor.checkout_head.as_bytes());
    push_field(&mut encoded, descriptor.merge_base.as_bytes());
    encoded.push(descriptor.object_format.tag());
    hash_ancestry_descriptor(&mut encoded, &descriptor.ancestry);
    encoded.push(u8::from(descriptor.capture.transaction_pending_before));
    encoded.push(u8::from(descriptor.capture.transaction_pending_after));
    push_field(
        &mut encoded,
        descriptor.capture.first_working_pair_sha256.as_bytes(),
    );
    push_field(
        &mut encoded,
        descriptor.capture.second_working_pair_sha256.as_bytes(),
    );
    // Baseline lanes hash first, then working lanes, each in lane order. Older
    // vintages omit the lanes they never had, which preserves the exact
    // interleaving their binaries produced.
    hash_manifest_descriptor(&mut encoded, &descriptor.baseline_knowledge);
    hash_manifest_descriptor(&mut encoded, &descriptor.baseline_gaps);
    if vintage.includes_graphs() {
        hash_manifest_descriptor(&mut encoded, &descriptor.baseline_graphs);
    }
    if vintage.includes_evidence() {
        hash_manifest_descriptor(&mut encoded, &descriptor.baseline_evidence);
    }
    hash_manifest_descriptor(&mut encoded, &descriptor.working_knowledge);
    hash_manifest_descriptor(&mut encoded, &descriptor.working_gaps);
    if vintage.includes_graphs() {
        hash_manifest_descriptor(&mut encoded, &descriptor.working_graphs);
    }
    if vintage.includes_evidence() {
        hash_manifest_descriptor(&mut encoded, &descriptor.working_evidence);
    }
    format!("kws_{}", sha256(&encoded))
}

/// Whether `stored_id` is an identity this workspace descriptor can carry.
///
/// An older rung is admissible only when BOTH that vintage's lanes are absent,
/// which is the only shape a capture of that vintage can present.
pub fn provisional_generation_id_matches(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
    stored_id: &str,
) -> Result<bool, ContractError> {
    if provisional_workspace_generation_id(descriptor)? == stored_id {
        return Ok(true);
    }
    Ok(admissible_provisional_vintages(descriptor)
        .into_iter()
        .any(|vintage| provisional_generation_id_for(descriptor, vintage) == stored_id))
}

fn admissible_provisional_vintages(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> Vec<LaneVintage> {
    let mut vintages = Vec::new();
    if descriptor.baseline_evidence.is_absent_lane() && descriptor.working_evidence.is_absent_lane()
    {
        vintages.push(LaneVintage::PreEvidence);
        if descriptor.baseline_graphs.is_absent_lane() && descriptor.working_graphs.is_absent_lane()
        {
            vintages.push(LaneVintage::PreGraphs);
        }
    }
    vintages
}

pub fn validate_provisional_generation_id(value: &str) -> Result<(), ContractError> {
    validate_prefixed_generation_id(value, "kws_")
}

fn validate_schema(schema_version: u32) -> Result<(), ContractError> {
    if schema_version != SCHEMA_VERSION {
        return Err(ContractError::UnsupportedSchema(schema_version));
    }
    Ok(())
}

fn validate_prefixed_generation_id(value: &str, prefix: &str) -> Result<(), ContractError> {
    let Some(digest) = value.strip_prefix(prefix) else {
        return Err(ContractError::InvalidDigest);
    };
    validate_sha256(digest)
}

fn validate_scope(scope: &PublishedScope) -> Result<(), ContractError> {
    scope.validate().map_err(|_| ContractError::InvalidScope)
}

fn validate_limit(value: u64, ceiling: u64) -> Result<(), ContractError> {
    if value == 0 || value > ceiling {
        return Err(ContractError::InvalidLimit);
    }
    Ok(())
}

fn validate_total_bytes<const N: usize>(
    values: [u64; N],
    maximum: u64,
) -> Result<(), ContractError> {
    let total = values
        .into_iter()
        .try_fold(0_u64, |total, value| total.checked_add(value))
        .ok_or(ContractError::GenerationLimitExceeded)?;
    if total > maximum {
        return Err(ContractError::GenerationLimitExceeded);
    }
    Ok(())
}

fn validate_object_id(value: &str, format: GitObjectFormatV1) -> Result<(), ContractError> {
    if value.len() != format.oid_len() || !is_lower_hex(value) {
        return Err(ContractError::InvalidObjectId);
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), ContractError> {
    if value.len() != 64 || !is_lower_hex(value) {
        return Err(ContractError::InvalidDigest);
    }
    Ok(())
}

fn validate_producer_id(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ContractError::InvalidProducerId);
    }
    Ok(())
}

fn validate_full_ref(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > MAX_FULL_REF_BYTES
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control() || byte == b' ')
        || value.contains('\\')
        || value.contains("..")
        || value.contains("@{")
        || value
            .bytes()
            .any(|byte| matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'['))
        || value.ends_with('/')
        || value.ends_with('.')
    {
        return Err(ContractError::InvalidFullRef);
    }
    let Some(branch) = value.strip_prefix("refs/heads/") else {
        return Err(ContractError::InvalidFullRef);
    };
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.split('/').any(|component| {
            component.is_empty()
                || component.starts_with('.')
                || component.ends_with('.')
                || component.ends_with(".lock")
        })
    {
        return Err(ContractError::InvalidFullRef);
    }
    Ok(())
}

fn validate_source_filename(
    scope: &PublishedScope,
    lane: SourceLaneV1,
    value: &str,
) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > MAX_REPOSITORY_RELATIVE_FILENAME_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || value
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(ContractError::InvalidSourceFilename);
    }
    let prefix = if scope.bbox_root_relpath() == "." {
        format!(".bbox/{}/", lane.directory())
    } else {
        format!("{}/.bbox/{}/", scope.bbox_root_relpath(), lane.directory())
    };
    let Some(filename) = value.strip_prefix(&prefix) else {
        return Err(ContractError::InvalidSourceFilename);
    };
    if lane == SourceLaneV1::Graphs {
        let mut components = filename.split('/');
        let Some(graph_id) = components.next() else {
            return Err(ContractError::InvalidGraphSourcePath);
        };
        let Some(graph_file) = components.next() else {
            return Err(ContractError::InvalidGraphSourcePath);
        };
        if components.next().is_some()
            || graph_id.is_empty()
            || graph_id.len() > 128
            || graph_id.contains(':')
            || !graph_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            || !matches!(
                graph_file,
                "graph.json" | "schema.json" | "vertices.jsonl" | "edges.jsonl"
            )
        {
            return Err(ContractError::InvalidGraphSourcePath);
        }
        return Ok(());
    }
    if lane == SourceLaneV1::Evidence {
        // The evidence lane is a single flat document, not a tree. Admitting
        // only this exact name is what makes "one complete valid document
        // replaces the accepted set" a transport-level guarantee rather than
        // a convention the reader has to re-derive.
        if filename != EVIDENCE_BINDINGS_FILENAME {
            return Err(ContractError::InvalidEvidenceSourcePath);
        }
        return Ok(());
    }
    let Some(record_id) = filename.strip_suffix(".json") else {
        return Err(ContractError::InvalidSourceFilename);
    };
    if record_id.is_empty()
        || record_id.contains('/')
        || matches!(record_id, "." | "..")
        || !record_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ContractError::InvalidSourceFilename);
    }
    Ok(())
}

fn validate_graph_source_manifest(
    scope: &PublishedScope,
    entries: &[SourceFileManifestEntryV1],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    let prefix = if scope.bbox_root_relpath() == "." {
        ".bbox/graphs/".to_string()
    } else {
        format!("{}/.bbox/graphs/", scope.bbox_root_relpath())
    };
    let mut graphs = BTreeMap::<&str, (u64, BTreeSet<&str>)>::new();
    for entry in entries {
        let relative = entry
            .repository_relative_filename
            .strip_prefix(&prefix)
            .ok_or(ContractError::InvalidGraphSourcePath)?;
        let (graph_id, graph_file) = relative
            .split_once('/')
            .ok_or(ContractError::InvalidGraphSourcePath)?;
        let graph = graphs
            .entry(graph_id)
            .or_insert_with(|| (0, BTreeSet::new()));
        graph.0 = graph
            .0
            .checked_add(entry.encoded_bytes)
            .ok_or(ContractError::GraphLimitExceeded)?;
        if graph.0 > limits.max_graph_bytes {
            return Err(ContractError::GraphLimitExceeded);
        }
        if !graph.1.insert(graph_file) {
            return Err(ContractError::IncompleteGraphSource);
        }
    }
    if graphs.len() as u64 > limits.max_graphs_per_lane {
        return Err(ContractError::GraphLimitExceeded);
    }
    let required = BTreeSet::from(["schema.json", "vertices.jsonl", "edges.jsonl"]);
    let allowed = BTreeSet::from(["graph.json", "schema.json", "vertices.jsonl", "edges.jsonl"]);
    if graphs
        .values()
        .any(|(_, files)| !required.is_subset(files) || !files.is_subset(&allowed))
    {
        return Err(ContractError::IncompleteGraphSource);
    }
    Ok(())
}

/// The evidence lane admits at most one file. `validate_source_filename`
/// already pins the name; this pins the count, so a manifest can never smuggle
/// a second accepted binding set past the replacement rule.
fn validate_evidence_source_manifest(
    entries: &[SourceFileManifestEntryV1],
) -> Result<(), ContractError> {
    if entries.len() > 1 {
        return Err(ContractError::InvalidEvidenceSourcePath);
    }
    Ok(())
}

fn graph_source_may_be_empty(path: &str) -> bool {
    (path.contains("/.bbox/graphs/") || path.starts_with(".bbox/graphs/"))
        && (path.ends_with("/vertices.jsonl") || path.ends_with("/edges.jsonl"))
}

fn validate_ancestry_descriptor(
    descriptor: &AncestryDescriptorV1,
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    limits.validate()?;
    validate_sha256(&descriptor.ancestry_sha256)?;
    if descriptor.node_count == 0
        || descriptor.node_count > limits.max_ancestry_nodes
        || descriptor.edge_count > limits.max_ancestry_edges
        || descriptor.page_count == 0
        || descriptor.page_count > descriptor.node_count
        || descriptor.page_count > limits.max_manifest_pages
        || descriptor.page_count
            < descriptor
                .node_count
                .div_ceil(limits.max_ancestry_page_nodes)
    {
        return Err(ContractError::AncestryLimitExceeded);
    }
    Ok(())
}

fn reachable_ancestors<'a>(
    start: &'a str,
    by_oid: &BTreeMap<&'a str, &'a AncestryCommitV1>,
) -> BTreeSet<&'a str> {
    let mut reachable = BTreeSet::new();
    let mut pending = vec![start];
    while let Some(oid) = pending.pop() {
        if !reachable.insert(oid) {
            continue;
        }
        pending.extend(by_oid[oid].parent_oids.iter().map(String::as_str));
    }
    reachable
}

/// Return the common ancestors with no strictly newer common descendant.
/// The graph is already closure-checked and acyclic. Processing children
/// before parents marks every older common ancestor in one linear pass.
fn best_common_ancestors<'a>(
    by_oid: &BTreeMap<&'a str, &'a AncestryCommitV1>,
    common: &BTreeSet<&'a str>,
) -> BTreeSet<&'a str> {
    let mut remaining_children = by_oid
        .keys()
        .map(|oid| (*oid, 0_usize))
        .collect::<BTreeMap<_, _>>();
    for node in by_oid.values() {
        for parent in &node.parent_oids {
            *remaining_children
                .get_mut(parent.as_str())
                .expect("ancestry closure checked before merge-base computation") += 1;
        }
    }
    let mut ready = remaining_children
        .iter()
        .filter_map(|(oid, children)| (*children == 0).then_some(*oid))
        .collect::<Vec<_>>();
    let mut has_common_descendant = BTreeSet::new();
    while let Some(oid) = ready.pop() {
        let carries_common = common.contains(oid) || has_common_descendant.contains(oid);
        for parent in &by_oid[oid].parent_oids {
            let parent = parent.as_str();
            if carries_common {
                has_common_descendant.insert(parent);
            }
            let children = remaining_children
                .get_mut(parent)
                .expect("ancestry closure checked before merge-base computation");
            *children -= 1;
            if *children == 0 {
                ready.push(parent);
            }
        }
    }
    common
        .iter()
        .filter(|oid| !has_common_descendant.contains(**oid))
        .copied()
        .collect()
}

fn validate_acyclic<'a>(
    nodes: &'a [AncestryCommitV1],
    by_oid: &BTreeMap<&'a str, &'a AncestryCommitV1>,
) -> Result<(), ContractError> {
    let mut dependencies = nodes
        .iter()
        .map(|node| (node.commit_oid.as_str(), node.parent_oids.len()))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<&str, Vec<&str>>::new();
    for node in nodes {
        for parent in &node.parent_oids {
            children
                .entry(parent.as_str())
                .or_default()
                .push(node.commit_oid.as_str());
        }
    }
    let mut ready = dependencies
        .iter()
        .filter_map(|(oid, count)| (*count == 0).then_some(*oid))
        .collect::<Vec<_>>();
    let mut processed = 0_usize;
    while let Some(oid) = ready.pop() {
        processed += 1;
        for child in children.get(oid).into_iter().flatten() {
            let count = dependencies
                .get_mut(child)
                .expect("ancestry closure checked before cycle detection");
            *count -= 1;
            if *count == 0 {
                ready.push(child);
            }
        }
    }
    if processed != by_oid.len() {
        return Err(ContractError::AncestryCycle);
    }
    Ok(())
}

fn hash_scope(encoded: &mut Vec<u8>, scope: &PublishedScope) {
    push_field(encoded, scope.repo_id().as_bytes());
    push_field(encoded, scope.bbox_root_relpath().as_bytes());
}

fn hash_manifest_descriptor(encoded: &mut Vec<u8>, descriptor: &SourceManifestDescriptorV1) {
    push_field(encoded, descriptor.manifest_sha256.as_bytes());
    encoded.extend_from_slice(&descriptor.file_count.to_be_bytes());
    encoded.extend_from_slice(&descriptor.logical_bytes.to_be_bytes());
    encoded.extend_from_slice(&descriptor.page_count.to_be_bytes());
}

fn hash_ancestry_descriptor(encoded: &mut Vec<u8>, descriptor: &AncestryDescriptorV1) {
    push_field(encoded, descriptor.ancestry_sha256.as_bytes());
    encoded.extend_from_slice(&descriptor.node_count.to_be_bytes());
    encoded.extend_from_slice(&descriptor.edge_count.to_be_bytes());
    encoded.extend_from_slice(&descriptor.page_count.to_be_bytes());
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn push_field(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    type LimitMutation = Box<dyn Fn(&mut KnowledgeSourceLimits)>;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-family", ".").unwrap()
    }

    fn entry(path: &str, bytes: &[u8]) -> SourceFileManifestEntryV1 {
        SourceFileManifestEntryV1 {
            repository_relative_filename: path.to_string(),
            encoded_bytes: bytes.len() as u64,
            content_sha256: source_file_blob_sha256(bytes),
        }
    }

    fn manifest(
        lane: SourceLaneV1,
        entries: &[SourceFileManifestEntryV1],
    ) -> SourceManifestDescriptorV1 {
        SourceManifestDescriptorV1 {
            manifest_sha256: source_manifest_sha256(lane, entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.encoded_bytes).sum(),
            page_count: (!entries.is_empty()) as u64,
        }
    }

    fn graph_entries(graph_id: &str) -> Vec<SourceFileManifestEntryV1> {
        let mut entries = [
            ("schema.json", br#"{"version":1}"#.as_slice()),
            ("vertices.jsonl", br#"{"id":"one"}"#.as_slice()),
            ("edges.jsonl", br#"{"from":"one"}"#.as_slice()),
        ]
        .into_iter()
        .map(|(filename, bytes)| entry(&format!(".bbox/graphs/{graph_id}/{filename}"), bytes))
        .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.repository_relative_filename
                .cmp(&right.repository_relative_filename)
        });
        entries
    }

    fn publication() -> (
        PublicationCandidateDescriptorV1,
        Vec<SourceFileManifestEntryV1>,
        Vec<SourceFileManifestEntryV1>,
    ) {
        let knowledge = vec![entry(
            ".bbox/knowledge/knowledge-1.json",
            br#"{"id":"knowledge-1"}"#,
        )];
        let gaps = vec![entry(
            ".bbox/gaps/gap-11111111.json",
            br#"{"id":"gap-11111111"}"#,
        )];
        let descriptor = PublicationCandidateDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            full_ref: "refs/heads/main".to_string(),
            publisher_commit: "1".repeat(40),
            object_format: GitObjectFormatV1::Sha1,
            knowledge: manifest(SourceLaneV1::Knowledge, &knowledge),
            gaps: manifest(SourceLaneV1::Gaps, &gaps),
            graphs: SourceManifestDescriptorV1::default(),
            evidence: SourceManifestDescriptorV1::default(),
        };
        (descriptor, knowledge, gaps)
    }

    fn evidence_entries() -> Vec<SourceFileManifestEntryV1> {
        vec![entry(
            ".bbox/evidence/bindings.json",
            br#"{"version":1,"bindings":[]}"#,
        )]
    }

    fn ancestry(format: GitObjectFormatV1) -> (AncestryDescriptorV1, Vec<AncestryCommitV1>) {
        let oid_len = format.oid_len();
        let root = "1".repeat(oid_len);
        let accepted = "2".repeat(oid_len);
        let head = "3".repeat(oid_len);
        let nodes = vec![
            AncestryCommitV1 {
                commit_oid: root.clone(),
                parent_oids: Vec::new(),
            },
            AncestryCommitV1 {
                commit_oid: accepted,
                parent_oids: vec![root.clone()],
            },
            AncestryCommitV1 {
                commit_oid: head,
                parent_oids: vec![root],
            },
        ];
        let descriptor = AncestryDescriptorV1 {
            ancestry_sha256: ancestry_sha256(format, &nodes),
            node_count: nodes.len() as u64,
            edge_count: 2,
            page_count: 1,
        };
        (descriptor, nodes)
    }

    fn provisional() -> (
        ProvisionalWorkspaceDescriptorV1,
        Vec<AncestryCommitV1>,
        Vec<SourceFileManifestEntryV1>,
        Vec<SourceFileManifestEntryV1>,
    ) {
        let knowledge = vec![entry(
            ".bbox/knowledge/knowledge-1.json",
            br#"{"id":"knowledge-1"}"#,
        )];
        let gaps = vec![entry(
            ".bbox/gaps/gap-11111111.json",
            br#"{"id":"gap-11111111"}"#,
        )];
        let (ancestry, nodes) = ancestry(GitObjectFormatV1::Sha1);
        let baseline_knowledge = manifest(SourceLaneV1::Knowledge, &knowledge);
        let baseline_gaps = manifest(SourceLaneV1::Gaps, &gaps);
        let working_knowledge = manifest(SourceLaneV1::Knowledge, &knowledge);
        let working_gaps = manifest(SourceLaneV1::Gaps, &gaps);
        let empty_graphs = SourceManifestDescriptorV1::default();
        let empty_evidence = SourceManifestDescriptorV1::default();
        let working_pair = working_pair_sha256(
            &working_knowledge,
            &working_gaps,
            &empty_graphs,
            &empty_evidence,
        );
        let descriptor = ProvisionalWorkspaceDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            workspace_id: WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            sequence: 7,
            accepted_generation: "a".repeat(64),
            accepted_commit: "2".repeat(40),
            checkout_head: "3".repeat(40),
            merge_base: "1".repeat(40),
            object_format: GitObjectFormatV1::Sha1,
            ancestry,
            capture: StableCaptureV1 {
                transaction_pending_before: false,
                transaction_pending_after: false,
                first_working_pair_sha256: working_pair.clone(),
                second_working_pair_sha256: working_pair,
            },
            baseline_knowledge,
            baseline_gaps,
            baseline_graphs: empty_graphs.clone(),
            baseline_evidence: empty_evidence.clone(),
            working_knowledge,
            working_gaps,
            working_graphs: empty_graphs,
            working_evidence: empty_evidence,
        };
        (descriptor, nodes, knowledge, gaps)
    }

    /// The manifest digest is lane-local and does not move when a lane is
    /// added, so it stays a hard golden. The generation identity DOES move,
    /// so its golden lives on the ladder rung that minted it: the literal
    /// below is the value this crate asserted for these exact fixtures while
    /// graphs was the newest lane, and it is now the pre-evidence rung.
    #[test]
    fn publication_manifest_and_generation_have_golden_commitments() {
        let (descriptor, knowledge, gaps) = publication();
        validate_publication_candidate(
            &descriptor,
            &knowledge,
            &gaps,
            &[],
            &[],
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            descriptor.knowledge.manifest_sha256,
            "bc51a2b293789c1a7a94ad7315de7fbe26119a1a99d8c8be4fb2b54f86c5bc3d"
        );
        assert_eq!(
            pre_evidence_publication_candidate_generation_id("producer-a", &descriptor),
            "kps_262e2277635441315066c6b9cb60a6243a14833da7f6c7729b518acecd5f6e71"
        );
        assert_eq!(
            legacy_publication_candidate_generation_id("producer-a", &descriptor),
            "kps_ed7b6e9e7378e05714065e1bac8f6ac8c12e47d2b758c5eb6a41edfd62a5df56"
        );
        // The current rung hashes one more descriptor, so it must differ from
        // both older rungs while all three stay admissible for this shape.
        let current = publication_candidate_generation_id("producer-a", &descriptor).unwrap();
        assert_ne!(
            current,
            pre_evidence_publication_candidate_generation_id("producer-a", &descriptor)
        );
        assert_ne!(
            current,
            legacy_publication_candidate_generation_id("producer-a", &descriptor)
        );
        for stored in [
            current,
            pre_evidence_publication_candidate_generation_id("producer-a", &descriptor),
            legacy_publication_candidate_generation_id("producer-a", &descriptor),
        ] {
            assert!(
                publication_generation_id_matches("producer-a", &descriptor, &stored).unwrap(),
                "an empty-lane candidate must admit every vintage that could have minted it"
            );
        }
    }

    #[test]
    fn graph_lane_admits_only_complete_known_three_file_directories() {
        let entries = graph_entries("records");
        let descriptor = manifest(SourceLaneV1::Graphs, &entries);
        validate_source_manifest(
            &scope(),
            SourceLaneV1::Graphs,
            &descriptor,
            &entries,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        validate_graph_source_manifest(&scope(), &entries, KnowledgeSourceLimits::default())
            .unwrap();

        let mut with_descriptor = entries.clone();
        with_descriptor.push(entry(
            ".bbox/graphs/records/graph.json",
            br#"{"graph_id":"records"}"#,
        ));
        with_descriptor.sort_by(|left, right| {
            left.repository_relative_filename
                .cmp(&right.repository_relative_filename)
        });
        validate_source_manifest(
            &scope(),
            SourceLaneV1::Graphs,
            &manifest(SourceLaneV1::Graphs, &with_descriptor),
            &with_descriptor,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        validate_graph_source_manifest(
            &scope(),
            &with_descriptor,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();

        for path in [
            ".bbox/graphs/records/unknown.json",
            ".bbox/graphs/records/nested/schema.json",
            ".bbox/graphs/../schema.json",
            ".bbox/graphs/bad:id/schema.json",
        ] {
            let invalid = vec![entry(path, b"{}")];
            assert!(matches!(
                validate_source_manifest(
                    &scope(),
                    SourceLaneV1::Graphs,
                    &manifest(SourceLaneV1::Graphs, &invalid),
                    &invalid,
                    KnowledgeSourceLimits::default(),
                ),
                Err(ContractError::InvalidGraphSourcePath | ContractError::InvalidSourceFilename)
            ));
        }

        let incomplete = entries[..2].to_vec();
        assert!(matches!(
            validate_graph_source_manifest(&scope(), &incomplete, KnowledgeSourceLimits::default()),
            Err(ContractError::IncompleteGraphSource)
        ));
    }

    #[test]
    fn graph_lane_enforces_per_graph_and_graph_count_ceilings() {
        let entries = graph_entries("records");
        let mut limits = KnowledgeSourceLimits::default();
        limits.max_graph_bytes = entries.iter().map(|entry| entry.encoded_bytes).sum::<u64>() - 1;
        assert!(matches!(
            validate_graph_source_manifest(&scope(), &entries, limits),
            Err(ContractError::GraphLimitExceeded)
        ));

        let mut limits = KnowledgeSourceLimits::default();
        limits.max_graphs_per_lane = 1;
        let mut two_graphs = entries;
        two_graphs.extend(graph_entries("second"));
        two_graphs.sort_by(|left, right| {
            left.repository_relative_filename
                .cmp(&right.repository_relative_filename)
        });
        assert!(matches!(
            validate_graph_source_manifest(&scope(), &two_graphs, limits),
            Err(ContractError::GraphLimitExceeded)
        ));
    }

    /// Same split as the publication golden. The ancestry digest is lane-free
    /// and stays a hard golden. The generation identity hashes the working
    /// pair, which the evidence lane widened, so reproducing the pre-evidence
    /// identity means restoring the pre-evidence capture commitment first -
    /// exactly what a stored pre-evidence workspace carries on disk.
    #[test]
    fn provisional_source_and_generation_have_golden_commitments() {
        let (descriptor, nodes, knowledge, gaps) = provisional();
        validate_provisional_workspace(
            &descriptor,
            &nodes,
            &knowledge,
            &gaps,
            &[],
            &[],
            &knowledge,
            &gaps,
            &[],
            &[],
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            descriptor.ancestry.ancestry_sha256,
            "ed5a0a409a0be0e8ffd40e95bd6bea7ef83c5732fd0b5f403098ed3eddb8e408"
        );

        let mut pre_evidence = descriptor.clone();
        let pair = pre_evidence_working_pair_sha256(
            &pre_evidence.working_knowledge,
            &pre_evidence.working_gaps,
            &pre_evidence.working_graphs,
        );
        pre_evidence.capture.first_working_pair_sha256 = pair.clone();
        pre_evidence.capture.second_working_pair_sha256 = pair;
        assert_eq!(
            pre_evidence_provisional_workspace_generation_id(&pre_evidence),
            "kws_1c65ac3aec908e12d6b741901abb05a23a9026cd303887cbdff0d6c759a99672"
        );
        // A stored pre-evidence workspace opens: its capture commitment and
        // its identity are both admissible while the evidence lane is absent.
        pre_evidence
            .validate_header(KnowledgeSourceLimits::default())
            .unwrap();
        assert!(
            provisional_generation_id_matches(
                &pre_evidence,
                &pre_evidence_provisional_workspace_generation_id(&pre_evidence)
            )
            .unwrap()
        );
    }

    /// The pre-evidence rung is admissible only for the shape a pre-evidence
    /// binary could have written. A workspace that actually carries evidence
    /// content has no pre-evidence vintage and is held to the current rung.
    #[test]
    fn pre_evidence_identity_is_admissible_only_without_evidence_content() {
        let (mut descriptor, ..) = provisional();
        let pre_evidence = pre_evidence_provisional_workspace_generation_id(&descriptor);
        let current = provisional_workspace_generation_id(&descriptor).unwrap();
        assert_ne!(pre_evidence, current);
        assert!(provisional_generation_id_matches(&descriptor, &pre_evidence).unwrap());
        assert!(provisional_generation_id_matches(&descriptor, &current).unwrap());

        descriptor.working_evidence = manifest(SourceLaneV1::Evidence, &evidence_entries());
        let pair = working_pair_sha256(
            &descriptor.working_knowledge,
            &descriptor.working_gaps,
            &descriptor.working_graphs,
            &descriptor.working_evidence,
        );
        descriptor.capture.first_working_pair_sha256 = pair.clone();
        descriptor.capture.second_working_pair_sha256 = pair;
        descriptor
            .validate_header(KnowledgeSourceLimits::default())
            .unwrap();
        assert!(!provisional_generation_id_matches(&descriptor, &pre_evidence).unwrap());
        assert!(
            provisional_generation_id_matches(
                &descriptor,
                &provisional_workspace_generation_id(&descriptor).unwrap(),
            )
            .unwrap()
        );
    }

    /// A capture that carries evidence content cannot present a pre-evidence
    /// working-pair commitment. This is the stability guarantee: an
    /// evidence-only edit during capture has to move the pair.
    #[test]
    fn a_pre_evidence_working_pair_stops_being_admissible_with_evidence_content() {
        let (mut descriptor, ..) = provisional();
        let pre_evidence_pair = pre_evidence_working_pair_sha256(
            &descriptor.working_knowledge,
            &descriptor.working_gaps,
            &descriptor.working_graphs,
        );
        descriptor.capture.first_working_pair_sha256 = pre_evidence_pair.clone();
        descriptor.capture.second_working_pair_sha256 = pre_evidence_pair;
        descriptor
            .validate_header(KnowledgeSourceLimits::default())
            .unwrap();

        descriptor.working_evidence = manifest(SourceLaneV1::Evidence, &evidence_entries());
        assert!(matches!(
            descriptor.validate_header(KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidInput)
        ));
    }

    /// The evidence lane is one document at one exact path. Anything else is
    /// refused, so the replacement rule cannot be smuggled past.
    #[test]
    fn evidence_lane_admits_only_the_single_bindings_document() {
        let entries = evidence_entries();
        validate_source_manifest(
            &scope(),
            SourceLaneV1::Evidence,
            &manifest(SourceLaneV1::Evidence, &entries),
            &entries,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        validate_evidence_source_manifest(&entries).unwrap();

        for path in [
            ".bbox/evidence/other.json",
            ".bbox/evidence/nested/bindings.json",
            ".bbox/evidence/bindings.jsonl",
            ".bbox/evidence/../bindings.json",
        ] {
            let invalid = vec![entry(path, b"{}")];
            assert!(
                matches!(
                    validate_source_manifest(
                        &scope(),
                        SourceLaneV1::Evidence,
                        &manifest(SourceLaneV1::Evidence, &invalid),
                        &invalid,
                        KnowledgeSourceLimits::default(),
                    ),
                    Err(ContractError::InvalidEvidenceSourcePath
                        | ContractError::InvalidSourceFilename)
                ),
                "{path} must not be admissible on the evidence lane"
            );
        }

        let mut two = evidence_entries();
        two.push(entry(".bbox/evidence/bindings.json", b"{}"));
        assert!(matches!(
            validate_evidence_source_manifest(&two),
            Err(ContractError::InvalidEvidenceSourcePath)
        ));
    }

    /// The evidence lane's manifest digest is domain-separated from every
    /// other lane, so the same entry list cannot be replayed across lanes.
    #[test]
    fn the_evidence_lane_tag_separates_its_manifest_digest() {
        let entries = evidence_entries();
        let evidence = source_manifest_sha256(SourceLaneV1::Evidence, &entries);
        for other in [
            SourceLaneV1::Knowledge,
            SourceLaneV1::Gaps,
            SourceLaneV1::Graphs,
        ] {
            assert_ne!(evidence, source_manifest_sha256(other, &entries));
        }
    }

    /// The lane filename this crate admits and the one the graph kernel writes
    /// have to be the same string. The crates cannot share a constant without
    /// making this wire leaf depend on the kernel, so the coupling is pinned
    /// here instead.
    #[test]
    fn the_evidence_lane_filename_is_the_documented_one() {
        assert_eq!(EVIDENCE_BINDINGS_FILENAME, "bindings.json");
    }

    /// Both goldens below are the values the pre-graphs-lane binary minted for
    /// these exact fixtures, taken from the assertions this crate carried
    /// before the graphs lane landed. They pin the legacy derivations to real
    /// on-disk identity rather than to a restatement of the current code.
    #[test]
    fn legacy_derivations_reproduce_pre_graphs_identity() {
        let (publication_descriptor, _, _) = publication();
        assert_eq!(
            legacy_publication_candidate_generation_id("producer-a", &publication_descriptor),
            "kps_ed7b6e9e7378e05714065e1bac8f6ac8c12e47d2b758c5eb6a41edfd62a5df56"
        );

        let (mut descriptor, ..) = provisional();
        let legacy_pair =
            legacy_working_pair_sha256(&descriptor.working_knowledge, &descriptor.working_gaps);
        descriptor.capture.first_working_pair_sha256 = legacy_pair.clone();
        descriptor.capture.second_working_pair_sha256 = legacy_pair;
        assert_eq!(
            legacy_provisional_workspace_generation_id(&descriptor),
            "kws_b2ef389d2c997a9c6aac48af0c7af608412d26f4dabf682e7b293b36e5feb37a"
        );
        // A pre-graphs working-pair commitment stays admissible while the
        // graph lane is absent, and stops being admissible the moment the
        // workspace carries graph content.
        descriptor
            .validate_header(KnowledgeSourceLimits::default())
            .unwrap();
        assert!(
            provisional_generation_id_matches(
                &descriptor,
                &legacy_provisional_workspace_generation_id(&descriptor)
            )
            .unwrap()
        );
        // Content on EITHER newer lane retires the pre-graphs vintage: no
        // binary of that era could have written either one.
        let mut with_graphs = descriptor.clone();
        with_graphs.working_graphs = manifest(SourceLaneV1::Graphs, &graph_entries("records"));
        assert!(matches!(
            with_graphs.validate_header(KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidInput)
        ));
        let mut with_evidence = descriptor;
        with_evidence.working_evidence = manifest(SourceLaneV1::Evidence, &evidence_entries());
        assert!(matches!(
            with_evidence.validate_header(KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidInput)
        ));
    }

    #[test]
    fn legacy_publication_identity_is_admissible_only_without_graph_content() {
        let (mut descriptor, ..) = publication();
        let legacy = legacy_publication_candidate_generation_id("producer-a", &descriptor);
        let current = publication_candidate_generation_id("producer-a", &descriptor).unwrap();
        assert_ne!(legacy, current);
        assert!(publication_generation_id_matches("producer-a", &descriptor, &legacy).unwrap());
        assert!(publication_generation_id_matches("producer-a", &descriptor, &current).unwrap());

        // Evidence content alone retires the pre-graphs rung too, because the
        // ladder guard requires every newer lane to be absent.
        let mut with_evidence = descriptor.clone();
        with_evidence.evidence = manifest(SourceLaneV1::Evidence, &evidence_entries());
        assert!(!publication_generation_id_matches("producer-a", &with_evidence, &legacy).unwrap());

        let graphs = graph_entries("records");
        descriptor.graphs = manifest(SourceLaneV1::Graphs, &graphs);
        assert!(!publication_generation_id_matches("producer-a", &descriptor, &legacy).unwrap());
        assert!(
            publication_generation_id_matches(
                "producer-a",
                &descriptor,
                &publication_candidate_generation_id("producer-a", &descriptor).unwrap(),
            )
            .unwrap()
        );
    }

    #[test]
    fn empty_lanes_are_explicit_and_canonical() {
        let entries = Vec::new();
        let descriptor = manifest(SourceLaneV1::Knowledge, &entries);
        validate_source_manifest(
            &scope(),
            SourceLaneV1::Knowledge,
            &descriptor,
            &entries,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(descriptor.file_count, 0);
        assert_eq!(descriptor.logical_bytes, 0);
        assert_eq!(descriptor.page_count, 0);
    }

    #[test]
    fn manifests_refuse_reordering_duplicates_and_hostile_paths() {
        let valid = entry(
            ".bbox/knowledge/knowledge-1.json",
            br#"{"id":"knowledge-1"}"#,
        );
        for path in [
            ".bbox/gaps/knowledge-1.json",
            ".bbox/knowledge/../escape.json",
            "/.bbox/knowledge/absolute.json",
            ".bbox/knowledge/nested/record.json",
            ".bbox/knowledge/record",
            ".bbox/knowledge/record\\evil.json",
        ] {
            let entries = vec![entry(path, b"{}")];
            let descriptor = manifest(SourceLaneV1::Knowledge, &entries);
            assert_eq!(
                validate_source_manifest(
                    &scope(),
                    SourceLaneV1::Knowledge,
                    &descriptor,
                    &entries,
                    KnowledgeSourceLimits::default()
                ),
                Err(ContractError::InvalidSourceFilename),
                "accepted {path:?}"
            );
        }
        let entries = vec![valid.clone(), valid];
        let descriptor = manifest(SourceLaneV1::Knowledge, &entries);
        assert_eq!(
            validate_source_manifest(
                &scope(),
                SourceLaneV1::Knowledge,
                &descriptor,
                &entries,
                KnowledgeSourceLimits::default()
            ),
            Err(ContractError::ManifestOutOfOrder)
        );
    }

    #[test]
    fn blob_and_manifest_page_validation_fail_closed() {
        let entries = vec![entry(
            ".bbox/knowledge/knowledge-1.json",
            br#"{"id":"knowledge-1"}"#,
        )];
        let descriptor = manifest(SourceLaneV1::Knowledge, &entries);
        validate_source_blob(
            &entries[0],
            br#"{"id":"knowledge-1"}"#,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            validate_source_blob(
                &entries[0],
                br#"{"id":"knowledge-2"}"#,
                KnowledgeSourceLimits::default()
            ),
            Err(ContractError::ManifestCommitmentMismatch)
        );

        let page = SourceManifestPageV1 {
            page_index: 0,
            entries,
        };
        validate_manifest_page(&descriptor, &page, 512, KnowledgeSourceLimits::default()).unwrap();
        let mut invalid = page;
        invalid.page_index = 1;
        assert_eq!(
            validate_manifest_page(&descriptor, &invalid, 512, KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidManifestPage)
        );
    }

    #[test]
    fn limits_accept_the_cap_and_reject_zero_or_cap_plus_one() {
        let caps = KnowledgeSourceLimits::default();
        caps.validate().unwrap();
        let mutations: Vec<LimitMutation> = vec![
            Box::new(|limits| limits.max_files_per_lane = MAX_SOURCE_FILES_PER_LANE + 1),
            Box::new(|limits| limits.max_file_bytes = MAX_SOURCE_FILE_BYTES + 1),
            Box::new(|limits| limits.max_lane_bytes = MAX_SOURCE_LANE_BYTES + 1),
            Box::new(|limits| limits.max_ancestry_nodes = MAX_ANCESTRY_NODES + 1),
            Box::new(|limits| limits.max_ancestry_edges = MAX_ANCESTRY_EDGES + 1),
            Box::new(|limits| limits.max_ancestry_page_nodes = MAX_ANCESTRY_PAGE_NODES + 1),
            Box::new(|limits| limits.max_ancestry_page_bytes = MAX_ANCESTRY_PAGE_BYTES + 1),
            Box::new(|limits| limits.max_manifest_page_entries = MAX_MANIFEST_PAGE_ENTRIES + 1),
            Box::new(|limits| limits.max_manifest_page_bytes = MAX_MANIFEST_PAGE_BYTES + 1),
            Box::new(|limits| limits.max_manifest_pages = MAX_MANIFEST_PAGES + 1),
            Box::new(|limits| limits.max_open_uploads = MAX_OPEN_UPLOADS + 1),
            Box::new(|limits| limits.max_generation_bytes = MAX_GENERATION_BYTES + 1),
        ];
        for mutate in mutations {
            let mut exceeded = caps;
            mutate(&mut exceeded);
            assert_eq!(exceeded.validate(), Err(ContractError::InvalidLimit));
        }
        let mut zero = caps;
        zero.max_file_bytes = 0;
        assert_eq!(zero.validate(), Err(ContractError::InvalidLimit));
    }

    #[test]
    fn ancestry_accepts_sha1_and_sha256_and_rejects_incomplete_graphs() {
        for format in [GitObjectFormatV1::Sha1, GitObjectFormatV1::Sha256] {
            let (descriptor, nodes) = ancestry(format);
            validate_ancestry(
                &descriptor,
                &nodes,
                format,
                &"3".repeat(format.oid_len()),
                &"2".repeat(format.oid_len()),
                &"1".repeat(format.oid_len()),
                KnowledgeSourceLimits::default(),
            )
            .unwrap();
            validate_ancestry_page(
                &descriptor,
                &AncestryPageV1 {
                    page_index: 0,
                    nodes: nodes.clone(),
                },
                1024,
                KnowledgeSourceLimits::default(),
            )
            .unwrap();

            let mut incomplete = nodes.clone();
            incomplete.remove(0);
            let descriptor = AncestryDescriptorV1 {
                ancestry_sha256: ancestry_sha256(format, &incomplete),
                node_count: incomplete.len() as u64,
                edge_count: 2,
                page_count: 1,
            };
            assert_eq!(
                validate_ancestry(
                    &descriptor,
                    &incomplete,
                    format,
                    &"3".repeat(format.oid_len()),
                    &"2".repeat(format.oid_len()),
                    &"1".repeat(format.oid_len()),
                    KnowledgeSourceLimits::default()
                ),
                Err(ContractError::AncestryIncomplete)
            );
        }
    }

    #[test]
    fn ancestry_rejects_multiple_best_merge_bases() {
        let format = GitObjectFormatV1::Sha1;
        let oid = |value: &str| value.repeat(40);
        let nodes = vec![
            AncestryCommitV1 {
                commit_oid: oid("1"),
                parent_oids: vec![],
            },
            AncestryCommitV1 {
                commit_oid: oid("2"),
                parent_oids: vec![oid("1")],
            },
            AncestryCommitV1 {
                commit_oid: oid("3"),
                parent_oids: vec![oid("1")],
            },
            AncestryCommitV1 {
                commit_oid: oid("4"),
                parent_oids: vec![oid("2"), oid("3")],
            },
            AncestryCommitV1 {
                commit_oid: oid("5"),
                parent_oids: vec![oid("2"), oid("3")],
            },
        ];
        let descriptor = AncestryDescriptorV1 {
            ancestry_sha256: ancestry_sha256(format, &nodes),
            node_count: nodes.len() as u64,
            edge_count: 6,
            page_count: 1,
        };
        assert_eq!(
            validate_ancestry(
                &descriptor,
                &nodes,
                format,
                &oid("4"),
                &oid("5"),
                &oid("2"),
                KnowledgeSourceLimits::default(),
            ),
            Err(ContractError::InvalidMergeBase)
        );
    }

    #[test]
    fn ancestry_refuses_cycles_duplicate_parents_and_bad_object_ids() {
        let format = GitObjectFormatV1::Sha1;
        let one = "1".repeat(40);
        let two = "2".repeat(40);
        let cycle = vec![
            AncestryCommitV1 {
                commit_oid: one.clone(),
                parent_oids: vec![two.clone()],
            },
            AncestryCommitV1 {
                commit_oid: two.clone(),
                parent_oids: vec![one.clone()],
            },
        ];
        let descriptor = AncestryDescriptorV1 {
            ancestry_sha256: ancestry_sha256(format, &cycle),
            node_count: 2,
            edge_count: 2,
            page_count: 1,
        };
        assert_eq!(
            validate_ancestry(
                &descriptor,
                &cycle,
                format,
                &two,
                &one,
                &one,
                KnowledgeSourceLimits::default()
            ),
            Err(ContractError::AncestryCycle)
        );

        let duplicate_parent = vec![
            AncestryCommitV1 {
                commit_oid: one.clone(),
                parent_oids: Vec::new(),
            },
            AncestryCommitV1 {
                commit_oid: two.clone(),
                parent_oids: vec![one.clone(), one.clone()],
            },
        ];
        let descriptor = AncestryDescriptorV1 {
            ancestry_sha256: ancestry_sha256(format, &duplicate_parent),
            node_count: 2,
            edge_count: 2,
            page_count: 1,
        };
        assert_eq!(
            validate_ancestry(
                &descriptor,
                &duplicate_parent,
                format,
                &two,
                &one,
                &one,
                KnowledgeSourceLimits::default()
            ),
            Err(ContractError::InvalidAncestryParent)
        );

        let mut bad_oid = duplicate_parent;
        bad_oid[1].commit_oid = "g".repeat(40);
        assert_eq!(
            validate_ancestry(
                &descriptor,
                &bad_oid,
                format,
                &two,
                &one,
                &one,
                KnowledgeSourceLimits::default()
            ),
            Err(ContractError::InvalidObjectId)
        );
    }

    #[test]
    fn generation_ids_bind_authority_and_every_descriptor_class() {
        let (publication, _, _) = publication();
        let base = publication_candidate_generation_id("producer-a", &publication).unwrap();
        assert_ne!(
            base,
            publication_candidate_generation_id("producer-b", &publication).unwrap()
        );
        let mut changed = publication.clone();
        changed.full_ref = "refs/heads/other".to_string();
        assert_ne!(
            base,
            publication_candidate_generation_id("producer-a", &changed).unwrap()
        );
        let mut changed = publication.clone();
        changed.knowledge.manifest_sha256 = "b".repeat(64);
        assert_ne!(
            base,
            publication_candidate_generation_id("producer-a", &changed).unwrap()
        );

        let (provisional, _, _, _) = provisional();
        let base = provisional_workspace_generation_id(&provisional).unwrap();
        let mut changed = provisional.clone();
        changed.sequence += 1;
        assert_ne!(base, provisional_workspace_generation_id(&changed).unwrap());
        let mut changed = provisional.clone();
        changed.ancestry.ancestry_sha256 = "b".repeat(64);
        assert_ne!(base, provisional_workspace_generation_id(&changed).unwrap());
        let mut changed = provisional.clone();
        changed.working_gaps.manifest_sha256 = "b".repeat(64);
        let changed_pair = working_pair_sha256(
            &changed.working_knowledge,
            &changed.working_gaps,
            &changed.working_graphs,
            &changed.working_evidence,
        );
        changed.capture.first_working_pair_sha256 = changed_pair.clone();
        changed.capture.second_working_pair_sha256 = changed_pair;
        assert_ne!(base, provisional_workspace_generation_id(&changed).unwrap());
    }

    #[test]
    fn provisional_capture_requires_a_stable_nontransactional_pair() {
        let (descriptor, _, _, _) = provisional();
        descriptor
            .validate_header(KnowledgeSourceLimits::default())
            .unwrap();

        let mut pending = descriptor.clone();
        pending.capture.transaction_pending_before = true;
        assert_eq!(
            pending.validate_header(KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidInput)
        );

        let mut moved = descriptor;
        moved.capture.second_working_pair_sha256 = "b".repeat(64);
        assert_eq!(
            moved.validate_header(KnowledgeSourceLimits::default()),
            Err(ContractError::InvalidInput)
        );
    }

    #[test]
    fn provisional_capture_context_validates_accepted_identity_and_lease() {
        let mut context = ProvisionalCaptureContextV1 {
            scope: scope(),
            accepted_generation: "a".repeat(64),
            accepted_commit: "1".repeat(40),
            lease_ttl_secs: 60,
        };
        context.validate().unwrap();

        context.lease_ttl_secs = 0;
        assert_eq!(context.validate(), Err(ContractError::InvalidLimit));
        context.lease_ttl_secs = 60;
        context.accepted_commit = "1".repeat(41);
        assert_eq!(context.validate(), Err(ContractError::InvalidObjectId));
        context.accepted_commit = "1".repeat(40);
        context.accepted_generation = "z".repeat(64);
        assert_eq!(context.validate(), Err(ContractError::InvalidDigest));
    }

    #[test]
    fn wire_decode_refuses_unknown_fields_and_noncanonical_workspace_ids() {
        let (descriptor, _, _) = publication();
        let mut value = serde_json::to_value(&descriptor).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PublicationCandidateDescriptorV1>(value).is_err());

        let (descriptor, _, _, _) = provisional();
        let mut value = serde_json::to_value(&descriptor).unwrap();
        value["workspace_id"] = serde_json::json!("0123456789abcdef0123456789abcdeF");
        assert!(serde_json::from_value::<ProvisionalWorkspaceDescriptorV1>(value).is_err());
    }
}
