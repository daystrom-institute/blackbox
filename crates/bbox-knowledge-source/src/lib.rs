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
pub const MAX_ANCESTRY_NODES: u64 = 2_000_000;
pub const MAX_ANCESTRY_EDGES: u64 = 8_000_000;
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
    pub max_ancestry_nodes: u64,
    pub max_ancestry_edges: u64,
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
            max_ancestry_nodes: MAX_ANCESTRY_NODES,
            max_ancestry_edges: MAX_ANCESTRY_EDGES,
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
        validate_limit(self.max_ancestry_nodes, MAX_ANCESTRY_NODES)?;
        validate_limit(self.max_ancestry_edges, MAX_ANCESTRY_EDGES)?;
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceLaneV1 {
    Knowledge,
    Gaps,
}

impl SourceLaneV1 {
    fn directory(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Gaps => "gaps",
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Knowledge => 1,
            Self::Gaps => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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

impl SourceManifestDescriptorV1 {
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
}

impl PublicationCandidateDescriptorV1 {
    pub fn validate_header(&self, limits: KnowledgeSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        validate_scope(&self.scope)?;
        validate_full_ref(&self.full_ref)?;
        validate_object_id(&self.publisher_commit, self.object_format)?;
        self.knowledge.validate_header(limits)?;
        self.gaps.validate_header(limits)?;
        validate_total_bytes(
            [self.knowledge.logical_bytes, self.gaps.logical_bytes],
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
    pub baseline_knowledge: SourceManifestDescriptorV1,
    pub baseline_gaps: SourceManifestDescriptorV1,
    pub working_knowledge: SourceManifestDescriptorV1,
    pub working_gaps: SourceManifestDescriptorV1,
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
        self.baseline_knowledge.validate_header(limits)?;
        self.baseline_gaps.validate_header(limits)?;
        self.working_knowledge.validate_header(limits)?;
        self.working_gaps.validate_header(limits)?;
        validate_total_bytes(
            [
                self.baseline_knowledge.logical_bytes,
                self.baseline_gaps.logical_bytes,
                self.working_knowledge.logical_bytes,
                self.working_gaps.logical_bytes,
            ],
            limits.max_generation_bytes,
        )
    }
}

pub fn source_file_blob_sha256(bytes: &[u8]) -> String {
    sha256(bytes)
}

pub fn validate_source_blob(
    entry: &SourceFileManifestEntryV1,
    bytes: &[u8],
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    limits.validate()?;
    validate_sha256(&entry.content_sha256)?;
    if entry.encoded_bytes == 0
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
        if entry.encoded_bytes == 0 || entry.encoded_bytes > limits.max_file_bytes {
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
    )
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
    working_knowledge: &[SourceFileManifestEntryV1],
    working_gaps: &[SourceFileManifestEntryV1],
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
            SourceLaneV1::Knowledge,
            &descriptor.working_knowledge,
            working_knowledge,
        ),
        (SourceLaneV1::Gaps, &descriptor.working_gaps, working_gaps),
    ] {
        validate_source_manifest(&descriptor.scope, lane, manifest, entries, limits)?;
    }
    Ok(())
}

pub fn publication_candidate_generation_id(
    producer_id: &str,
    descriptor: &PublicationCandidateDescriptorV1,
) -> Result<String, ContractError> {
    validate_producer_id(producer_id)?;
    descriptor.validate_header(KnowledgeSourceLimits::default())?;
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
    Ok(format!("kps_{}", sha256(&encoded)))
}

pub fn provisional_workspace_generation_id(
    descriptor: &ProvisionalWorkspaceDescriptorV1,
) -> Result<String, ContractError> {
    descriptor.validate_header(KnowledgeSourceLimits::default())?;
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
    hash_manifest_descriptor(&mut encoded, &descriptor.baseline_knowledge);
    hash_manifest_descriptor(&mut encoded, &descriptor.baseline_gaps);
    hash_manifest_descriptor(&mut encoded, &descriptor.working_knowledge);
    hash_manifest_descriptor(&mut encoded, &descriptor.working_gaps);
    Ok(format!("kws_{}", sha256(&encoded)))
}

fn validate_schema(schema_version: u32) -> Result<(), ContractError> {
    if schema_version != SCHEMA_VERSION {
        return Err(ContractError::UnsupportedSchema(schema_version));
    }
    Ok(())
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

fn validate_ancestry_descriptor(
    descriptor: &AncestryDescriptorV1,
    limits: KnowledgeSourceLimits,
) -> Result<(), ContractError> {
    limits.validate()?;
    validate_sha256(&descriptor.ancestry_sha256)?;
    if descriptor.node_count == 0
        || descriptor.node_count > limits.max_ancestry_nodes
        || descriptor.edge_count > limits.max_ancestry_edges
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
        };
        (descriptor, knowledge, gaps)
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
            baseline_knowledge: manifest(SourceLaneV1::Knowledge, &knowledge),
            baseline_gaps: manifest(SourceLaneV1::Gaps, &gaps),
            working_knowledge: manifest(SourceLaneV1::Knowledge, &knowledge),
            working_gaps: manifest(SourceLaneV1::Gaps, &gaps),
        };
        (descriptor, nodes, knowledge, gaps)
    }

    #[test]
    fn publication_manifest_and_generation_have_golden_commitments() {
        let (descriptor, knowledge, gaps) = publication();
        validate_publication_candidate(
            &descriptor,
            &knowledge,
            &gaps,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            descriptor.knowledge.manifest_sha256,
            "bc51a2b293789c1a7a94ad7315de7fbe26119a1a99d8c8be4fb2b54f86c5bc3d"
        );
        assert_eq!(
            publication_candidate_generation_id("producer-a", &descriptor).unwrap(),
            "kps_ed7b6e9e7378e05714065e1bac8f6ac8c12e47d2b758c5eb6a41edfd62a5df56"
        );
    }

    #[test]
    fn provisional_source_and_generation_have_golden_commitments() {
        let (descriptor, nodes, knowledge, gaps) = provisional();
        validate_provisional_workspace(
            &descriptor,
            &nodes,
            &knowledge,
            &gaps,
            &knowledge,
            &gaps,
            KnowledgeSourceLimits::default(),
        )
        .unwrap();
        assert_eq!(
            descriptor.ancestry.ancestry_sha256,
            "ed5a0a409a0be0e8ffd40e95bd6bea7ef83c5732fd0b5f403098ed3eddb8e408"
        );
        assert_eq!(
            provisional_workspace_generation_id(&descriptor).unwrap(),
            "kws_f218f735ff68245aefea8848e8785ca28db93f1487aea094445cb6f4e4818370"
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
        let mutations: Vec<Box<dyn Fn(&mut KnowledgeSourceLimits)>> = vec![
            Box::new(|limits| limits.max_files_per_lane = MAX_SOURCE_FILES_PER_LANE + 1),
            Box::new(|limits| limits.max_file_bytes = MAX_SOURCE_FILE_BYTES + 1),
            Box::new(|limits| limits.max_lane_bytes = MAX_SOURCE_LANE_BYTES + 1),
            Box::new(|limits| limits.max_ancestry_nodes = MAX_ANCESTRY_NODES + 1),
            Box::new(|limits| limits.max_ancestry_edges = MAX_ANCESTRY_EDGES + 1),
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

            let mut incomplete = nodes.clone();
            incomplete.remove(0);
            let descriptor = AncestryDescriptorV1 {
                ancestry_sha256: ancestry_sha256(format, &incomplete),
                node_count: incomplete.len() as u64,
                edge_count: 2,
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
        assert_ne!(base, provisional_workspace_generation_id(&changed).unwrap());
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
