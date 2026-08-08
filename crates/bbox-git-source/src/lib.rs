//! Dependency-clean wire contracts for typed Git-history and provenance
//! transport.

use std::collections::{BTreeMap, BTreeSet};

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{CommitNamespace, RepoHistoryId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_HISTORY_MANIFEST_PAGE_ENTRIES: usize = 2_000;
pub const MAX_HISTORY_MANIFEST_PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_HISTORY_RECORD_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_AUTHOR_FIELD_BYTES: usize = 64 * 1024;
pub const MAX_COMMIT_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_RELATIVE_PATH_BYTES: usize = 4096;
pub const MAX_PROVENANCE_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitSourceLimits {
    pub max_history_commits: u64,
    pub max_history_logical_bytes: u64,
    pub max_provenance_documents: u64,
    pub max_provenance_logical_bytes: u64,
}

impl Default for GitSourceLimits {
    fn default() -> Self {
        Self {
            max_history_commits: 2_000_000,
            max_history_logical_bytes: 8 * 1024 * 1024 * 1024,
            max_provenance_documents: 1_000_000,
            max_provenance_logical_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    #[error("unsupported Git-source schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid published scope")]
    InvalidScope,
    #[error("invalid Git object id")]
    InvalidObjectId,
    #[error("mixed Git object formats")]
    ObjectFormatMismatch,
    #[error("invalid sha256 digest")]
    InvalidDigest,
    #[error("invalid repository-relative path")]
    InvalidRelativePath,
    #[error("history manifest is not strictly sorted")]
    HistoryManifestOutOfOrder,
    #[error("history fragments are not contiguous")]
    HistoryFragmentGap,
    #[error("history record does not match its manifest entry")]
    HistoryRecordMismatch,
    #[error("history commit graph is incomplete")]
    HistoryGraphIncomplete,
    #[error("history contains a commit unreachable from HEAD")]
    HistoryUnreachableRecord,
    #[error("history count does not match its descriptor")]
    HistoryCountMismatch,
    #[error("history commitment does not match its descriptor")]
    HistoryCommitmentMismatch,
    #[error("history source exceeds an enforced limit")]
    HistoryLimitExceeded,
    #[error("history record contains an oversized indivisible field")]
    HistoryRecordTooLarge,
    #[error("invalid provenance notes ref")]
    InvalidNotesRef,
    #[error("provenance manifest is not strictly sorted")]
    ProvenanceManifestOutOfOrder,
    #[error("provenance document does not match its manifest entry")]
    ProvenanceDocumentMismatch,
    #[error("provenance document is invalid")]
    ProvenanceDocumentInvalid,
    #[error("provenance count does not match its descriptor")]
    ProvenanceCountMismatch,
    #[error("provenance commitment does not match its descriptor")]
    ProvenanceCommitmentMismatch,
    #[error("provenance source exceeds an enforced limit")]
    ProvenanceLimitExceeded,
    #[error("invalid producer id")]
    InvalidProducerId,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryDescriptorV1 {
    pub schema_version: u32,
    pub scope: PublishedScope,
    pub repo_head: String,
    pub object_format: GitObjectFormatV1,
    pub manifest_sha256: String,
    pub commit_count: u64,
    pub fragment_count: u64,
    pub logical_bytes: u64,
}

impl GitHistoryDescriptorV1 {
    pub fn validate_header(&self, limits: GitSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        self.scope
            .validate()
            .map_err(|_| ContractError::InvalidScope)?;
        validate_object_id(&self.repo_head, self.object_format)?;
        validate_sha256(&self.manifest_sha256)?;
        if self.commit_count == 0
            || self.fragment_count < self.commit_count
            || self.commit_count > limits.max_history_commits
            || self.logical_bytes > limits.max_history_logical_bytes
        {
            return Err(ContractError::HistoryLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryManifestEntryV1 {
    pub commit_oid: String,
    pub fragment_index: u32,
    pub encoded_bytes: u64,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryManifestPageV1 {
    pub entries: Vec<GitHistoryManifestEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryCommitHeaderV1 {
    pub parent_oids: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryCommitFragmentV1 {
    pub commit_oid: String,
    pub fragment_index: u32,
    pub fragment_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<GitHistoryCommitHeaderV1>,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryProbeRequestV1 {
    pub scope: PublishedScope,
    pub repo_head: String,
    pub object_format: GitObjectFormatV1,
}

impl GitHistoryProbeRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.scope
            .validate()
            .map_err(|_| ContractError::InvalidScope)?;
        validate_object_id(&self.repo_head, self.object_format)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHistoryProbeResponseV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<GitHistorySourceStatusV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginGitHistoryUploadRequestV1 {
    pub descriptor: GitHistoryDescriptorV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BeginGitHistoryUploadResponseV1 {
    pub upload_id: String,
    pub max_page_entries: usize,
    pub max_page_bytes: usize,
    pub max_record_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissingHistoryRecordsPageV1 {
    pub source_generation_id: String,
    pub hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalizeGitHistoryUploadResponseV1 {
    pub source_generation_id: String,
    pub status_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitHistorySourceStateV1 {
    ReceivingManifest,
    MissingRecords,
    Ready,
    Materializing,
    Publishing,
    Active,
    Superseded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHistorySourceStatusV1 {
    pub source_generation_id: String,
    pub state: GitHistorySourceStateV1,
    pub commit_count: u64,
    pub logical_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

pub fn encode_history_fragment(fragment: &GitHistoryCommitFragmentV1) -> Vec<u8> {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-git-history-fragment-v1");
    push_field(&mut encoded, fragment.commit_oid.as_bytes());
    encoded.extend_from_slice(&fragment.fragment_index.to_be_bytes());
    encoded.extend_from_slice(&fragment.fragment_count.to_be_bytes());
    match &fragment.header {
        Some(header) => {
            encoded.push(1);
            encoded.extend_from_slice(&(header.parent_oids.len() as u64).to_be_bytes());
            for parent in &header.parent_oids {
                push_field(&mut encoded, parent.as_bytes());
            }
            push_field(&mut encoded, header.author_name.as_bytes());
            push_field(&mut encoded, header.author_email.as_bytes());
            push_field(&mut encoded, header.message.as_bytes());
        }
        None => encoded.push(0),
    }
    encoded.extend_from_slice(&(fragment.changed_paths.len() as u64).to_be_bytes());
    for path in &fragment.changed_paths {
        push_field(&mut encoded, path.as_bytes());
    }
    encoded
}

pub fn decode_history_fragment(
    encoded: &[u8],
) -> Result<GitHistoryCommitFragmentV1, ContractError> {
    let mut decoder = Decoder::new(encoded);
    if decoder.field()? != b"bbox-git-history-fragment-v1" {
        return Err(ContractError::HistoryRecordMismatch);
    }
    let commit_oid = decoder.string()?;
    let fragment_index = decoder.u32()?;
    let fragment_count = decoder.u32()?;
    let header = match decoder.byte()? {
        0 => None,
        1 => {
            let parent_count = decoder.count()?;
            let mut parent_oids = Vec::with_capacity(parent_count);
            for _ in 0..parent_count {
                parent_oids.push(decoder.string()?);
            }
            Some(GitHistoryCommitHeaderV1 {
                parent_oids,
                author_name: decoder.string()?,
                author_email: decoder.string()?,
                message: decoder.string()?,
            })
        }
        _ => return Err(ContractError::HistoryRecordMismatch),
    };
    let path_count = decoder.count()?;
    let mut changed_paths = Vec::with_capacity(path_count);
    for _ in 0..path_count {
        changed_paths.push(decoder.string()?);
    }
    decoder.finish()?;
    Ok(GitHistoryCommitFragmentV1 {
        commit_oid,
        fragment_index,
        fragment_count,
        header,
        changed_paths,
    })
}

pub fn history_record_sha256(fragment: &GitHistoryCommitFragmentV1) -> String {
    sha256(&encode_history_fragment(fragment))
}

pub fn history_manifest_sha256(entries: &[GitHistoryManifestEntryV1]) -> String {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-git-history-manifest-v1");
    encoded.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for entry in entries {
        push_field(&mut encoded, entry.commit_oid.as_bytes());
        encoded.extend_from_slice(&entry.fragment_index.to_be_bytes());
        encoded.extend_from_slice(&entry.encoded_bytes.to_be_bytes());
        push_field(&mut encoded, entry.content_sha256.as_bytes());
    }
    sha256(&encoded)
}

pub fn validate_history_manifest(
    descriptor: &GitHistoryDescriptorV1,
    entries: &[GitHistoryManifestEntryV1],
    limits: GitSourceLimits,
) -> Result<(), ContractError> {
    descriptor.validate_header(limits)?;
    if entries.len() as u64 != descriptor.fragment_count {
        return Err(ContractError::HistoryCountMismatch);
    }
    let mut previous: Option<(&str, u32)> = None;
    let mut commits = BTreeSet::new();
    let mut next_fragment = BTreeMap::<&str, u32>::new();
    let mut logical_bytes = 0_u64;
    for entry in entries {
        validate_object_id(&entry.commit_oid, descriptor.object_format)?;
        validate_sha256(&entry.content_sha256)?;
        if entry.encoded_bytes == 0 || entry.encoded_bytes > MAX_HISTORY_RECORD_BYTES {
            return Err(ContractError::HistoryRecordTooLarge);
        }
        if previous.is_some_and(|prior| prior >= (entry.commit_oid.as_str(), entry.fragment_index))
        {
            return Err(ContractError::HistoryManifestOutOfOrder);
        }
        let expected = next_fragment.entry(&entry.commit_oid).or_insert(0);
        if entry.fragment_index != *expected {
            return Err(ContractError::HistoryFragmentGap);
        }
        *expected = expected
            .checked_add(1)
            .ok_or(ContractError::HistoryLimitExceeded)?;
        commits.insert(entry.commit_oid.as_str());
        logical_bytes = logical_bytes
            .checked_add(entry.encoded_bytes)
            .ok_or(ContractError::HistoryLimitExceeded)?;
        previous = Some((&entry.commit_oid, entry.fragment_index));
    }
    if commits.len() as u64 != descriptor.commit_count || logical_bytes != descriptor.logical_bytes
    {
        return Err(ContractError::HistoryCountMismatch);
    }
    if history_manifest_sha256(entries) != descriptor.manifest_sha256 {
        return Err(ContractError::HistoryCommitmentMismatch);
    }
    Ok(())
}

pub fn validate_history_source(
    descriptor: &GitHistoryDescriptorV1,
    manifest: &[GitHistoryManifestEntryV1],
    fragments: &[GitHistoryCommitFragmentV1],
    limits: GitSourceLimits,
) -> Result<(), ContractError> {
    if manifest.len() != fragments.len() {
        return Err(ContractError::HistoryCountMismatch);
    }
    let mut verifier = HistorySourceVerifier::new(descriptor, manifest, limits)?;
    for fragment in fragments {
        verifier.push_fragment(fragment)?;
    }
    verifier.finish()
}

/// Incremental complete-snapshot verifier. It retains only graph closure and
/// per-commit fragment evidence, so a bounded store can validate records one
/// at a time without loading all commit messages and changed paths together.
pub struct HistorySourceVerifier<'a> {
    descriptor: &'a GitHistoryDescriptorV1,
    manifest: &'a [GitHistoryManifestEntryV1],
    next: usize,
    parents_by_commit: BTreeMap<String, Vec<String>>,
    fragment_counts: BTreeMap<String, u32>,
    observed_fragments: BTreeMap<String, u32>,
    last_path_by_commit: BTreeMap<String, String>,
}

impl<'a> HistorySourceVerifier<'a> {
    pub fn new(
        descriptor: &'a GitHistoryDescriptorV1,
        manifest: &'a [GitHistoryManifestEntryV1],
        limits: GitSourceLimits,
    ) -> Result<Self, ContractError> {
        validate_history_manifest(descriptor, manifest, limits)?;
        Ok(Self {
            descriptor,
            manifest,
            next: 0,
            parents_by_commit: BTreeMap::new(),
            fragment_counts: BTreeMap::new(),
            observed_fragments: BTreeMap::new(),
            last_path_by_commit: BTreeMap::new(),
        })
    }

    pub fn push_encoded(&mut self, encoded: &[u8]) -> Result<(), ContractError> {
        let fragment = decode_history_fragment(encoded)?;
        self.push_fragment_with_bytes(&fragment, encoded)
    }

    pub fn push_fragment(
        &mut self,
        fragment: &GitHistoryCommitFragmentV1,
    ) -> Result<(), ContractError> {
        let encoded = encode_history_fragment(fragment);
        self.push_fragment_with_bytes(fragment, &encoded)
    }

    fn push_fragment_with_bytes(
        &mut self,
        fragment: &GitHistoryCommitFragmentV1,
        encoded: &[u8],
    ) -> Result<(), ContractError> {
        let entry = self
            .manifest
            .get(self.next)
            .ok_or(ContractError::HistoryCountMismatch)?;
        validate_history_fragment(fragment, self.descriptor.object_format)?;
        if entry.commit_oid != fragment.commit_oid
            || entry.fragment_index != fragment.fragment_index
            || entry.encoded_bytes != encoded.len() as u64
            || entry.content_sha256 != sha256(encoded)
        {
            return Err(ContractError::HistoryRecordMismatch);
        }
        let expected_count = self
            .fragment_counts
            .entry(fragment.commit_oid.clone())
            .or_insert(fragment.fragment_count);
        if *expected_count != fragment.fragment_count {
            return Err(ContractError::HistoryFragmentGap);
        }
        *self
            .observed_fragments
            .entry(fragment.commit_oid.clone())
            .or_default() += 1;
        if let Some(header) = &fragment.header {
            if self
                .parents_by_commit
                .insert(fragment.commit_oid.clone(), header.parent_oids.clone())
                .is_some()
            {
                return Err(ContractError::HistoryRecordMismatch);
            }
        }
        if let Some(first) = fragment.changed_paths.first()
            && self
                .last_path_by_commit
                .get(&fragment.commit_oid)
                .is_some_and(|last| last >= first)
        {
            return Err(ContractError::HistoryRecordMismatch);
        }
        if let Some(last) = fragment.changed_paths.last() {
            self.last_path_by_commit
                .insert(fragment.commit_oid.clone(), last.clone());
        }
        self.next += 1;
        Ok(())
    }

    pub fn finish(self) -> Result<(), ContractError> {
        if self.next != self.manifest.len() {
            return Err(ContractError::HistoryCountMismatch);
        }
        for (commit, fragment_count) in &self.fragment_counts {
            if self.observed_fragments.get(commit) != Some(fragment_count)
                || !self.parents_by_commit.contains_key(commit)
            {
                return Err(ContractError::HistoryFragmentGap);
            }
        }
        if !self
            .parents_by_commit
            .contains_key(&self.descriptor.repo_head)
        {
            return Err(ContractError::HistoryGraphIncomplete);
        }
        for parents in self.parents_by_commit.values() {
            if parents
                .iter()
                .any(|parent| !self.parents_by_commit.contains_key(parent))
            {
                return Err(ContractError::HistoryGraphIncomplete);
            }
        }
        let mut reachable = BTreeSet::new();
        let mut pending = vec![self.descriptor.repo_head.clone()];
        while let Some(commit) = pending.pop() {
            if !reachable.insert(commit.clone()) {
                continue;
            }
            pending.extend(self.parents_by_commit[&commit].iter().cloned());
        }
        if reachable.len() != self.parents_by_commit.len() {
            return Err(ContractError::HistoryUnreachableRecord);
        }
        Ok(())
    }
}

pub fn history_source_generation_id(
    producer_id: &str,
    repo_history_id: &RepoHistoryId,
    primary_namespace: &CommitNamespace,
    descriptor: &GitHistoryDescriptorV1,
) -> Result<String, ContractError> {
    validate_producer_id(producer_id)?;
    descriptor.validate_header(GitSourceLimits {
        max_history_commits: u64::MAX,
        max_history_logical_bytes: u64::MAX,
        ..GitSourceLimits::default()
    })?;
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-git-history-source-generation-v1");
    push_field(&mut encoded, producer_id.as_bytes());
    push_field(&mut encoded, repo_history_id.as_str().as_bytes());
    push_field(&mut encoded, primary_namespace.as_str().as_bytes());
    push_field(&mut encoded, descriptor.repo_head.as_bytes());
    encoded.push(match descriptor.object_format {
        GitObjectFormatV1::Sha1 => 1,
        GitObjectFormatV1::Sha256 => 2,
    });
    push_field(&mut encoded, descriptor.manifest_sha256.as_bytes());
    Ok(format!("ghs_{}", sha256(&encoded)))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceExportPullRequestV1 {
    pub scope: PublishedScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
}

/// Transport envelope for one page. The embedded page deliberately remains
/// the landed interactive MCP/CLI contract; plan-wide receipt evidence lives
/// beside it so adding authenticated transport does not change those bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceExportPageResponseV1 {
    pub schema_version: u32,
    pub page: bbox_provenance::ProvenanceExportPage,
    pub document_count: u64,
    pub logical_bytes: u64,
    pub ordered_document_commitment: String,
}

impl ProvenanceExportPageResponseV1 {
    pub fn validate(&self, limits: GitSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        self.page
            .scope
            .validate()
            .map_err(|_| ContractError::InvalidScope)?;
        validate_notes_ref(&self.page.notes_ref)?;
        validate_sha256(&self.page.generation)?;
        validate_sha256(&self.ordered_document_commitment)?;
        if self.document_count > limits.max_provenance_documents
            || self.logical_bytes > limits.max_provenance_logical_bytes
            || self.page.documents.len() as u64 > self.document_count
        {
            return Err(ContractError::ProvenanceLimitExceeded);
        }
        for document in &self.page.documents {
            if document.document.len() as u64 > MAX_PROVENANCE_DOCUMENT_BYTES
                || document.document_sha256 != bbox_provenance::document_sha256(&document.document)
            {
                return Err(ContractError::ProvenanceCommitmentMismatch);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceExportReceiptV1 {
    pub schema_version: u32,
    pub scope: PublishedScope,
    pub generation: String,
    pub notes_ref: String,
    pub document_count: u64,
    pub ordered_document_commitment: String,
    pub local_notes_tip: String,
    pub written: u64,
    pub unchanged: u64,
}

impl ProvenanceExportReceiptV1 {
    pub fn validate(&self, limits: GitSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        self.scope
            .validate()
            .map_err(|_| ContractError::InvalidScope)?;
        validate_sha256(&self.generation)?;
        validate_notes_ref(&self.notes_ref)?;
        validate_sha256(&self.ordered_document_commitment)?;
        if self.local_notes_tip.is_empty() {
            if self.document_count != 0 {
                return Err(ContractError::InvalidObjectId);
            }
        } else {
            validate_any_object_id(&self.local_notes_tip)?;
        }
        if self.document_count > limits.max_provenance_documents
            || self.written.checked_add(self.unchanged) != Some(self.document_count)
        {
            return Err(ContractError::ProvenanceLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceImportDescriptorV1 {
    pub schema_version: u32,
    pub scope: PublishedScope,
    pub notes_ref: String,
    pub notes_tip: String,
    pub manifest_sha256: String,
    pub document_count: u64,
    pub logical_bytes: u64,
}

impl ProvenanceImportDescriptorV1 {
    pub fn validate_header(&self, limits: GitSourceLimits) -> Result<(), ContractError> {
        validate_schema(self.schema_version)?;
        self.scope
            .validate()
            .map_err(|_| ContractError::InvalidScope)?;
        validate_notes_ref(&self.notes_ref)?;
        if !self.notes_tip.is_empty() {
            validate_any_object_id(&self.notes_tip)?;
        }
        validate_sha256(&self.manifest_sha256)?;
        if self.document_count > limits.max_provenance_documents
            || self.logical_bytes > limits.max_provenance_logical_bytes
        {
            return Err(ContractError::ProvenanceLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceImportManifestEntryV1 {
    pub note_commit: String,
    pub document_ordinal: u32,
    pub encoded_bytes: u64,
    pub document_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceImportManifestPageV1 {
    pub entries: Vec<ProvenanceImportManifestEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginProvenanceImportRequestV1 {
    pub descriptor: ProvenanceImportDescriptorV1,
}

pub fn provenance_manifest_sha256(entries: &[ProvenanceImportManifestEntryV1]) -> String {
    let mut encoded = Vec::new();
    push_field(&mut encoded, b"bbox-provenance-import-manifest-v1");
    encoded.extend_from_slice(&(entries.len() as u64).to_be_bytes());
    for entry in entries {
        push_field(&mut encoded, entry.note_commit.as_bytes());
        encoded.extend_from_slice(&entry.document_ordinal.to_be_bytes());
        encoded.extend_from_slice(&entry.encoded_bytes.to_be_bytes());
        push_field(&mut encoded, entry.document_sha256.as_bytes());
    }
    sha256(&encoded)
}

pub fn validate_provenance_manifest(
    descriptor: &ProvenanceImportDescriptorV1,
    entries: &[ProvenanceImportManifestEntryV1],
    limits: GitSourceLimits,
) -> Result<(), ContractError> {
    descriptor.validate_header(limits)?;
    if entries.len() as u64 != descriptor.document_count {
        return Err(ContractError::ProvenanceCountMismatch);
    }
    let mut previous: Option<(&str, u32)> = None;
    let mut logical_bytes = 0_u64;
    for entry in entries {
        validate_any_object_id(&entry.note_commit)?;
        validate_sha256(&entry.document_sha256)?;
        if entry.encoded_bytes > MAX_PROVENANCE_DOCUMENT_BYTES {
            return Err(ContractError::ProvenanceLimitExceeded);
        }
        if previous
            .is_some_and(|prior| prior >= (entry.note_commit.as_str(), entry.document_ordinal))
        {
            return Err(ContractError::ProvenanceManifestOutOfOrder);
        }
        logical_bytes = logical_bytes
            .checked_add(entry.encoded_bytes)
            .ok_or(ContractError::ProvenanceLimitExceeded)?;
        previous = Some((&entry.note_commit, entry.document_ordinal));
    }
    if logical_bytes != descriptor.logical_bytes {
        return Err(ContractError::ProvenanceCountMismatch);
    }
    if provenance_manifest_sha256(entries) != descriptor.manifest_sha256 {
        return Err(ContractError::ProvenanceCommitmentMismatch);
    }
    Ok(())
}

pub fn validate_provenance_documents(
    descriptor: &ProvenanceImportDescriptorV1,
    manifest: &[ProvenanceImportManifestEntryV1],
    documents: &[String],
    limits: GitSourceLimits,
) -> Result<(), ContractError> {
    validate_provenance_manifest(descriptor, manifest, limits)?;
    if manifest.len() != documents.len() {
        return Err(ContractError::ProvenanceCountMismatch);
    }
    for (entry, document) in manifest.iter().zip(documents) {
        if entry.encoded_bytes != document.len() as u64
            || entry.document_sha256 != bbox_provenance::document_sha256(document)
        {
            return Err(ContractError::ProvenanceDocumentMismatch);
        }
        let note = bbox_provenance::parse_note_document(document)
            .map_err(|_| ContractError::ProvenanceDocumentInvalid)?;
        if note.commit != entry.note_commit {
            return Err(ContractError::ProvenanceDocumentMismatch);
        }
    }
    Ok(())
}

fn validate_history_fragment(
    fragment: &GitHistoryCommitFragmentV1,
    object_format: GitObjectFormatV1,
) -> Result<(), ContractError> {
    validate_object_id(&fragment.commit_oid, object_format)?;
    if fragment.fragment_count == 0 || fragment.fragment_index >= fragment.fragment_count {
        return Err(ContractError::HistoryFragmentGap);
    }
    if (fragment.fragment_index == 0) != fragment.header.is_some() {
        return Err(ContractError::HistoryRecordMismatch);
    }
    if let Some(header) = &fragment.header {
        for parent in &header.parent_oids {
            validate_object_id(parent, object_format)?;
        }
        if header.author_name.len() > MAX_AUTHOR_FIELD_BYTES
            || header.author_email.len() > MAX_AUTHOR_FIELD_BYTES
            || header.message.len() > MAX_COMMIT_MESSAGE_BYTES
        {
            return Err(ContractError::HistoryRecordTooLarge);
        }
    }
    ensure_strictly_sorted_paths(&fragment.changed_paths)?;
    if encode_history_fragment(fragment).len() as u64 > MAX_HISTORY_RECORD_BYTES {
        return Err(ContractError::HistoryRecordTooLarge);
    }
    Ok(())
}

fn ensure_strictly_sorted_paths(paths: &[String]) -> Result<(), ContractError> {
    let mut previous: Option<&str> = None;
    for path in paths {
        validate_relative_path(path)?;
        if previous.is_some_and(|prior| prior >= path.as_str()) {
            return Err(ContractError::HistoryRecordMismatch);
        }
        previous = Some(path);
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), ContractError> {
    if path.is_empty()
        || path.len() > MAX_RELATIVE_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(ContractError::InvalidRelativePath);
    }
    Ok(())
}

fn validate_schema(schema_version: u32) -> Result<(), ContractError> {
    if schema_version != SCHEMA_VERSION {
        return Err(ContractError::UnsupportedSchema(schema_version));
    }
    Ok(())
}

fn validate_object_id(oid: &str, object_format: GitObjectFormatV1) -> Result<(), ContractError> {
    if oid.len() != object_format.oid_len() || !is_lower_hex(oid) {
        return Err(ContractError::InvalidObjectId);
    }
    Ok(())
}

fn validate_any_object_id(oid: &str) -> Result<(), ContractError> {
    match oid.len() {
        40 if is_lower_hex(oid) => Ok(()),
        64 if is_lower_hex(oid) => Ok(()),
        _ => Err(ContractError::InvalidObjectId),
    }
}

fn validate_sha256(value: &str) -> Result<(), ContractError> {
    if value.len() != 64 || !is_lower_hex(value) {
        return Err(ContractError::InvalidDigest);
    }
    Ok(())
}

fn validate_notes_ref(notes_ref: &str) -> Result<(), ContractError> {
    bbox_provenance::validate_notes_ref(notes_ref).map_err(|_| ContractError::InvalidNotesRef)
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

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn push_field(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn byte(&mut self) -> Result<u8, ContractError> {
        let Some((byte, rest)) = self.remaining.split_first() else {
            return Err(ContractError::HistoryRecordMismatch);
        };
        self.remaining = rest;
        Ok(*byte)
    }

    fn u32(&mut self) -> Result<u32, ContractError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().expect("exact length")))
    }

    fn u64(&mut self) -> Result<u64, ContractError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().expect("exact length")))
    }

    fn count(&mut self) -> Result<usize, ContractError> {
        let count =
            usize::try_from(self.u64()?).map_err(|_| ContractError::HistoryRecordTooLarge)?;
        if count > self.remaining.len() / 8 {
            return Err(ContractError::HistoryRecordTooLarge);
        }
        Ok(count)
    }

    fn field(&mut self) -> Result<&'a [u8], ContractError> {
        let len = usize::try_from(self.u64()?).map_err(|_| ContractError::HistoryRecordTooLarge)?;
        self.take(len)
    }

    fn string(&mut self) -> Result<String, ContractError> {
        String::from_utf8(self.field()?.to_vec()).map_err(|_| ContractError::HistoryRecordMismatch)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ContractError> {
        if len > self.remaining.len() {
            return Err(ContractError::HistoryRecordMismatch);
        }
        let (head, tail) = self.remaining.split_at(len);
        self.remaining = tail;
        Ok(head)
    }

    fn finish(self) -> Result<(), ContractError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(ContractError::HistoryRecordMismatch)
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_provenance::{GitProvenanceNote, ProducedBy, ProvenanceExportDocument};

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-a", ".").unwrap()
    }

    fn fragment(oid: &str, parents: Vec<String>, paths: Vec<String>) -> GitHistoryCommitFragmentV1 {
        GitHistoryCommitFragmentV1 {
            commit_oid: oid.to_string(),
            fragment_index: 0,
            fragment_count: 1,
            header: Some(GitHistoryCommitHeaderV1 {
                parent_oids: parents,
                author_name: "A".into(),
                author_email: "a@example.invalid".into(),
                message: "message".into(),
            }),
            changed_paths: paths,
        }
    }

    fn history_fixture() -> (
        GitHistoryDescriptorV1,
        Vec<GitHistoryManifestEntryV1>,
        Vec<GitHistoryCommitFragmentV1>,
    ) {
        let root = "1".repeat(40);
        let head = "2".repeat(40);
        let fragments = vec![
            fragment(&root, vec![], vec!["README.md".into()]),
            fragment(&head, vec![root], vec!["src/lib.rs".into()]),
        ];
        let manifest = fragments
            .iter()
            .map(|fragment| {
                let encoded = encode_history_fragment(fragment);
                GitHistoryManifestEntryV1 {
                    commit_oid: fragment.commit_oid.clone(),
                    fragment_index: fragment.fragment_index,
                    encoded_bytes: encoded.len() as u64,
                    content_sha256: sha256(&encoded),
                }
            })
            .collect::<Vec<_>>();
        let descriptor = GitHistoryDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            repo_head: head,
            object_format: GitObjectFormatV1::Sha1,
            manifest_sha256: history_manifest_sha256(&manifest),
            commit_count: 2,
            fragment_count: 2,
            logical_bytes: manifest.iter().map(|entry| entry.encoded_bytes).sum(),
        };
        (descriptor, manifest, fragments)
    }

    #[test]
    fn complete_history_snapshot_validates() {
        let (descriptor, manifest, fragments) = history_fixture();
        validate_history_source(
            &descriptor,
            &manifest,
            &fragments,
            GitSourceLimits::default(),
        )
        .unwrap();
        for fragment in fragments {
            let encoded = encode_history_fragment(&fragment);
            assert_eq!(decode_history_fragment(&encoded).unwrap(), fragment);
        }
    }

    #[test]
    fn history_refuses_missing_parent_and_unreachable_commit() {
        let (descriptor, manifest, mut fragments) = history_fixture();
        fragments[1].header.as_mut().unwrap().parent_oids = vec!["3".repeat(40)];
        assert_eq!(
            validate_history_source(
                &descriptor,
                &manifest,
                &fragments,
                GitSourceLimits::default()
            ),
            Err(ContractError::HistoryRecordMismatch),
            "mutating a content-addressed record is rejected before graph validation"
        );

        let (mut descriptor, mut manifest, mut fragments) = history_fixture();
        let orphan = fragment(&"3".repeat(40), vec![], vec!["orphan.md".into()]);
        let encoded = encode_history_fragment(&orphan);
        manifest.push(GitHistoryManifestEntryV1 {
            commit_oid: orphan.commit_oid.clone(),
            fragment_index: 0,
            encoded_bytes: encoded.len() as u64,
            content_sha256: sha256(&encoded),
        });
        fragments.push(orphan);
        descriptor.commit_count = 3;
        descriptor.fragment_count = 3;
        descriptor.logical_bytes = manifest.iter().map(|entry| entry.encoded_bytes).sum();
        descriptor.manifest_sha256 = history_manifest_sha256(&manifest);
        assert_eq!(
            validate_history_source(
                &descriptor,
                &manifest,
                &fragments,
                GitSourceLimits::default()
            ),
            Err(ContractError::HistoryUnreachableRecord)
        );
    }

    #[test]
    fn canonical_hashes_change_with_authority_and_content() {
        let (descriptor, manifest, mut fragments) = history_fixture();
        let history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let first =
            history_source_generation_id("producer-a", &history_id, &namespace, &descriptor)
                .unwrap();
        let second =
            history_source_generation_id("producer-b", &history_id, &namespace, &descriptor)
                .unwrap();
        assert_ne!(first, second);
        let before = history_record_sha256(&fragments[0]);
        fragments[0].header.as_mut().unwrap().message.push('!');
        assert_ne!(before, history_record_sha256(&fragments[0]));
        assert_eq!(
            descriptor.manifest_sha256,
            history_manifest_sha256(&manifest)
        );
    }

    #[test]
    fn contracts_reject_unknown_fields() {
        let json = r#"{
            "schema_version":1,
            "scope":{"repo_id":"repo-a","bbox_root_relpath":"."},
            "repo_head":"1111111111111111111111111111111111111111",
            "object_format":"sha1",
            "manifest_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "commit_count":1,
            "fragment_count":1,
            "logical_bytes":1,
            "project_id":"caller-chosen"
        }"#;
        assert!(serde_json::from_str::<GitHistoryDescriptorV1>(json).is_err());
    }

    #[test]
    fn provenance_manifest_reuses_landed_note_schema() {
        let note = GitProvenanceNote::new_v2("1".repeat(40), ProducedBy::default(), vec![], vec![]);
        let document = ProvenanceExportDocument::from_note(&note).unwrap().document;
        let mut manifest = vec![ProvenanceImportManifestEntryV1 {
            note_commit: "1".repeat(40),
            document_ordinal: 0,
            encoded_bytes: document.len() as u64,
            document_sha256: bbox_provenance::document_sha256(&document),
        }];
        let descriptor = ProvenanceImportDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope(),
            notes_ref: "refs/notes/bbox/provenance".into(),
            notes_tip: "2".repeat(40),
            manifest_sha256: provenance_manifest_sha256(&manifest),
            document_count: 1,
            logical_bytes: document.len() as u64,
        };
        validate_provenance_documents(
            &descriptor,
            &manifest,
            std::slice::from_ref(&document),
            GitSourceLimits::default(),
        )
        .unwrap();

        manifest[0].document_sha256 = "f".repeat(64);
        assert_eq!(
            validate_provenance_documents(
                &descriptor,
                &manifest,
                &[document],
                GitSourceLimits::default()
            ),
            Err(ContractError::ProvenanceCommitmentMismatch)
        );
    }

    #[test]
    fn provenance_export_transport_envelope_and_empty_receipt_validate() {
        let plan = bbox_provenance::ProvenanceExportPlan::new(
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            Vec::new(),
        )
        .unwrap();
        let response = ProvenanceExportPageResponseV1 {
            schema_version: SCHEMA_VERSION,
            page: plan.page(Vec::new(), None),
            document_count: 0,
            logical_bytes: 0,
            ordered_document_commitment: plan.ordered_document_commitment().unwrap(),
        };
        response.validate(GitSourceLimits::default()).unwrap();
        ProvenanceExportReceiptV1 {
            schema_version: SCHEMA_VERSION,
            scope: response.page.scope.clone(),
            generation: response.page.generation.clone(),
            notes_ref: response.page.notes_ref.clone(),
            document_count: 0,
            ordered_document_commitment: response.ordered_document_commitment,
            local_notes_tip: String::new(),
            written: 0,
            unchanged: 0,
        }
        .validate(GitSourceLimits::default())
        .unwrap();
    }
}
