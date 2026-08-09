//! Durable intake store for complete typed Git-history snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::git::GitCommit;
use bbox_corpus_core::git_overlay::GitOverlaySelector;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow,
};
use bbox_corpus_core::project_catalog::{CommitNamespace, RepoHistoryId};
use bbox_git_source::{
    BeginGitHistoryUploadResponseV1, BeginProvenanceImportResponseV1,
    FinalizeGitHistoryUploadResponseV1, FinalizeProvenanceImportResponseV1, GitHistoryDescriptorV1,
    GitHistoryManifestEntryV1, GitHistoryManifestPageV1, GitHistorySourceStateV1,
    GitHistorySourceStatusV1, GitSourceLimits, HistorySourceVerifier,
    MAX_HISTORY_MANIFEST_PAGE_BYTES, MAX_HISTORY_MANIFEST_PAGE_ENTRIES, MAX_HISTORY_RECORD_BYTES,
    MAX_PROVENANCE_DOCUMENT_BYTES, MAX_PROVENANCE_MANIFEST_PAGE_BYTES,
    MAX_PROVENANCE_MANIFEST_PAGE_ENTRIES, MissingHistoryRecordsPageV1,
    MissingProvenanceDocumentsPageV1, ProvenanceExportReceiptV1, ProvenanceImportDescriptorV1,
    ProvenanceImportManifestEntryV1, ProvenanceImportManifestPageV1, ProvenanceImportStateV1,
    ProvenanceImportStatusV1, ProvenanceSourceVerifier, history_source_generation_id,
    provenance_import_generation_id, validate_history_manifest, validate_provenance_manifest,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MAX_UPLOAD_RECORD_BYTES: usize = 256 * 1024;
const MAX_GENERATION_RECORD_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MAX_PROVENANCE_RECEIPT_BYTES: usize = 64 * 1024;
const MISSING_PAGE_SIZE: usize = 1_000;
const HISTORY_UPLOAD_IDLE_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    pub contract: GitSourceLimits,
    pub max_open_uploads_per_producer: usize,
    pub retained_history_generations: usize,
    pub unreferenced_record_grace_secs: u64,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            contract: GitSourceLimits::default(),
            max_open_uploads_per_producer: 2,
            retained_history_generations: 2,
            unreferenced_record_grace_secs: 7 * 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub expired_uploads: u64,
    pub retired_generations: u64,
    pub deleted_records: u64,
    pub deleted_record_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreRequestError {
    LimitExceeded,
    TooManyOpenUploads,
    InvalidState,
    InvalidInput,
    NotFound,
}

impl std::fmt::Display for StoreRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitExceeded => "Git-source input exceeds an enforced limit",
            Self::TooManyOpenUploads => "producer has too many open Git-source uploads",
            Self::InvalidState => "Git-source upload is not in the required state",
            Self::InvalidInput => "Git-source input is invalid",
            Self::NotFound => "Git-source resource was not found",
        })
    }
}

impl std::error::Error for StoreRequestError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryUploadRecordV1 {
    version: u32,
    upload_id: String,
    producer_id: String,
    repo_history_id: RepoHistoryId,
    primary_namespace: CommitNamespace,
    descriptor: GitHistoryDescriptorV1,
    state: GitHistorySourceStateV1,
    next_page: u32,
    page_digests: BTreeMap<u32, String>,
    source_generation_id: Option<String>,
    updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceUploadRecordV1 {
    version: u32,
    upload_id: String,
    producer_id: String,
    project_id: String,
    descriptor: ProvenanceImportDescriptorV1,
    state: ProvenanceImportStateV1,
    next_page: u32,
    page_digests: BTreeMap<u32, String>,
    import_generation_id: String,
    updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredProvenanceImportV1 {
    pub version: u32,
    pub import_generation_id: String,
    pub producer_id: String,
    pub project_id: String,
    pub descriptor: ProvenanceImportDescriptorV1,
    pub state: ProvenanceImportStateV1,
    pub created_unix_secs: u64,
    pub accepted_sequence: u64,
    #[serde(default)]
    pub edges_imported: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceGenerationIndexV1 {
    version: u32,
    import_generation_id: String,
    producer_id: String,
    project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceReadyPointerV1 {
    version: u32,
    import_generation_id: String,
    producer_id: String,
    notes_tip: String,
    accepted_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ProvenanceAcceptanceSequenceV1 {
    version: u32,
    next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProvenanceImportV1 {
    pub import_generation_id: String,
    pub producer_id: String,
    pub project_id: String,
    pub scope: bbox_corpus_core::identity::PublishedScope,
    pub notes_ref: String,
    pub notes_tip: String,
    pub manifest_sha256: String,
    pub source_evidence: String,
    pub document_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedProvenanceDocumentV1 {
    pub note_commit: String,
    pub document_ordinal: u32,
    pub document_sha256: String,
    pub document: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceImportAuthorityV1 {
    pub scope: bbox_corpus_core::identity::PublishedScope,
    pub project_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceImportStageV1 {
    Prepared,
    EdgesPublished,
    Committed,
    Superseded,
    Quarantined,
}

impl ProvenanceImportStageV1 {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Superseded | Self::Quarantined)
    }

    fn ordinal(self) -> Option<u8> {
        match self {
            Self::Prepared => Some(0),
            Self::EdgesPublished => Some(1),
            Self::Committed => Some(2),
            Self::Superseded => None,
            Self::Quarantined => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceImportJournalV1 {
    pub version: u32,
    pub stage: ProvenanceImportStageV1,
    pub import_generation_id: String,
    pub producer_id: String,
    pub project_id: String,
    pub source_evidence: String,
    pub catalog_epoch: u64,
    pub code_selector: String,
    #[serde(default)]
    pub edge_count: u64,
    #[serde(default)]
    pub edge_keys_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    pub checksum_sha256: String,
}

impl ProvenanceImportJournalV1 {
    pub fn new_prepared(
        source: &VerifiedProvenanceImportV1,
        catalog_epoch: u64,
        code_selector: String,
    ) -> Result<Self> {
        Self {
            version: STORE_VERSION,
            stage: ProvenanceImportStageV1::Prepared,
            import_generation_id: source.import_generation_id.clone(),
            producer_id: source.producer_id.clone(),
            project_id: source.project_id.clone(),
            source_evidence: source.source_evidence.clone(),
            catalog_epoch,
            code_selector,
            edge_count: 0,
            edge_keys_sha256: String::new(),
            diagnostic: None,
            checksum_sha256: String::new(),
        }
        .seal()
    }

    pub fn seal(mut self) -> Result<Self> {
        self.checksum_sha256.clear();
        self.checksum_sha256 = sha256(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || self.project_id.is_empty()
            || self.code_selector.is_empty()
            || self.source_evidence.len() != 64
            || self
                .diagnostic
                .as_ref()
                .is_some_and(|value| value.len() > 512)
            || (self.stage == ProvenanceImportStageV1::Prepared && self.edge_count != 0)
            || (self.stage == ProvenanceImportStageV1::Prepared
                && !self.edge_keys_sha256.is_empty())
            || (matches!(
                self.stage,
                ProvenanceImportStageV1::EdgesPublished | ProvenanceImportStageV1::Committed
            ) && validate_sha256(&self.edge_keys_sha256).is_err())
            || (self.stage == ProvenanceImportStageV1::Superseded
                && !self.edge_keys_sha256.is_empty()
                && validate_sha256(&self.edge_keys_sha256).is_err())
            || (self.stage == ProvenanceImportStageV1::Quarantined
                && !self.edge_keys_sha256.is_empty())
            || (matches!(
                self.stage,
                ProvenanceImportStageV1::Superseded | ProvenanceImportStageV1::Quarantined
            ) && self.diagnostic.as_deref().is_none_or(str::is_empty))
            || (!matches!(
                self.stage,
                ProvenanceImportStageV1::Superseded | ProvenanceImportStageV1::Quarantined
            ) && self.diagnostic.is_some())
        {
            bail!(StoreRequestError::InvalidState);
        }
        validate_receipt_authority(&self.producer_id, &self.project_id)?;
        validate_provenance_generation_id(&self.import_generation_id)?;
        validate_sha256(&self.source_evidence)?;
        let mut projection = self.clone();
        let checksum = std::mem::take(&mut projection.checksum_sha256);
        if checksum != sha256(&serde_json::to_vec(&projection)?) {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(())
    }

    fn immutable_projection(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            self.version,
            &self.import_generation_id,
            &self.producer_id,
            &self.project_id,
            &self.source_evidence,
            self.catalog_epoch,
            &self.code_selector,
        ))?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredHistorySourceV1 {
    pub version: u32,
    pub source_generation_id: String,
    pub producer_id: String,
    pub repo_history_id: RepoHistoryId,
    pub primary_namespace: CommitNamespace,
    pub descriptor: GitHistoryDescriptorV1,
    pub state: GitHistorySourceStateV1,
    pub created_unix_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

/// Immutable, fully reverified handoff into the certified P3 history builder.
///
/// The handle carries metadata only. Commit records are visited one commit at
/// a time through [`GitSourceStore::visit_verified_history_commits`], keeping
/// source-sized payloads out of the daemon heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedGitHistorySourceV1 {
    pub source_generation_id: String,
    pub producer_id: String,
    pub authority_scope: bbox_corpus_core::identity::PublishedScope,
    pub repo_history_id: RepoHistoryId,
    pub primary_namespace: CommitNamespace,
    pub repo_head: String,
    pub manifest_sha256: String,
    pub source_evidence: String,
    pub commit_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedGitHistoryCommitV1 {
    pub commit: GitCommit,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryActivationStageV1 {
    Prepared,
    GenerationVerified,
    MaterializationAdvanced,
    CommitViewPublished,
    OverlaysPublished,
    Committed,
    Superseded,
}

impl HistoryActivationStageV1 {
    fn ordinal(self) -> Option<u8> {
        match self {
            Self::Prepared => Some(0),
            Self::GenerationVerified => Some(1),
            Self::MaterializationAdvanced => Some(2),
            Self::CommitViewPublished => Some(3),
            Self::OverlaysPublished => Some(4),
            Self::Committed => Some(5),
            Self::Superseded => None,
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Superseded)
    }

    pub fn is_at_least(self, expected: Self) -> bool {
        match (self.ordinal(), expected.ordinal()) {
            (Some(current), Some(expected)) => current >= expected,
            _ => self == expected,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryActivationOverlayV1 {
    pub project_id: String,
    pub snapshot_id: String,
    pub selector: GitOverlaySelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_commitment: Option<String>,
}

/// Monotonic durable lower bound for one repo-level history activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryActivationJournalV1 {
    pub version: u32,
    pub stage: HistoryActivationStageV1,
    pub source_generation_id: String,
    pub producer_id: String,
    pub source_evidence: String,
    pub grant_commitment: String,
    pub catalog_epoch_prepared: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_epoch_after: Option<u64>,
    pub repo_history_id: RepoHistoryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_p3_generation_id: Option<String>,
    pub planned_p3_generation_id: String,
    pub planned_p3_manifest_sha256: String,
    pub code_selectors: BTreeMap<String, String>,
    pub overlays: Vec<HistoryActivationOverlayV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlay_clears: Vec<String>,
    pub commit_document_count: u64,
    pub commit_document_commitment_sha256: String,
    pub vector_input_count: u64,
    pub vector_input_commitment_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_view_commitment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    pub checksum_sha256: String,
}

impl HistoryActivationJournalV1 {
    pub fn seal(mut self) -> Result<Self> {
        self.checksum_sha256 = self.recompute_checksum()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || self.source_generation_id.is_empty()
            || self.producer_id.is_empty()
            || self.source_evidence.len() != 64
            || self.grant_commitment.len() != 64
            || self.planned_p3_manifest_sha256.len() != 64
            || self.commit_document_commitment_sha256.len() != 64
            || self.vector_input_commitment_sha256.len() != 64
            || self.recompute_checksum()? != self.checksum_sha256
        {
            bail!(StoreRequestError::InvalidState);
        }
        validate_generation_id(&self.source_generation_id)?;
        for digest in [
            &self.grant_commitment,
            &self.source_evidence,
            &self.planned_p3_manifest_sha256,
            &self.commit_document_commitment_sha256,
            &self.vector_input_commitment_sha256,
        ] {
            validate_sha256(digest)?;
        }
        if let Some(commitment) = &self.commit_view_commitment {
            validate_sha256(commitment)?;
        }
        let mut previous = None;
        for overlay in &self.overlays {
            if overlay.project_id != overlay.selector.project_id
                || overlay.selector.repo_history_generation != self.planned_p3_generation_id
                || self.code_selectors.get(&overlay.project_id)
                    != Some(&overlay.selector.code_generation)
                || overlay.selector.source.producer_transport()
                    != Some((
                        self.producer_id.as_str(),
                        self.source_generation_id.as_str(),
                    ))
                || overlay.snapshot_id.is_empty()
                || previous
                    .as_ref()
                    .is_some_and(|prior| prior >= &overlay.project_id)
            {
                bail!(StoreRequestError::InvalidState);
            }
            if let Some(commitment) = &overlay.file_commitment {
                validate_sha256(commitment)?;
            }
            previous = Some(overlay.project_id.clone());
        }
        if self
            .overlay_clears
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
            || self.overlay_clears.iter().any(|project_id| {
                self.overlays
                    .iter()
                    .any(|overlay| overlay.project_id == *project_id)
            })
        {
            bail!(StoreRequestError::InvalidState);
        }
        if self.stage != HistoryActivationStageV1::Superseded {
            if self
                .stage
                .is_at_least(HistoryActivationStageV1::MaterializationAdvanced)
                && self.catalog_epoch_after.is_none()
            {
                bail!(StoreRequestError::InvalidState);
            }
            if self
                .stage
                .is_at_least(HistoryActivationStageV1::CommitViewPublished)
                && (self.commit_view_commitment.as_deref()
                    != Some(self.commit_document_commitment_sha256.as_str())
                    || self.overlays.iter().any(|overlay| {
                        overlay.file_commitment.as_deref().is_none_or(str::is_empty)
                    }))
            {
                bail!(StoreRequestError::InvalidState);
            }
        }
        Ok(())
    }

    fn recompute_checksum(&self) -> Result<String> {
        let mut projection = self.clone();
        projection.checksum_sha256.clear();
        Ok(sha256(&serde_json::to_vec(&projection)?))
    }

    fn immutable_projection(&self) -> Result<Vec<u8>> {
        let overlays = self
            .overlays
            .iter()
            .map(|overlay| (&overlay.project_id, &overlay.snapshot_id, &overlay.selector))
            .collect::<Vec<_>>();
        Ok(serde_json::to_vec(&(
            (
                self.version,
                &self.source_generation_id,
                &self.producer_id,
                &self.source_evidence,
                &self.grant_commitment,
                self.catalog_epoch_prepared,
                &self.repo_history_id,
                &self.prior_p3_generation_id,
                &self.planned_p3_generation_id,
            ),
            (
                &self.planned_p3_manifest_sha256,
                &self.code_selectors,
                overlays,
                &self.overlay_clears,
                self.commit_document_count,
                &self.commit_document_commitment_sha256,
                self.vector_input_count,
                &self.vector_input_commitment_sha256,
            ),
        ))?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct GenerationIndexV1 {
    version: u32,
    source_generation_id: String,
    producer_id: String,
    repo_history_id: RepoHistoryId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReadyPointerV1 {
    version: u32,
    source_generation_id: String,
    producer_id: String,
    repo_head: String,
}

pub struct GitSourceStore {
    root: PathBuf,
    limits: RwLock<StoreLimits>,
    mutation: Mutex<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTransportAuthorityV1 {
    pub scope: bbox_corpus_core::identity::PublishedScope,
    pub repo_history_id: RepoHistoryId,
    pub primary_namespace: CommitNamespace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHistorySourceAuthorityV1 {
    pub producer_id: String,
    pub repo_history_id: RepoHistoryId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredProvenanceExportReceiptV1 {
    pub version: u32,
    pub producer_id: String,
    pub project_id: String,
    pub receipt: ProvenanceExportReceiptV1,
    pub accepted_unix_secs: u64,
}

struct MutationGuard<'a> {
    _anchor: StoreLockGuard,
    _in_process: MutexGuard<'a, ()>,
}

impl GitSourceStore {
    pub fn open(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        validate_store_limits(limits)?;
        let root = root.into();
        NofollowDirectory::open_or_create(&root)?;
        for relative in [
            "uploads",
            "records",
            "records/sha256",
            "repos",
            "generation-index",
            "activations",
            "provenance-receipts",
            "provenance-imports",
            "provenance-imports/uploads",
            "provenance-imports/documents",
            "provenance-imports/documents/sha256",
            "provenance-imports/generations",
            "provenance-imports/generation-index",
            "provenance-imports/journals",
            "provenance-imports/projects",
        ] {
            NofollowDirectory::open_or_create(&root.join(relative))?;
        }
        Ok(Self {
            root,
            limits: RwLock::new(limits),
            mutation: Mutex::new(()),
        })
    }

    /// Open an already initialized store without creating any directory.
    /// Offline cutover preflight is observational and must not make an empty
    /// transport estate look initialized merely by inspecting it.
    pub fn open_existing(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        validate_store_limits(limits)?;
        let root = root.into();
        NofollowDirectory::open_existing(&root)?
            .ok_or_else(|| anyhow!("Git-source store root is missing"))?;
        for relative in [
            "uploads",
            "records",
            "records/sha256",
            "repos",
            "generation-index",
            "activations",
            "provenance-receipts",
            "provenance-imports",
            "provenance-imports/uploads",
            "provenance-imports/documents",
            "provenance-imports/documents/sha256",
            "provenance-imports/generations",
            "provenance-imports/generation-index",
            "provenance-imports/journals",
            "provenance-imports/projects",
        ] {
            NofollowDirectory::open_existing(&root.join(relative))?
                .ok_or_else(|| anyhow!("Git-source store member {relative} is missing"))?;
        }
        Ok(Self {
            root,
            limits: RwLock::new(limits),
            mutation: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn update_limits(&self, limits: StoreLimits) -> Result<()> {
        validate_store_limits(limits)?;
        *self
            .limits
            .write()
            .map_err(|_| anyhow!("Git-source limit lock is poisoned"))? = limits;
        Ok(())
    }

    pub fn current_contract_limits(&self) -> Result<GitSourceLimits> {
        Ok(self.current_limits()?.contract)
    }

    /// Atomically retain the last collector proof for one catalog project.
    /// An identical retry is a true no-op, including its acceptance time.
    pub fn record_provenance_export_receipt(
        &self,
        producer_id: &str,
        project_id: &str,
        receipt: ProvenanceExportReceiptV1,
    ) -> Result<StoredProvenanceExportReceiptV1> {
        let limits = self.current_limits()?;
        receipt.validate(limits.contract)?;
        validate_receipt_authority(producer_id, project_id)?;
        let _guard = self.lock_mutation()?;
        let directory = NofollowDirectory::open_or_create(&self.root.join("provenance-receipts"))?;
        let name = format!("{project_id}.json");
        if let Some(bytes) = directory.read_regular(
            &name,
            MAX_PROVENANCE_RECEIPT_BYTES,
            "provenance export receipt",
        )? {
            let existing: StoredProvenanceExportReceiptV1 =
                serde_json::from_slice(&bytes).context("decoding provenance export receipt")?;
            validate_stored_provenance_receipt(&existing, limits.contract)?;
            if existing.project_id != project_id {
                bail!("provenance receipt filename disagrees with its project id");
            }
            if existing.producer_id == producer_id && existing.receipt == receipt {
                return Ok(existing);
            }
        }
        let stored = StoredProvenanceExportReceiptV1 {
            version: STORE_VERSION,
            producer_id: producer_id.to_string(),
            project_id: project_id.to_string(),
            receipt,
            accepted_unix_secs: now_unix_secs(),
        };
        write_json(&directory, &name, &stored)?;
        Ok(stored)
    }

    pub fn provenance_export_receipt(
        &self,
        project_id: &str,
    ) -> Result<Option<StoredProvenanceExportReceiptV1>> {
        validate_receipt_authority("read", project_id)?;
        let Some(stored) = read_json::<StoredProvenanceExportReceiptV1>(
            &self.root.join("provenance-receipts"),
            &format!("{project_id}.json"),
            MAX_PROVENANCE_RECEIPT_BYTES,
            "provenance export receipt",
        )?
        else {
            return Ok(None);
        };
        validate_stored_provenance_receipt(&stored, self.current_limits()?.contract)?;
        if stored.project_id != project_id {
            bail!("provenance receipt filename disagrees with its project id");
        }
        Ok(Some(stored))
    }

    pub fn provenance_export_receipts(&self) -> Result<Vec<StoredProvenanceExportReceiptV1>> {
        let root = self.root.join("provenance-receipts");
        let limits = self.current_limits()?.contract;
        let mut receipts = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("refusing unsafe provenance receipt store member");
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("provenance receipt filename is not UTF-8"))?;
            let Some(project_id) = name.strip_suffix(".json") else {
                bail!("unexpected provenance receipt store member");
            };
            let stored = read_json::<StoredProvenanceExportReceiptV1>(
                &root,
                &name,
                MAX_PROVENANCE_RECEIPT_BYTES,
                "provenance export receipt",
            )?
            .ok_or_else(|| anyhow!("provenance receipt disappeared while reading"))?;
            validate_stored_provenance_receipt(&stored, limits)?;
            if stored.project_id != project_id {
                bail!("provenance receipt filename disagrees with its project id");
            }
            receipts.push(stored);
        }
        receipts.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(receipts)
    }

    pub fn begin_provenance_import(
        &self,
        producer_id: &str,
        project_id: &str,
        descriptor: ProvenanceImportDescriptorV1,
    ) -> Result<BeginProvenanceImportResponseV1> {
        validate_receipt_authority(producer_id, project_id)?;
        let limits = self.current_limits()?;
        descriptor.validate_header(limits.contract)?;
        let import_generation_id = provenance_import_generation_id(producer_id, &descriptor)?;
        let _guard = self.lock_mutation()?;
        let producer_dir = self.provenance_producer_upload_dir(producer_id)?;
        let mut open_uploads = 0_usize;
        for entry in read_directories(&producer_dir)? {
            let Some(record) = read_json::<ProvenanceUploadRecordV1>(
                &entry,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "provenance import upload record",
            )?
            else {
                continue;
            };
            if record.producer_id == producer_id
                && record.project_id == project_id
                && record.descriptor == descriptor
                && matches!(
                    record.state,
                    ProvenanceImportStateV1::ReceivingManifest
                        | ProvenanceImportStateV1::MissingDocuments
                )
            {
                return Ok(begin_provenance_response(record.upload_id));
            }
            if matches!(
                record.state,
                ProvenanceImportStateV1::ReceivingManifest
                    | ProvenanceImportStateV1::MissingDocuments
            ) {
                open_uploads += 1;
            }
        }
        if open_uploads >= limits.max_open_uploads_per_producer {
            bail!(StoreRequestError::TooManyOpenUploads);
        }
        let upload_id = Uuid::new_v4().simple().to_string();
        let upload_path = producer_dir.join(&upload_id);
        let upload_dir = NofollowDirectory::open_or_create(&upload_path)?;
        NofollowDirectory::open_or_create(&upload_path.join("pages"))?;
        write_json(
            &upload_dir,
            "upload.json",
            &ProvenanceUploadRecordV1 {
                version: STORE_VERSION,
                upload_id: upload_id.clone(),
                producer_id: producer_id.to_string(),
                project_id: project_id.to_string(),
                descriptor,
                state: ProvenanceImportStateV1::ReceivingManifest,
                next_page: 0,
                page_digests: BTreeMap::new(),
                import_generation_id,
                updated_unix_secs: now_unix_secs(),
            },
        )?;
        Ok(begin_provenance_response(upload_id))
    }

    pub fn put_provenance_manifest_page(
        &self,
        producer_id: &str,
        upload_id: &str,
        page: u32,
        body: &ProvenanceImportManifestPageV1,
    ) -> Result<()> {
        if body.entries.is_empty() || body.entries.len() > MAX_PROVENANCE_MANIFEST_PAGE_ENTRIES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let raw = serde_json::to_vec(body)?;
        if raw.len() > MAX_PROVENANCE_MANIFEST_PAGE_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let digest = sha256(&raw);
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let mut record = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if record.state != ProvenanceImportStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        if page < record.next_page {
            if record.page_digests.get(&page) == Some(&digest) {
                return Ok(());
            }
            bail!(StoreRequestError::InvalidInput);
        }
        if page != record.next_page {
            bail!(StoreRequestError::InvalidInput);
        }
        let pages = NofollowDirectory::open_existing(&upload_path.join("pages"))?
            .ok_or(StoreRequestError::InvalidState)?;
        pages.atomic_replace(&format!("{page:08}.json"), &raw)?;
        record.page_digests.insert(page, digest);
        record.next_page = record
            .next_page
            .checked_add(1)
            .ok_or(StoreRequestError::LimitExceeded)?;
        record.updated_unix_secs = now_unix_secs();
        let directory =
            NofollowDirectory::open_existing(&upload_path)?.ok_or(StoreRequestError::NotFound)?;
        write_json(&directory, "upload.json", &record)
    }

    pub fn complete_provenance_manifest(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<MissingProvenanceDocumentsPageV1> {
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let directory =
            NofollowDirectory::open_existing(&upload_path)?.ok_or(StoreRequestError::NotFound)?;
        let mut record = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if record.state == ProvenanceImportStateV1::MissingDocuments {
            return self.missing_provenance_documents_locked(&record, None);
        }
        if record.state != ProvenanceImportStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        let mut manifest = Vec::new();
        if record.descriptor.document_count > 0 {
            if record.next_page == 0 {
                bail!(StoreRequestError::InvalidState);
            }
            for page in 0..record.next_page {
                let body = read_json::<ProvenanceImportManifestPageV1>(
                    &upload_path.join("pages"),
                    &format!("{page:08}.json"),
                    MAX_PROVENANCE_MANIFEST_PAGE_BYTES,
                    "provenance import manifest page",
                )?
                .ok_or(StoreRequestError::InvalidState)?;
                manifest.extend(body.entries);
                if manifest.len() as u64 > record.descriptor.document_count {
                    bail!(StoreRequestError::LimitExceeded);
                }
            }
        } else if record.next_page != 0 {
            bail!(StoreRequestError::InvalidInput);
        }
        validate_provenance_manifest(
            &record.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        let raw = serde_json::to_vec(&manifest)?;
        if raw.len() > MAX_MANIFEST_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        directory.atomic_replace("manifest.json", &raw)?;
        record.state = ProvenanceImportStateV1::MissingDocuments;
        record.updated_unix_secs = now_unix_secs();
        write_json(&directory, "upload.json", &record)?;
        self.missing_provenance_documents_locked(&record, None)
    }

    pub fn missing_provenance_documents(
        &self,
        producer_id: &str,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingProvenanceDocumentsPageV1> {
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let record = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if record.state != ProvenanceImportStateV1::MissingDocuments {
            bail!(StoreRequestError::InvalidState);
        }
        self.missing_provenance_documents_locked(&record, cursor)
    }

    pub fn expected_provenance_document_size(
        &self,
        producer_id: &str,
        upload_id: &str,
        hash: &str,
    ) -> Result<u64> {
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let record = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if record.state != ProvenanceImportStateV1::MissingDocuments {
            bail!(StoreRequestError::InvalidState);
        }
        self.load_provenance_manifest(&upload_path)?
            .into_iter()
            .find(|entry| entry.document_sha256 == hash)
            .map(|entry| entry.encoded_bytes)
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))
    }

    pub fn install_provenance_document(
        &self,
        producer_id: &str,
        upload_id: &str,
        hash: &str,
        expected_size: u64,
        mut reader: impl Read,
    ) -> Result<()> {
        if expected_size > MAX_PROVENANCE_DOCUMENT_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let mut record = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if record.state != ProvenanceImportStateV1::MissingDocuments {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_provenance_manifest(&upload_path)?;
        let entry = manifest
            .iter()
            .find(|entry| entry.document_sha256 == hash)
            .ok_or(StoreRequestError::NotFound)?;
        if entry.encoded_bytes != expected_size {
            bail!(StoreRequestError::InvalidInput);
        }
        let mut bytes = Vec::with_capacity(expected_size as usize);
        reader
            .by_ref()
            .take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size || sha256(&bytes) != hash {
            bail!(StoreRequestError::InvalidInput);
        }
        std::str::from_utf8(&bytes).map_err(|_| StoreRequestError::InvalidInput)?;
        self.install_provenance_document_bytes(hash, &bytes)?;
        record.updated_unix_secs = now_unix_secs();
        let directory =
            NofollowDirectory::open_existing(&upload_path)?.ok_or(StoreRequestError::NotFound)?;
        write_json(&directory, "upload.json", &record)
    }

    pub fn finalize_provenance_import(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<FinalizeProvenanceImportResponseV1> {
        let _guard = self.lock_mutation()?;
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let upload_directory =
            NofollowDirectory::open_existing(&upload_path)?.ok_or(StoreRequestError::NotFound)?;
        let mut upload = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        if !matches!(
            upload.state,
            ProvenanceImportStateV1::MissingDocuments | ProvenanceImportStateV1::Ready
        ) {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_provenance_manifest(&upload_path)?;
        let mut verifier = ProvenanceSourceVerifier::new(
            &upload.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        for entry in &manifest {
            let bytes = self
                .read_provenance_document_bytes(
                    &entry.document_sha256,
                    entry.encoded_bytes as usize,
                )?
                .ok_or(StoreRequestError::InvalidState)?;
            let document = String::from_utf8(bytes).map_err(|_| StoreRequestError::InvalidInput)?;
            verifier.push(&document)?;
        }
        verifier.finish()?;
        let generation_path = self.provenance_generation_dir(&upload.import_generation_id)?;
        let existing = read_json::<StoredProvenanceImportV1>(
            &generation_path,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "stored provenance import",
        )?;
        let source = if let Some(mut existing) = existing {
            if existing.version != STORE_VERSION
                || existing.import_generation_id != upload.import_generation_id
                || existing.accepted_sequence == 0
                || existing.producer_id != producer_id
                || existing.project_id != upload.project_id
                || existing.descriptor != upload.descriptor
                || self.load_provenance_generation_manifest(&upload.import_generation_id)?
                    != manifest
            {
                bail!(StoreRequestError::InvalidInput);
            }
            // Quarantine is terminal for autonomous background redrive, but
            // finalizing the same fully verified immutable upload again is an
            // authenticated, explicit retry. Reopen only that exact source;
            // the activation worker replaces its terminal journal with a new
            // plan pinned to the current catalog and code selector.
            if existing.state == ProvenanceImportStateV1::Quarantined {
                existing.state = ProvenanceImportStateV1::Ready;
                existing.edges_imported = 0;
                existing.diagnostic = None;
                self.write_provenance_source(&existing)?;
            }
            existing
        } else {
            let generation_dir = NofollowDirectory::open_or_create(&generation_path)?;
            let source = StoredProvenanceImportV1 {
                version: STORE_VERSION,
                import_generation_id: upload.import_generation_id.clone(),
                producer_id: producer_id.to_string(),
                project_id: upload.project_id.clone(),
                descriptor: upload.descriptor.clone(),
                state: ProvenanceImportStateV1::Ready,
                created_unix_secs: now_unix_secs(),
                accepted_sequence: self
                    .allocate_provenance_acceptance_sequence(&upload.project_id)?,
                edges_imported: 0,
                diagnostic: None,
            };
            install_immutable_json(&generation_dir, "descriptor.json", &upload.descriptor)?;
            install_immutable_json(&generation_dir, "manifest.json", &manifest)?;
            install_immutable_json(&generation_dir, "source.json", &source)?;
            source
        };
        let index = NofollowDirectory::open_existing(
            &self.root.join("provenance-imports/generation-index"),
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        write_json(
            &index,
            &format!("{}.json", upload.import_generation_id),
            &ProvenanceGenerationIndexV1 {
                version: STORE_VERSION,
                import_generation_id: upload.import_generation_id.clone(),
                producer_id: producer_id.to_string(),
                project_id: upload.project_id.clone(),
            },
        )?;
        self.advance_provenance_ready_pointer(&source)?;
        upload.state = ProvenanceImportStateV1::Ready;
        upload.updated_unix_secs = now_unix_secs();
        write_json(&upload_directory, "upload.json", &upload)?;
        Ok(finalize_provenance_response(upload.import_generation_id))
    }

    pub fn provenance_import_status(
        &self,
        producer_id: &str,
        import_generation_id: &str,
    ) -> Result<ProvenanceImportStatusV1> {
        let source =
            self.load_provenance_generation_for_producer(producer_id, import_generation_id)?;
        Ok(ProvenanceImportStatusV1 {
            import_generation_id: source.import_generation_id,
            state: source.state,
            document_count: source.descriptor.document_count,
            logical_bytes: source.descriptor.logical_bytes,
            edges_imported: source.edges_imported,
            diagnostic: source.diagnostic,
        })
    }

    pub fn provenance_upload_authority(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<ProvenanceImportAuthorityV1> {
        let upload_path = self.provenance_upload_dir(producer_id, upload_id)?;
        let upload = self.load_provenance_upload(&upload_path, producer_id, upload_id)?;
        Ok(ProvenanceImportAuthorityV1 {
            scope: upload.descriptor.scope,
            project_id: upload.project_id,
        })
    }

    pub fn provenance_generation_authority(
        &self,
        producer_id: &str,
        import_generation_id: &str,
    ) -> Result<ProvenanceImportAuthorityV1> {
        let source =
            self.load_provenance_generation_for_producer(producer_id, import_generation_id)?;
        Ok(ProvenanceImportAuthorityV1 {
            scope: source.descriptor.scope,
            project_id: source.project_id,
        })
    }

    pub fn ready_provenance_import_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.root.join("provenance-imports/generation-index"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.file_type()?.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            let index = self.load_provenance_generation_index(id)?;
            let source = self.load_provenance_generation(id)?;
            if source.producer_id != index.producer_id || source.project_id != index.project_id {
                bail!(StoreRequestError::InvalidState);
            }
            if matches!(
                source.state,
                ProvenanceImportStateV1::Ready | ProvenanceImportStateV1::Importing
            ) {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn current_ready_provenance_import_id(&self, project_id: &str) -> Result<Option<String>> {
        let Some(pointer) = self.load_provenance_ready_pointer(project_id)? else {
            return Ok(None);
        };
        Ok(Some(pointer.import_generation_id))
    }

    /// Repair the narrow crash window between immutable generation/index
    /// installation and ready-pointer replacement. The acceptance sequence
    /// is allocated durably first, so choosing the greatest surviving
    /// sequence cannot make an older generation current.
    pub fn repair_current_ready_provenance_import_id(
        &self,
        project_id: &str,
    ) -> Result<Option<String>> {
        let _guard = self.lock_mutation()?;
        if let Some(pointer) = self.load_provenance_ready_pointer(project_id)? {
            return Ok(Some(pointer.import_generation_id));
        }
        let mut candidate: Option<StoredProvenanceImportV1> = None;
        for entry in fs::read_dir(self.root.join("provenance-imports/generation-index"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.file_type()?.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            let source = self.load_provenance_generation(id)?;
            if source.project_id != project_id
                || !matches!(
                    source.state,
                    ProvenanceImportStateV1::Ready
                        | ProvenanceImportStateV1::Importing
                        | ProvenanceImportStateV1::Active
                        | ProvenanceImportStateV1::Quarantined
                )
            {
                continue;
            }
            if candidate
                .as_ref()
                .is_none_or(|current| source.accepted_sequence > current.accepted_sequence)
            {
                candidate = Some(source);
            }
        }
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        self.advance_provenance_ready_pointer(&candidate)?;
        Ok(Some(candidate.import_generation_id))
    }

    pub fn verified_provenance_import(
        &self,
        import_generation_id: &str,
    ) -> Result<VerifiedProvenanceImportV1> {
        let source = self.load_provenance_generation(import_generation_id)?;
        if !matches!(
            source.state,
            ProvenanceImportStateV1::Ready
                | ProvenanceImportStateV1::Importing
                | ProvenanceImportStateV1::Active
                | ProvenanceImportStateV1::Superseded
        ) {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_provenance_generation_manifest(import_generation_id)?;
        validate_provenance_manifest(
            &source.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        let source_evidence = sha256(&serde_json::to_vec(&(
            &source.import_generation_id,
            &source.producer_id,
            &source.project_id,
            &source.descriptor,
            &manifest,
        ))?);
        Ok(VerifiedProvenanceImportV1 {
            import_generation_id: source.import_generation_id,
            producer_id: source.producer_id,
            project_id: source.project_id,
            scope: source.descriptor.scope,
            notes_ref: source.descriptor.notes_ref,
            notes_tip: source.descriptor.notes_tip,
            manifest_sha256: source.descriptor.manifest_sha256,
            source_evidence,
            document_count: source.descriptor.document_count,
        })
    }

    pub fn visit_verified_provenance_documents(
        &self,
        source: &VerifiedProvenanceImportV1,
        mut visit: impl FnMut(VerifiedProvenanceDocumentV1) -> Result<()>,
    ) -> Result<()> {
        if &self.verified_provenance_import(&source.import_generation_id)? != source {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_provenance_generation_manifest(&source.import_generation_id)?;
        for entry in manifest {
            let bytes = self
                .read_provenance_document_bytes(
                    &entry.document_sha256,
                    entry.encoded_bytes as usize,
                )?
                .ok_or(StoreRequestError::InvalidState)?;
            let document = String::from_utf8(bytes).map_err(|_| StoreRequestError::InvalidInput)?;
            visit(VerifiedProvenanceDocumentV1 {
                note_commit: entry.note_commit,
                document_ordinal: entry.document_ordinal,
                document_sha256: entry.document_sha256,
                document,
            })?;
        }
        Ok(())
    }

    pub fn transition_provenance_import(
        &self,
        import_generation_id: &str,
        next: ProvenanceImportStateV1,
        edges_imported: u64,
        diagnostic: Option<&str>,
    ) -> Result<StoredProvenanceImportV1> {
        let _guard = self.lock_mutation()?;
        let mut source = self.load_provenance_generation(import_generation_id)?;
        let allowed = matches!(
            (source.state, next),
            (
                ProvenanceImportStateV1::Ready,
                ProvenanceImportStateV1::Importing
            ) | (
                ProvenanceImportStateV1::Importing,
                ProvenanceImportStateV1::Importing
            ) | (
                ProvenanceImportStateV1::Importing,
                ProvenanceImportStateV1::Active
            ) | (
                ProvenanceImportStateV1::Ready,
                ProvenanceImportStateV1::Quarantined
            ) | (
                ProvenanceImportStateV1::Importing,
                ProvenanceImportStateV1::Quarantined
            ) | (
                ProvenanceImportStateV1::Active,
                ProvenanceImportStateV1::Superseded
            ) | (
                ProvenanceImportStateV1::Active,
                ProvenanceImportStateV1::Active
            ) | (
                ProvenanceImportStateV1::Ready,
                ProvenanceImportStateV1::Superseded
            ) | (
                ProvenanceImportStateV1::Importing,
                ProvenanceImportStateV1::Superseded
            ) | (
                ProvenanceImportStateV1::Superseded,
                ProvenanceImportStateV1::Superseded
            )
        );
        if !allowed {
            bail!(StoreRequestError::InvalidState);
        }
        source.state = next;
        source.edges_imported = edges_imported;
        source.diagnostic = diagnostic.map(|value| value.chars().take(512).collect());
        self.write_provenance_source(&source)?;
        Ok(source)
    }

    /// Atomically decide whether a fully published import is still the
    /// newest accepted note generation for its project. Older work may
    /// finish its idempotent sidecar append, but it can never become Active
    /// after a newer finalize advanced the durable ready pointer.
    pub fn settle_provenance_import(
        &self,
        import_generation_id: &str,
        edges_imported: u64,
    ) -> Result<StoredProvenanceImportV1> {
        let _guard = self.lock_mutation()?;
        let mut source = self.load_provenance_generation(import_generation_id)?;
        if !matches!(
            source.state,
            ProvenanceImportStateV1::Ready
                | ProvenanceImportStateV1::Importing
                | ProvenanceImportStateV1::Active
                | ProvenanceImportStateV1::Superseded
        ) {
            bail!(StoreRequestError::InvalidState);
        }
        let current = self.load_provenance_ready_pointer(&source.project_id)?;
        let is_current = current
            .as_ref()
            .is_some_and(|pointer| pointer.import_generation_id == import_generation_id);
        if is_current {
            self.supersede_other_active_provenance_imports_locked(
                &source.project_id,
                import_generation_id,
            )?;
            source.state = ProvenanceImportStateV1::Active;
            source.diagnostic = None;
        } else {
            source.state = ProvenanceImportStateV1::Superseded;
            source.diagnostic = Some(
                current
                    .map(|pointer| {
                        format!(
                            "superseded by accepted provenance import {}",
                            pointer.import_generation_id
                        )
                    })
                    .unwrap_or_else(|| "provenance ready pointer is absent".to_string()),
            );
        }
        source.edges_imported = edges_imported;
        self.write_provenance_source(&source)?;
        Ok(source)
    }

    pub fn supersede_other_active_provenance_imports(
        &self,
        project_id: &str,
        active_import_generation_id: &str,
    ) -> Result<u64> {
        validate_receipt_authority("supersede", project_id)?;
        validate_provenance_generation_id(active_import_generation_id)?;
        let _guard = self.lock_mutation()?;
        self.supersede_other_active_provenance_imports_locked(
            project_id,
            active_import_generation_id,
        )
    }

    fn supersede_other_active_provenance_imports_locked(
        &self,
        project_id: &str,
        active_import_generation_id: &str,
    ) -> Result<u64> {
        let mut superseded = 0_u64;
        for entry in fs::read_dir(self.root.join("provenance-imports/generation-index"))? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(generation_id) = name.strip_suffix(".json") else {
                continue;
            };
            let index = self.load_provenance_generation_index(generation_id)?;
            if index.project_id != project_id || generation_id == active_import_generation_id {
                continue;
            }
            let mut source = self.load_provenance_generation(generation_id)?;
            if source.state != ProvenanceImportStateV1::Active {
                continue;
            }
            source.state = ProvenanceImportStateV1::Superseded;
            source.diagnostic = Some(format!(
                "superseded by provenance import {active_import_generation_id}"
            ));
            self.write_provenance_source(&source)?;
            superseded = superseded.saturating_add(1);
        }
        Ok(superseded)
    }

    pub fn read_provenance_import_journal(
        &self,
        project_id: &str,
    ) -> Result<Option<ProvenanceImportJournalV1>> {
        validate_receipt_authority("read", project_id)?;
        let journal = read_json::<ProvenanceImportJournalV1>(
            &self.root.join("provenance-imports/journals"),
            &format!("{project_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "provenance import journal",
        )?;
        if let Some(journal) = journal.as_ref() {
            journal.validate()?;
            if journal.project_id != project_id {
                bail!(StoreRequestError::InvalidState);
            }
        }
        Ok(journal)
    }

    pub fn save_provenance_import_journal(
        &self,
        journal: ProvenanceImportJournalV1,
    ) -> Result<ProvenanceImportJournalV1> {
        let _guard = self.lock_mutation()?;
        let journal = journal.seal()?;
        journal.validate()?;
        let previous = self.read_provenance_import_journal(&journal.project_id)?;
        match previous.as_ref() {
            None if journal.stage != ProvenanceImportStageV1::Prepared => {
                bail!(StoreRequestError::InvalidState)
            }
            None => {}
            Some(previous)
                if previous.stage.terminal()
                    && journal.stage == ProvenanceImportStageV1::Prepared => {}
            Some(previous) => {
                if previous.immutable_projection()? != journal.immutable_projection()? {
                    bail!(StoreRequestError::InvalidState);
                }
                if previous.stage.terminal() && previous.stage != journal.stage {
                    bail!(StoreRequestError::InvalidState);
                }
                if !matches!(
                    journal.stage,
                    ProvenanceImportStageV1::Superseded | ProvenanceImportStageV1::Quarantined
                ) {
                    let Some(previous_ordinal) = previous.stage.ordinal() else {
                        bail!(StoreRequestError::InvalidState);
                    };
                    let Some(next_ordinal) = journal.stage.ordinal() else {
                        bail!(StoreRequestError::InvalidState);
                    };
                    if next_ordinal < previous_ordinal
                        || next_ordinal > previous_ordinal.saturating_add(1)
                    {
                        bail!(StoreRequestError::InvalidState);
                    }
                }
            }
        }
        if journal.stage == ProvenanceImportStageV1::Prepared {
            let verified = self.verified_provenance_import(&journal.import_generation_id)?;
            if verified.producer_id != journal.producer_id
                || verified.project_id != journal.project_id
                || verified.source_evidence != journal.source_evidence
            {
                bail!(StoreRequestError::InvalidState);
            }
        }
        let directory =
            NofollowDirectory::open_existing(&self.root.join("provenance-imports/journals"))?
                .ok_or(StoreRequestError::InvalidState)?;
        write_json(
            &directory,
            &format!("{}.json", journal.project_id),
            &journal,
        )?;
        Ok(journal)
    }

    pub fn list_provenance_import_journals(&self) -> Result<Vec<ProvenanceImportJournalV1>> {
        let root = self.root.join("provenance-imports/journals");
        let mut journals = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(journal) = read_json::<ProvenanceImportJournalV1>(
                &root,
                &name,
                MAX_GENERATION_RECORD_BYTES,
                "provenance import journal",
            )?
            else {
                continue;
            };
            journal.validate()?;
            if name != format!("{}.json", journal.project_id) {
                bail!(StoreRequestError::InvalidState);
            }
            journals.push(journal);
        }
        journals.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(journals)
    }

    /// Reclaim only state that durable store evidence proves unreferenced.
    /// Future materializers pass their pinned source-generation ids here;
    /// GH-B has no external pins, so current/retained ready sources are the
    /// complete root set.
    pub fn maintain(
        &self,
        protected_generation_ids: &BTreeSet<String>,
    ) -> Result<MaintenanceReport> {
        let _guard = self.lock_mutation()?;
        self.maintain_locked(protected_generation_ids, now_unix_secs())
    }

    pub fn begin_history_upload(
        &self,
        producer_id: &str,
        repo_history_id: &RepoHistoryId,
        primary_namespace: &CommitNamespace,
        descriptor: GitHistoryDescriptorV1,
    ) -> Result<BeginGitHistoryUploadResponseV1> {
        let limits = self.current_limits()?;
        descriptor.validate_header(limits.contract)?;
        let source_generation_id = history_source_generation_id(
            producer_id,
            repo_history_id,
            primary_namespace,
            &descriptor,
        )?;
        let _guard = self.lock_mutation()?;
        let producer_dir = self.producer_upload_dir(producer_id)?;
        let mut open_uploads = 0_usize;
        for entry in read_directories(&producer_dir)? {
            let record = read_json::<HistoryUploadRecordV1>(
                &entry,
                "upload.json",
                MAX_UPLOAD_RECORD_BYTES,
                "Git-history upload record",
            )?;
            let Some(record) = record else { continue };
            if record.producer_id == producer_id
                && record.repo_history_id == *repo_history_id
                && record.primary_namespace == *primary_namespace
                && record.descriptor == descriptor
                && !matches!(
                    record.state,
                    GitHistorySourceStateV1::Ready
                        | GitHistorySourceStateV1::Active
                        | GitHistorySourceStateV1::Superseded
                        | GitHistorySourceStateV1::Failed
                )
            {
                return Ok(begin_response(record.upload_id));
            }
            if !matches!(
                record.state,
                GitHistorySourceStateV1::Ready
                    | GitHistorySourceStateV1::Active
                    | GitHistorySourceStateV1::Superseded
                    | GitHistorySourceStateV1::Failed
            ) {
                open_uploads += 1;
            }
        }
        if open_uploads >= limits.max_open_uploads_per_producer {
            bail!(StoreRequestError::TooManyOpenUploads);
        }

        let upload_id = Uuid::new_v4().simple().to_string();
        let upload_dir = NofollowDirectory::open_or_create(&producer_dir.join(&upload_id))?;
        NofollowDirectory::open_or_create(&producer_dir.join(&upload_id).join("pages"))?;
        write_json(
            &upload_dir,
            "upload.json",
            &HistoryUploadRecordV1 {
                version: STORE_VERSION,
                upload_id: upload_id.clone(),
                producer_id: producer_id.to_string(),
                repo_history_id: repo_history_id.clone(),
                primary_namespace: primary_namespace.clone(),
                descriptor,
                state: GitHistorySourceStateV1::ReceivingManifest,
                next_page: 0,
                page_digests: BTreeMap::new(),
                source_generation_id: Some(source_generation_id),
                updated_unix_secs: now_unix_secs(),
            },
        )?;
        Ok(begin_response(upload_id))
    }

    pub fn put_history_manifest_page(
        &self,
        producer_id: &str,
        upload_id: &str,
        page: u32,
        body: &GitHistoryManifestPageV1,
    ) -> Result<()> {
        if body.entries.is_empty() || body.entries.len() > MAX_HISTORY_MANIFEST_PAGE_ENTRIES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let raw = serde_json::to_vec(body)?;
        if raw.len() > MAX_HISTORY_MANIFEST_PAGE_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let digest = sha256(&raw);
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let mut record = self.load_upload(&upload_dir, producer_id, upload_id)?;
        if record.state != GitHistorySourceStateV1::ReceivingManifest {
            bail!(StoreRequestError::InvalidState);
        }
        if page < record.next_page {
            if record
                .page_digests
                .get(&page)
                .is_some_and(|prior| prior == &digest)
            {
                return Ok(());
            }
            bail!(StoreRequestError::InvalidInput);
        }
        if page != record.next_page {
            bail!(StoreRequestError::InvalidInput);
        }
        let pages = NofollowDirectory::open_existing(&upload_dir.join("pages"))?
            .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
        pages.atomic_replace(&format!("{page:08}.json"), &raw)?;
        record.page_digests.insert(page, digest);
        record.next_page = record
            .next_page
            .checked_add(1)
            .ok_or(StoreRequestError::LimitExceeded)?;
        record.updated_unix_secs = now_unix_secs();
        let upload_directory = NofollowDirectory::open_existing(&upload_dir)?
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        write_json(&upload_directory, "upload.json", &record)
    }

    pub fn complete_history_manifest(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<MissingHistoryRecordsPageV1> {
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let directory = NofollowDirectory::open_existing(&upload_dir)?
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        let mut record = self.load_upload(&upload_dir, producer_id, upload_id)?;
        if record.state == GitHistorySourceStateV1::MissingRecords {
            return self.missing_history_records_locked(&record, None);
        }
        if record.state != GitHistorySourceStateV1::ReceivingManifest || record.next_page == 0 {
            bail!(StoreRequestError::InvalidState);
        }
        let pages = NofollowDirectory::open_existing(&upload_dir.join("pages"))?
            .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
        let mut manifest = Vec::new();
        for page in 0..record.next_page {
            let body = read_json::<GitHistoryManifestPageV1>(
                &upload_dir.join("pages"),
                &format!("{page:08}.json"),
                MAX_HISTORY_MANIFEST_PAGE_BYTES,
                "Git-history manifest page",
            )?
            .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
            manifest.extend(body.entries);
            if manifest.len() as u64 > record.descriptor.fragment_count {
                bail!(StoreRequestError::LimitExceeded);
            }
        }
        validate_history_manifest(
            &record.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        let raw = serde_json::to_vec(&manifest)?;
        if raw.len() > MAX_MANIFEST_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        directory.atomic_replace("manifest.json", &raw)?;
        pages.ensure_still_current()?;
        record.state = GitHistorySourceStateV1::MissingRecords;
        record.updated_unix_secs = now_unix_secs();
        write_json(&directory, "upload.json", &record)?;
        self.missing_history_records_locked(&record, None)
    }

    pub fn missing_history_records(
        &self,
        producer_id: &str,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingHistoryRecordsPageV1> {
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let record = self.load_upload(&upload_dir, producer_id, upload_id)?;
        if record.state != GitHistorySourceStateV1::MissingRecords {
            bail!(StoreRequestError::InvalidState);
        }
        self.missing_history_records_locked(&record, cursor)
    }

    pub fn expected_history_record_size(
        &self,
        producer_id: &str,
        upload_id: &str,
        hash: &str,
    ) -> Result<u64> {
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let record = self.load_upload(&upload_dir, producer_id, upload_id)?;
        if record.state != GitHistorySourceStateV1::MissingRecords {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_manifest(&upload_dir)?;
        manifest
            .iter()
            .find(|entry| entry.content_sha256 == hash)
            .map(|entry| entry.encoded_bytes)
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))
    }

    pub fn install_history_record(
        &self,
        producer_id: &str,
        upload_id: &str,
        hash: &str,
        expected_size: u64,
        mut reader: impl Read,
    ) -> Result<()> {
        if expected_size > MAX_HISTORY_RECORD_BYTES {
            bail!(StoreRequestError::LimitExceeded);
        }
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let mut record = self.load_upload(&upload_dir, producer_id, upload_id)?;
        if record.state != GitHistorySourceStateV1::MissingRecords {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_manifest(&upload_dir)?;
        let entry = manifest
            .iter()
            .find(|entry| entry.content_sha256 == hash)
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if entry.encoded_bytes != expected_size {
            bail!(StoreRequestError::InvalidInput);
        }
        let mut bytes = Vec::with_capacity(expected_size as usize);
        reader
            .by_ref()
            .take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size || sha256(&bytes) != hash {
            bail!(StoreRequestError::InvalidInput);
        }
        bbox_git_source::decode_history_fragment(&bytes)?;
        self.install_record_bytes(hash, &bytes)?;
        record.updated_unix_secs = now_unix_secs();
        let upload_directory = NofollowDirectory::open_existing(&upload_dir)?
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        write_json(&upload_directory, "upload.json", &record)
    }

    pub fn finalize_history_upload(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<FinalizeGitHistoryUploadResponseV1> {
        let _guard = self.lock_mutation()?;
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let upload_directory = NofollowDirectory::open_existing(&upload_dir)?
            .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        let mut upload = self.load_upload(&upload_dir, producer_id, upload_id)?;
        let source_generation_id = upload
            .source_generation_id
            .clone()
            .ok_or(StoreRequestError::InvalidState)?;
        if upload.state == GitHistorySourceStateV1::Ready {
            return Ok(finalize_response(source_generation_id));
        }
        if upload.state != GitHistorySourceStateV1::MissingRecords {
            bail!(StoreRequestError::InvalidState);
        }
        let manifest = self.load_manifest(&upload_dir)?;
        let mut verifier = HistorySourceVerifier::new(
            &upload.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        for entry in &manifest {
            let bytes = self
                .read_record_bytes(&entry.content_sha256, entry.encoded_bytes as usize)?
                .ok_or(StoreRequestError::InvalidState)?;
            verifier.push_encoded(&bytes)?;
        }
        verifier.finish()?;

        let generation_path =
            self.generation_dir(&upload.repo_history_id, &source_generation_id)?;
        let generation_dir = NofollowDirectory::open_or_create(&generation_path)?;
        let stored = StoredHistorySourceV1 {
            version: STORE_VERSION,
            source_generation_id: source_generation_id.clone(),
            producer_id: producer_id.to_string(),
            repo_history_id: upload.repo_history_id.clone(),
            primary_namespace: upload.primary_namespace.clone(),
            descriptor: upload.descriptor.clone(),
            state: GitHistorySourceStateV1::Ready,
            created_unix_secs: now_unix_secs(),
            diagnostic: None,
        };
        install_immutable_json(&generation_dir, "descriptor.json", &upload.descriptor)?;
        install_immutable_json(&generation_dir, "manifest.json", &manifest)?;
        install_immutable_json(&generation_dir, "source.json", &stored)?;

        let index_dir = NofollowDirectory::open_existing(&self.root.join("generation-index"))?
            .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
        write_json(
            &index_dir,
            &format!("{source_generation_id}.json"),
            &GenerationIndexV1 {
                version: STORE_VERSION,
                source_generation_id: source_generation_id.clone(),
                producer_id: producer_id.to_string(),
                repo_history_id: upload.repo_history_id.clone(),
            },
        )?;
        let history_root =
            NofollowDirectory::open_existing(&self.repo_history_root(&upload.repo_history_id)?)?
                .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
        write_json(
            &history_root,
            "current-ready.json",
            &ReadyPointerV1 {
                version: STORE_VERSION,
                source_generation_id: source_generation_id.clone(),
                producer_id: producer_id.to_string(),
                repo_head: upload.descriptor.repo_head.clone(),
            },
        )?;
        upload.state = GitHistorySourceStateV1::Ready;
        upload.updated_unix_secs = now_unix_secs();
        write_json(&upload_directory, "upload.json", &upload)?;
        Ok(finalize_response(source_generation_id))
    }

    pub fn history_status(
        &self,
        producer_id: &str,
        source_generation_id: &str,
    ) -> Result<GitHistorySourceStatusV1> {
        validate_generation_id(source_generation_id)?;
        let index_dir = self.root.join("generation-index");
        let index = read_json::<GenerationIndexV1>(
            &index_dir,
            &format!("{source_generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history generation index",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if index.producer_id != producer_id || index.source_generation_id != source_generation_id {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_generation(&index.repo_history_id, source_generation_id)?;
        Ok(GitHistorySourceStatusV1 {
            source_generation_id: source.source_generation_id,
            state: source.state,
            commit_count: source.descriptor.commit_count,
            logical_bytes: source.descriptor.logical_bytes,
            diagnostic: source.diagnostic,
        })
    }

    pub fn probe_ready_history(
        &self,
        producer_id: &str,
        repo_history_id: &RepoHistoryId,
        repo_head: &str,
        object_format: bbox_git_source::GitObjectFormatV1,
    ) -> Result<Option<StoredHistorySourceV1>> {
        let history_root = self.repo_history_root(repo_history_id)?;
        let Some(pointer) = read_json::<ReadyPointerV1>(
            &history_root,
            "current-ready.json",
            MAX_GENERATION_RECORD_BYTES,
            "Git-history ready pointer",
        )?
        else {
            return Ok(None);
        };
        if pointer.producer_id != producer_id || pointer.repo_head != repo_head {
            return Ok(None);
        }
        let source = self.load_generation(repo_history_id, &pointer.source_generation_id)?;
        if source.descriptor.object_format != object_format
            || source.descriptor.schema_version != bbox_git_source::SCHEMA_VERSION
        {
            return Ok(None);
        }
        Ok(Some(source))
    }

    pub fn upload_authority(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<HistoryTransportAuthorityV1> {
        let upload_dir = self.upload_dir(producer_id, upload_id)?;
        let upload = self.load_upload(&upload_dir, producer_id, upload_id)?;
        Ok(HistoryTransportAuthorityV1 {
            scope: upload.descriptor.scope,
            repo_history_id: upload.repo_history_id,
            primary_namespace: upload.primary_namespace,
        })
    }

    pub fn generation_authority(
        &self,
        producer_id: &str,
        source_generation_id: &str,
    ) -> Result<HistoryTransportAuthorityV1> {
        validate_generation_id(source_generation_id)?;
        let index = read_json::<GenerationIndexV1>(
            &self.root.join("generation-index"),
            &format!("{source_generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history generation index",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if index.producer_id != producer_id {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_generation(&index.repo_history_id, source_generation_id)?;
        Ok(HistoryTransportAuthorityV1 {
            scope: source.descriptor.scope,
            repo_history_id: source.repo_history_id,
            primary_namespace: source.primary_namespace,
        })
    }

    pub fn generation_authority_for_any_producer(
        &self,
        source_generation_id: &str,
    ) -> Result<StoredHistorySourceAuthorityV1> {
        validate_generation_id(source_generation_id)?;
        let index = read_json::<GenerationIndexV1>(
            &self.root.join("generation-index"),
            &format!("{source_generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history generation index",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if index.source_generation_id != source_generation_id {
            bail!(StoreRequestError::NotFound);
        }
        Ok(StoredHistorySourceAuthorityV1 {
            producer_id: index.producer_id,
            repo_history_id: index.repo_history_id,
        })
    }

    /// Reverify one immutable accepted source and return its path-free builder
    /// handoff. Verification reads every manifest record and re-runs graph
    /// closure; a successful finalize from an earlier process is evidence,
    /// never a substitute for checking the bytes this process will consume.
    pub fn verified_history_source(
        &self,
        producer_id: &str,
        source_generation_id: &str,
    ) -> Result<VerifiedGitHistorySourceV1> {
        let (verified, manifest) =
            self.verified_history_source_metadata(producer_id, source_generation_id)?;
        let source = self.load_generation(&verified.repo_history_id, source_generation_id)?;
        let mut verifier = HistorySourceVerifier::new(
            &source.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        for entry in &manifest {
            let bytes = self
                .read_record_bytes(&entry.content_sha256, entry.encoded_bytes as usize)?
                .ok_or(StoreRequestError::InvalidState)?;
            verifier.push_encoded(&bytes)?;
        }
        verifier.finish()?;
        Ok(verified)
    }

    /// Rebind a verified handoff to the immutable descriptor + manifest
    /// without rereading the source-sized CAS record set. Every consuming
    /// pass still hashes and decodes each record it reads; this bounded seam
    /// is for journal pinning and for avoiding a redundant full graph walk
    /// before each such pass.
    fn verified_history_source_metadata(
        &self,
        producer_id: &str,
        source_generation_id: &str,
    ) -> Result<(VerifiedGitHistorySourceV1, Vec<GitHistoryManifestEntryV1>)> {
        validate_generation_id(source_generation_id)?;
        let index = read_json::<GenerationIndexV1>(
            &self.root.join("generation-index"),
            &format!("{source_generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history generation index",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if index.producer_id != producer_id || index.source_generation_id != source_generation_id {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_generation(&index.repo_history_id, source_generation_id)?;
        if source.producer_id != producer_id
            || source.source_generation_id != source_generation_id
            || matches!(
                source.state,
                GitHistorySourceStateV1::ReceivingManifest
                    | GitHistorySourceStateV1::MissingRecords
                    | GitHistorySourceStateV1::Failed
            )
        {
            bail!(StoreRequestError::InvalidState);
        }
        let generation_dir = self.generation_dir(&source.repo_history_id, source_generation_id)?;
        let manifest: Vec<GitHistoryManifestEntryV1> = read_json(
            &generation_dir,
            "manifest.json",
            MAX_MANIFEST_BYTES,
            "Git-history generation manifest",
        )?
        .ok_or(StoreRequestError::InvalidState)?;
        validate_history_manifest(
            &source.descriptor,
            &manifest,
            self.current_limits()?.contract,
        )?;
        let source_evidence = sha256(&serde_json::to_vec(&(
            &source.source_generation_id,
            &source.producer_id,
            &source.repo_history_id,
            &source.primary_namespace,
            &source.descriptor,
            &manifest,
        ))?);
        Ok((
            VerifiedGitHistorySourceV1 {
                source_generation_id: source.source_generation_id,
                producer_id: source.producer_id,
                authority_scope: source.descriptor.scope.clone(),
                repo_history_id: source.repo_history_id,
                primary_namespace: source.primary_namespace,
                repo_head: source.descriptor.repo_head,
                manifest_sha256: source.descriptor.manifest_sha256,
                source_evidence,
                commit_count: source.descriptor.commit_count,
            },
            manifest,
        ))
    }

    /// Re-prove the bounded immutable metadata pinned by an activation
    /// journal without rereading the source-sized record set. Later recovery
    /// stages use this before trusting already-published P3/index/sidecar
    /// commitments; any repair that must consume records still goes through
    /// [`Self::verified_history_source`] and the per-record visitor.
    pub fn verify_activation_source_pin(
        &self,
        journal: &HistoryActivationJournalV1,
    ) -> Result<VerifiedGitHistorySourceV1> {
        let (source, _) = self.verified_history_source_metadata(
            &journal.producer_id,
            &journal.source_generation_id,
        )?;
        if source.repo_history_id != journal.repo_history_id
            || source.source_evidence != journal.source_evidence
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(source)
    }

    /// Visit a reverified source one reconstructed commit at a time.
    ///
    /// The manifest is ordered by `(commit_oid, fragment_index)`, so only one
    /// commit's changed-path set is resident. The source metadata and every
    /// record hash are rechecked against the verified handoff before the
    /// visitor receives anything.
    pub fn visit_verified_history_commits(
        &self,
        source: &VerifiedGitHistorySourceV1,
        mut visit: impl FnMut(VerifiedGitHistoryCommitV1) -> Result<()>,
    ) -> Result<()> {
        let (current, manifest) = self
            .verified_history_source_metadata(&source.producer_id, &source.source_generation_id)?;
        if &current != source {
            bail!(StoreRequestError::InvalidState);
        }

        let mut active_oid: Option<String> = None;
        let mut header: Option<bbox_git_source::GitHistoryCommitHeaderV1> = None;
        let mut changed_paths = Vec::new();
        let mut emitted = 0_u64;
        let flush = |oid: &mut Option<String>,
                     header: &mut Option<bbox_git_source::GitHistoryCommitHeaderV1>,
                     paths: &mut Vec<String>,
                     emitted: &mut u64,
                     visit: &mut dyn FnMut(VerifiedGitHistoryCommitV1) -> Result<()>|
         -> Result<()> {
            let Some(oid) = oid.take() else {
                return Ok(());
            };
            let header = header.take().ok_or(StoreRequestError::InvalidState)?;
            visit(VerifiedGitHistoryCommitV1 {
                commit: GitCommit {
                    sha: oid,
                    parent_shas: header.parent_oids,
                    author_name: header.author_name,
                    author_email: header.author_email,
                    message: header.message,
                },
                changed_paths: std::mem::take(paths),
            })?;
            *emitted = emitted.saturating_add(1);
            Ok(())
        };

        for entry in &manifest {
            if active_oid
                .as_deref()
                .is_some_and(|active| active != entry.commit_oid)
            {
                flush(
                    &mut active_oid,
                    &mut header,
                    &mut changed_paths,
                    &mut emitted,
                    &mut visit,
                )?;
            }
            let bytes = self
                .read_record_bytes(&entry.content_sha256, entry.encoded_bytes as usize)?
                .ok_or(StoreRequestError::InvalidState)?;
            let fragment = bbox_git_source::decode_history_fragment(&bytes)?;
            if active_oid.is_none() {
                active_oid = Some(fragment.commit_oid.clone());
            }
            if let Some(fragment_header) = fragment.header {
                if header.replace(fragment_header).is_some() {
                    bail!(StoreRequestError::InvalidState);
                }
            }
            changed_paths.extend(fragment.changed_paths);
        }
        flush(
            &mut active_oid,
            &mut header,
            &mut changed_paths,
            &mut emitted,
            &mut visit,
        )?;
        if emitted != source.commit_count {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(())
    }

    pub fn read_activation_journal(
        &self,
        repo_history_id: &RepoHistoryId,
    ) -> Result<Option<HistoryActivationJournalV1>> {
        let journal = read_json::<HistoryActivationJournalV1>(
            &self.root.join("activations"),
            &format!("{}.json", repo_history_id.as_str()),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history activation journal",
        )?;
        if let Some(journal) = journal.as_ref() {
            journal.validate()?;
            if &journal.repo_history_id != repo_history_id {
                bail!(StoreRequestError::InvalidState);
            }
        }
        Ok(journal)
    }

    /// Install `Prepared` or monotonically advance one existing activation.
    /// Immutable plan fields cannot drift after preparation; recovery changes
    /// only progress evidence and exact publication commitments.
    pub fn save_activation_journal(
        &self,
        journal: HistoryActivationJournalV1,
    ) -> Result<HistoryActivationJournalV1> {
        let _guard = self.lock_mutation()?;
        let journal = journal.seal()?;
        journal.validate()?;
        let previous = self.read_activation_journal(&journal.repo_history_id)?;
        match previous.as_ref() {
            None if journal.stage != HistoryActivationStageV1::Prepared => {
                bail!(StoreRequestError::InvalidState);
            }
            None => {}
            Some(previous)
                if previous.stage.terminal()
                    && journal.stage == HistoryActivationStageV1::Prepared => {}
            Some(previous) => {
                if previous.immutable_projection()? != journal.immutable_projection()? {
                    bail!(StoreRequestError::InvalidState);
                }
                if previous.stage.terminal() && previous.stage != journal.stage {
                    bail!(StoreRequestError::InvalidState);
                }
                if journal.stage != HistoryActivationStageV1::Superseded {
                    let Some(previous_ordinal) = previous.stage.ordinal() else {
                        bail!(StoreRequestError::InvalidState);
                    };
                    let Some(next_ordinal) = journal.stage.ordinal() else {
                        bail!(StoreRequestError::InvalidState);
                    };
                    if next_ordinal < previous_ordinal
                        || next_ordinal > previous_ordinal.saturating_add(1)
                    {
                        bail!(StoreRequestError::InvalidState);
                    }
                }
            }
        }
        if journal.stage == HistoryActivationStageV1::Prepared {
            let (verified, _) = self.verified_history_source_metadata(
                &journal.producer_id,
                &journal.source_generation_id,
            )?;
            if verified.repo_history_id != journal.repo_history_id
                || verified.source_evidence != journal.source_evidence
            {
                bail!(StoreRequestError::InvalidState);
            }
        }
        let activations = NofollowDirectory::open_existing(&self.root.join("activations"))?
            .ok_or(StoreRequestError::InvalidState)?;
        write_json(
            &activations,
            &format!("{}.json", journal.repo_history_id.as_str()),
            &journal,
        )?;
        Ok(journal)
    }

    pub fn list_activation_journals(&self) -> Result<Vec<HistoryActivationJournalV1>> {
        let root = self.root.join("activations");
        let mut journals = Vec::new();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            let Some(journal) = read_json::<HistoryActivationJournalV1>(
                &root,
                &name,
                MAX_GENERATION_RECORD_BYTES,
                "Git-history activation journal",
            )?
            else {
                continue;
            };
            journal.validate()?;
            if name != format!("{}.json", journal.repo_history_id.as_str()) {
                bail!(StoreRequestError::InvalidState);
            }
            journals.push(journal);
        }
        journals.sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
        Ok(journals)
    }

    pub fn activation_source_roots(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .list_activation_journals()?
            .into_iter()
            .map(|journal| journal.source_generation_id)
            .collect())
    }

    pub fn current_ready_source_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for repo_dir in read_directories(&self.root.join("repos"))? {
            let history_dir = repo_dir.join("history");
            if NofollowDirectory::open_existing(&history_dir)?.is_none() {
                continue;
            }
            if let Some(pointer) = read_json::<ReadyPointerV1>(
                &history_dir,
                "current-ready.json",
                MAX_GENERATION_RECORD_BYTES,
                "Git-history ready pointer",
            )? {
                validate_generation_id(&pointer.source_generation_id)?;
                ids.push(pointer.source_generation_id);
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    pub fn current_ready_source_id(
        &self,
        repo_history_id: &RepoHistoryId,
    ) -> Result<Option<String>> {
        let history_dir = self.repo_history_root(repo_history_id)?;
        let Some(pointer) = read_json::<ReadyPointerV1>(
            &history_dir,
            "current-ready.json",
            MAX_GENERATION_RECORD_BYTES,
            "Git-history ready pointer",
        )?
        else {
            return Ok(None);
        };
        validate_generation_id(&pointer.source_generation_id)?;
        let source = self.load_generation(repo_history_id, &pointer.source_generation_id)?;
        if source.producer_id != pointer.producer_id
            || source.descriptor.repo_head != pointer.repo_head
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(Some(pointer.source_generation_id))
    }

    pub fn set_history_source_state(
        &self,
        producer_id: &str,
        source_generation_id: &str,
        next: GitHistorySourceStateV1,
        diagnostic: Option<String>,
    ) -> Result<StoredHistorySourceV1> {
        let _guard = self.lock_mutation()?;
        let authority = self.generation_authority(producer_id, source_generation_id)?;
        let mut source = self.load_generation(&authority.repo_history_id, source_generation_id)?;
        let allowed = source.state == next
            || matches!(
                (source.state, next),
                (
                    GitHistorySourceStateV1::Ready,
                    GitHistorySourceStateV1::Materializing
                ) | (
                    GitHistorySourceStateV1::Active,
                    GitHistorySourceStateV1::Materializing
                ) | (
                    GitHistorySourceStateV1::Superseded,
                    GitHistorySourceStateV1::Materializing
                ) | (
                    GitHistorySourceStateV1::Materializing,
                    GitHistorySourceStateV1::Publishing
                ) | (
                    GitHistorySourceStateV1::Publishing,
                    GitHistorySourceStateV1::Active
                ) | (
                    GitHistorySourceStateV1::Ready,
                    GitHistorySourceStateV1::Superseded
                ) | (
                    GitHistorySourceStateV1::Materializing,
                    GitHistorySourceStateV1::Superseded
                ) | (
                    GitHistorySourceStateV1::Publishing,
                    GitHistorySourceStateV1::Superseded
                ) | (
                    GitHistorySourceStateV1::Active,
                    GitHistorySourceStateV1::Superseded
                ) | (
                    GitHistorySourceStateV1::Ready,
                    GitHistorySourceStateV1::Failed
                ) | (
                    GitHistorySourceStateV1::Materializing,
                    GitHistorySourceStateV1::Failed
                ) | (
                    GitHistorySourceStateV1::Publishing,
                    GitHistorySourceStateV1::Failed
                )
            );
        if !allowed {
            bail!(StoreRequestError::InvalidState);
        }
        source.state = next;
        source.diagnostic = diagnostic.map(|value| value.chars().take(512).collect());
        let generation_dir = NofollowDirectory::open_existing(
            &self.generation_dir(&authority.repo_history_id, source_generation_id)?,
        )?
        .ok_or(StoreRequestError::NotFound)?;
        write_json(&generation_dir, "source.json", &source)?;
        Ok(source)
    }

    /// Retire older active sources after the selected activation is durable.
    ///
    /// This is deliberately state-selective: a newer `Ready` upload may have
    /// arrived while the current activation was publishing and remains
    /// eligible for the next activation. Only obsolete `Active` rows are
    /// superseded, including the prior source left active when recovery
    /// resumes after the journal replaced its committed predecessor.
    pub fn supersede_other_active_history_sources(
        &self,
        repo_history_id: &RepoHistoryId,
        active_source_generation_id: &str,
    ) -> Result<u64> {
        validate_generation_id(active_source_generation_id)?;
        let _guard = self.lock_mutation()?;
        let history_dir = self.repo_history_root(repo_history_id)?;
        let mut superseded = 0_u64;
        for generation_dir in read_child_directories(&history_dir, &["current-ready.json"])? {
            let Some(mut source) = read_json::<StoredHistorySourceV1>(
                &generation_dir,
                "source.json",
                MAX_GENERATION_RECORD_BYTES,
                "stored Git-history source",
            )?
            else {
                bail!("Git-history generation is missing source metadata");
            };
            if source.repo_history_id != *repo_history_id
                || generation_dir.file_name().and_then(|name| name.to_str())
                    != Some(source.source_generation_id.as_str())
            {
                bail!("Git-history generation metadata does not match its durable location");
            }
            if source.source_generation_id == active_source_generation_id
                || source.state != GitHistorySourceStateV1::Active
            {
                continue;
            }
            source.state = GitHistorySourceStateV1::Superseded;
            source.diagnostic = Some(format!(
                "superseded by activated source {active_source_generation_id}"
            ));
            let directory = NofollowDirectory::open_existing(&generation_dir)?
                .ok_or(StoreRequestError::NotFound)?;
            write_json(&directory, "source.json", &source)?;
            superseded = superseded.saturating_add(1);
        }
        Ok(superseded)
    }

    fn missing_history_records_locked(
        &self,
        upload: &HistoryUploadRecordV1,
        cursor: Option<&str>,
    ) -> Result<MissingHistoryRecordsPageV1> {
        let upload_dir = self.upload_dir(&upload.producer_id, &upload.upload_id)?;
        let manifest = self.load_manifest(&upload_dir)?;
        let unique = manifest
            .iter()
            .map(|entry| entry.content_sha256.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let start = match cursor {
            Some(value) => value
                .parse::<usize>()
                .ok()
                .filter(|index| *index <= unique.len())
                .ok_or(StoreRequestError::InvalidInput)?,
            None => 0,
        };
        let mut missing = Vec::new();
        let mut examined = start;
        while examined < unique.len() && missing.len() < MISSING_PAGE_SIZE {
            let hash = unique[examined];
            let size = manifest
                .iter()
                .find(|entry| entry.content_sha256 == hash)
                .expect("hash came from manifest")
                .encoded_bytes as usize;
            if self.read_record_bytes(hash, size)?.is_none() {
                missing.push(hash.to_string());
            }
            examined += 1;
        }
        Ok(MissingHistoryRecordsPageV1 {
            source_generation_id: upload
                .source_generation_id
                .clone()
                .ok_or(StoreRequestError::InvalidState)?,
            hashes: missing,
            next_cursor: (examined < unique.len()).then(|| examined.to_string()),
        })
    }

    fn missing_provenance_documents_locked(
        &self,
        upload: &ProvenanceUploadRecordV1,
        cursor: Option<&str>,
    ) -> Result<MissingProvenanceDocumentsPageV1> {
        let upload_path = self.provenance_upload_dir(&upload.producer_id, &upload.upload_id)?;
        let manifest = self.load_provenance_manifest(&upload_path)?;
        let unique = manifest
            .iter()
            .map(|entry| entry.document_sha256.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let start = match cursor {
            Some(value) => value
                .parse::<usize>()
                .ok()
                .filter(|index| *index <= unique.len())
                .ok_or(StoreRequestError::InvalidInput)?,
            None => 0,
        };
        let mut missing = Vec::new();
        let mut examined = start;
        while examined < unique.len() && missing.len() < MISSING_PAGE_SIZE {
            let hash = unique[examined];
            let size = manifest
                .iter()
                .find(|entry| entry.document_sha256 == hash)
                .expect("hash came from manifest")
                .encoded_bytes as usize;
            if self.read_provenance_document_bytes(hash, size)?.is_none() {
                missing.push(hash.to_string());
            }
            examined += 1;
        }
        Ok(MissingProvenanceDocumentsPageV1 {
            import_generation_id: upload.import_generation_id.clone(),
            hashes: missing,
            next_cursor: (examined < unique.len()).then(|| examined.to_string()),
        })
    }

    fn provenance_producer_upload_dir(&self, producer_id: &str) -> Result<PathBuf> {
        validate_producer_authority(producer_id)?;
        let path = self
            .root
            .join("provenance-imports/uploads")
            .join(sha256(producer_id.as_bytes()));
        NofollowDirectory::open_or_create(&path)?;
        Ok(path)
    }

    fn provenance_upload_dir(&self, producer_id: &str, upload_id: &str) -> Result<PathBuf> {
        validate_upload_id(upload_id)?;
        let path = self
            .provenance_producer_upload_dir(producer_id)?
            .join(upload_id);
        if NofollowDirectory::open_existing(&path)?.is_none() {
            bail!(StoreRequestError::NotFound);
        }
        Ok(path)
    }

    fn load_provenance_upload(
        &self,
        upload_path: &Path,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<ProvenanceUploadRecordV1> {
        let record = read_json::<ProvenanceUploadRecordV1>(
            upload_path,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "provenance import upload record",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if record.version != STORE_VERSION
            || record.producer_id != producer_id
            || record.upload_id != upload_id
        {
            bail!(StoreRequestError::NotFound);
        }
        Ok(record)
    }

    fn load_provenance_manifest(
        &self,
        upload_path: &Path,
    ) -> Result<Vec<ProvenanceImportManifestEntryV1>> {
        read_json(
            upload_path,
            "manifest.json",
            MAX_MANIFEST_BYTES,
            "provenance import manifest",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))
    }

    fn provenance_document_bucket(&self, hash: &str) -> Result<NofollowDirectory> {
        validate_sha256(hash)?;
        NofollowDirectory::open_or_create(
            &self
                .root
                .join("provenance-imports/documents/sha256")
                .join(&hash[..2]),
        )
    }

    fn install_provenance_document_bytes(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        let directory = self.provenance_document_bucket(hash)?;
        if let Some(existing) = directory.read_regular(
            hash,
            MAX_PROVENANCE_DOCUMENT_BYTES as usize,
            "provenance import document",
        )? {
            if existing != bytes || sha256(&existing) != hash {
                bail!(StoreRequestError::InvalidInput);
            }
            return Ok(());
        }
        directory.atomic_replace(hash, bytes)
    }

    fn read_provenance_document_bytes(
        &self,
        hash: &str,
        expected_size: usize,
    ) -> Result<Option<Vec<u8>>> {
        let directory = self.provenance_document_bucket(hash)?;
        let Some(bytes) = directory.read_regular(
            hash,
            expected_size.saturating_add(1),
            "provenance import document",
        )?
        else {
            return Ok(None);
        };
        if bytes.len() != expected_size || sha256(&bytes) != hash {
            bail!(StoreRequestError::InvalidInput);
        }
        Ok(Some(bytes))
    }

    fn provenance_project_dir(&self, project_id: &str) -> Result<PathBuf> {
        validate_receipt_authority("project", project_id)?;
        Ok(self
            .root
            .join("provenance-imports/projects")
            .join(project_id))
    }

    fn allocate_provenance_acceptance_sequence(&self, project_id: &str) -> Result<u64> {
        let path = self.provenance_project_dir(project_id)?;
        let directory = NofollowDirectory::open_or_create(&path)?;
        let sequence = read_json::<ProvenanceAcceptanceSequenceV1>(
            &path,
            "acceptance-sequence.json",
            MAX_GENERATION_RECORD_BYTES,
            "provenance acceptance sequence",
        )?
        .unwrap_or(ProvenanceAcceptanceSequenceV1 {
            version: STORE_VERSION,
            next_sequence: 1,
        });
        if sequence.version != STORE_VERSION || sequence.next_sequence == 0 {
            bail!(StoreRequestError::InvalidState);
        }
        let accepted = sequence.next_sequence;
        let next_sequence = accepted
            .checked_add(1)
            .ok_or(StoreRequestError::LimitExceeded)?;
        write_json(
            &directory,
            "acceptance-sequence.json",
            &ProvenanceAcceptanceSequenceV1 {
                version: STORE_VERSION,
                next_sequence,
            },
        )?;
        Ok(accepted)
    }

    fn load_provenance_ready_pointer(
        &self,
        project_id: &str,
    ) -> Result<Option<ProvenanceReadyPointerV1>> {
        let path = self.provenance_project_dir(project_id)?;
        let Some(pointer) = read_json::<ProvenanceReadyPointerV1>(
            &path,
            "current-ready.json",
            MAX_GENERATION_RECORD_BYTES,
            "provenance ready pointer",
        )?
        else {
            return Ok(None);
        };
        if pointer.version != STORE_VERSION
            || pointer.accepted_sequence == 0
            || pointer.import_generation_id.is_empty()
        {
            bail!(StoreRequestError::InvalidState);
        }
        let source = self.load_provenance_generation(&pointer.import_generation_id)?;
        if source.project_id != project_id
            || source.producer_id != pointer.producer_id
            || source.descriptor.notes_tip != pointer.notes_tip
            || source.accepted_sequence != pointer.accepted_sequence
            || !matches!(
                source.state,
                ProvenanceImportStateV1::Ready
                    | ProvenanceImportStateV1::Importing
                    | ProvenanceImportStateV1::Active
                    | ProvenanceImportStateV1::Quarantined
            )
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(Some(pointer))
    }

    fn advance_provenance_ready_pointer(&self, source: &StoredProvenanceImportV1) -> Result<()> {
        if !matches!(
            source.state,
            ProvenanceImportStateV1::Ready
                | ProvenanceImportStateV1::Importing
                | ProvenanceImportStateV1::Active
        ) {
            return Ok(());
        }
        if let Some(current) = self.load_provenance_ready_pointer(&source.project_id)? {
            if current.accepted_sequence > source.accepted_sequence {
                return Ok(());
            }
            if current.accepted_sequence == source.accepted_sequence {
                if current.import_generation_id != source.import_generation_id {
                    bail!(StoreRequestError::InvalidState);
                }
                return Ok(());
            }
        }
        let path = self.provenance_project_dir(&source.project_id)?;
        let directory = NofollowDirectory::open_or_create(&path)?;
        write_json(
            &directory,
            "current-ready.json",
            &ProvenanceReadyPointerV1 {
                version: STORE_VERSION,
                import_generation_id: source.import_generation_id.clone(),
                producer_id: source.producer_id.clone(),
                notes_tip: source.descriptor.notes_tip.clone(),
                accepted_sequence: source.accepted_sequence,
            },
        )
    }

    fn write_provenance_source(&self, source: &StoredProvenanceImportV1) -> Result<()> {
        let directory = NofollowDirectory::open_existing(
            &self.provenance_generation_dir(&source.import_generation_id)?,
        )?
        .ok_or(StoreRequestError::NotFound)?;
        write_json(&directory, "source.json", source)
    }

    fn provenance_generation_dir(&self, import_generation_id: &str) -> Result<PathBuf> {
        validate_provenance_generation_id(import_generation_id)?;
        Ok(self
            .root
            .join("provenance-imports/generations")
            .join(import_generation_id))
    }

    fn load_provenance_generation_index(
        &self,
        import_generation_id: &str,
    ) -> Result<ProvenanceGenerationIndexV1> {
        validate_provenance_generation_id(import_generation_id)?;
        let index = read_json::<ProvenanceGenerationIndexV1>(
            &self.root.join("provenance-imports/generation-index"),
            &format!("{import_generation_id}.json"),
            MAX_GENERATION_RECORD_BYTES,
            "provenance import generation index",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if index.version != STORE_VERSION || index.import_generation_id != import_generation_id {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(index)
    }

    fn load_provenance_generation(
        &self,
        import_generation_id: &str,
    ) -> Result<StoredProvenanceImportV1> {
        let source = read_json::<StoredProvenanceImportV1>(
            &self.provenance_generation_dir(import_generation_id)?,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "stored provenance import",
        )?
        .ok_or(StoreRequestError::NotFound)?;
        if source.version != STORE_VERSION
            || source.import_generation_id != import_generation_id
            || source.accepted_sequence == 0
        {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(source)
    }

    fn load_provenance_generation_for_producer(
        &self,
        producer_id: &str,
        import_generation_id: &str,
    ) -> Result<StoredProvenanceImportV1> {
        let index = self.load_provenance_generation_index(import_generation_id)?;
        if index.producer_id != producer_id {
            bail!(StoreRequestError::NotFound);
        }
        let source = self.load_provenance_generation(import_generation_id)?;
        if source.producer_id != index.producer_id || source.project_id != index.project_id {
            bail!(StoreRequestError::InvalidState);
        }
        Ok(source)
    }

    fn load_provenance_generation_manifest(
        &self,
        import_generation_id: &str,
    ) -> Result<Vec<ProvenanceImportManifestEntryV1>> {
        read_json(
            &self.provenance_generation_dir(import_generation_id)?,
            "manifest.json",
            MAX_MANIFEST_BYTES,
            "provenance import generation manifest",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))
    }

    fn producer_upload_dir(&self, producer_id: &str) -> Result<PathBuf> {
        let digest = sha256(producer_id.as_bytes());
        let path = self.root.join("uploads").join(digest);
        NofollowDirectory::open_or_create(&path)?;
        Ok(path)
    }

    fn upload_dir(&self, producer_id: &str, upload_id: &str) -> Result<PathBuf> {
        validate_upload_id(upload_id)?;
        let path = self.producer_upload_dir(producer_id)?.join(upload_id);
        if NofollowDirectory::open_existing(&path)?.is_none() {
            bail!(StoreRequestError::NotFound);
        }
        Ok(path)
    }

    fn load_upload(
        &self,
        upload_dir: &Path,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<HistoryUploadRecordV1> {
        let record = read_json::<HistoryUploadRecordV1>(
            upload_dir,
            "upload.json",
            MAX_UPLOAD_RECORD_BYTES,
            "Git-history upload record",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))?;
        if record.version != STORE_VERSION
            || record.producer_id != producer_id
            || record.upload_id != upload_id
        {
            bail!(StoreRequestError::NotFound);
        }
        Ok(record)
    }

    fn load_manifest(&self, upload_dir: &Path) -> Result<Vec<GitHistoryManifestEntryV1>> {
        read_json(
            upload_dir,
            "manifest.json",
            MAX_MANIFEST_BYTES,
            "Git-history manifest",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))
    }

    fn install_record_bytes(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        validate_sha256(hash)?;
        let directory = self.record_bucket(hash)?;
        if let Some(existing) = directory.read_regular(
            hash,
            MAX_HISTORY_RECORD_BYTES as usize,
            "Git-history record",
        )? {
            if existing != bytes || sha256(&existing) != hash {
                bail!(StoreRequestError::InvalidInput);
            }
            return Ok(());
        }
        directory.atomic_replace(hash, bytes)
    }

    fn read_record_bytes(&self, hash: &str, expected_size: usize) -> Result<Option<Vec<u8>>> {
        validate_sha256(hash)?;
        let directory = self.record_bucket(hash)?;
        let Some(bytes) =
            directory.read_regular(hash, expected_size.saturating_add(1), "Git-history record")?
        else {
            return Ok(None);
        };
        if bytes.len() != expected_size || sha256(&bytes) != hash {
            bail!(StoreRequestError::InvalidInput);
        }
        Ok(Some(bytes))
    }

    fn record_bucket(&self, hash: &str) -> Result<NofollowDirectory> {
        validate_sha256(hash)?;
        NofollowDirectory::open_or_create(&self.root.join("records/sha256").join(&hash[..2]))
    }

    fn repo_history_root(&self, repo_history_id: &RepoHistoryId) -> Result<PathBuf> {
        let path = self
            .root
            .join("repos")
            .join(repo_history_id.as_str())
            .join("history");
        NofollowDirectory::open_or_create(&path)?;
        Ok(path)
    }

    fn generation_dir(
        &self,
        repo_history_id: &RepoHistoryId,
        source_generation_id: &str,
    ) -> Result<PathBuf> {
        validate_generation_id(source_generation_id)?;
        Ok(self
            .repo_history_root(repo_history_id)?
            .join(source_generation_id))
    }

    fn load_generation(
        &self,
        repo_history_id: &RepoHistoryId,
        source_generation_id: &str,
    ) -> Result<StoredHistorySourceV1> {
        read_json(
            &self.generation_dir(repo_history_id, source_generation_id)?,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "stored Git-history source",
        )?
        .ok_or_else(|| anyhow!(StoreRequestError::NotFound))
    }

    fn expire_stale_uploads(&self, now: u64) -> Result<u64> {
        let mut expired = 0_u64;
        for producer_dir in read_directories(&self.root.join("uploads"))? {
            for upload_dir in read_directories(&producer_dir)? {
                let upload = read_json::<HistoryUploadRecordV1>(
                    &upload_dir,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "Git-history upload record",
                )?
                .ok_or_else(|| anyhow!("Git-source upload directory is missing its record"))?;
                if now.saturating_sub(upload.updated_unix_secs) < HISTORY_UPLOAD_IDLE_TTL_SECS {
                    continue;
                }
                remove_upload_directory(&upload_dir)?;
                expired = expired.saturating_add(1);
            }
            remove_directory_if_empty(&producer_dir)?;
        }
        Ok(expired)
    }

    fn expire_stale_provenance_uploads(&self, now: u64) -> Result<u64> {
        let mut expired = 0_u64;
        for producer_dir in read_directories(&self.root.join("provenance-imports/uploads"))? {
            for upload_dir in read_directories(&producer_dir)? {
                let upload = read_json::<ProvenanceUploadRecordV1>(
                    &upload_dir,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provenance import upload record",
                )?
                .ok_or_else(|| anyhow!("provenance upload directory is missing its record"))?;
                if now.saturating_sub(upload.updated_unix_secs) < HISTORY_UPLOAD_IDLE_TTL_SECS {
                    continue;
                }
                remove_upload_directory(&upload_dir)?;
                expired = expired.saturating_add(1);
            }
            remove_directory_if_empty(&producer_dir)?;
        }
        Ok(expired)
    }

    fn retire_old_generations(
        &self,
        protected_generation_ids: &BTreeSet<String>,
        retained: usize,
    ) -> Result<u64> {
        let mut retired = 0_u64;
        for repo_dir in read_directories(&self.root.join("repos"))? {
            let history_dir = repo_dir.join("history");
            if NofollowDirectory::open_existing(&history_dir)?.is_none() {
                continue;
            }
            let current = read_json::<ReadyPointerV1>(
                &history_dir,
                "current-ready.json",
                MAX_GENERATION_RECORD_BYTES,
                "Git-history ready pointer",
            )?;
            let mut sources = Vec::new();
            for generation_dir in read_child_directories(&history_dir, &["current-ready.json"])? {
                let source = read_json::<StoredHistorySourceV1>(
                    &generation_dir,
                    "source.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "stored Git-history source",
                )?
                .ok_or_else(|| anyhow!("Git-history generation is missing source metadata"))?;
                if generation_dir.file_name().and_then(|name| name.to_str())
                    != Some(source.source_generation_id.as_str())
                    || repo_dir.file_name().and_then(|name| name.to_str())
                        != Some(source.repo_history_id.as_str())
                    || matches!(
                        source.state,
                        GitHistorySourceStateV1::ReceivingManifest
                            | GitHistorySourceStateV1::MissingRecords
                    )
                {
                    bail!("Git-history generation metadata does not match its durable location");
                }
                sources.push(source);
            }
            if let Some(pointer) = current.as_ref() {
                let source = sources
                    .iter()
                    .find(|source| source.source_generation_id == pointer.source_generation_id)
                    .ok_or_else(|| {
                        anyhow!("Git-history ready pointer references a missing generation")
                    })?;
                if source.producer_id != pointer.producer_id
                    || source.descriptor.repo_head != pointer.repo_head
                {
                    bail!("Git-history ready pointer disagrees with its generation");
                }
            }
            sources.sort_by(|left, right| {
                right
                    .created_unix_secs
                    .cmp(&left.created_unix_secs)
                    .then_with(|| right.source_generation_id.cmp(&left.source_generation_id))
            });
            let mut retained_by_policy = BTreeSet::new();
            if let Some(pointer) = current.as_ref() {
                retained_by_policy.insert(pointer.source_generation_id.clone());
            }
            let mut retained_prior = 0_usize;
            for source in &sources {
                if retained_by_policy.contains(&source.source_generation_id) {
                    continue;
                }
                if retained_prior >= retained {
                    break;
                }
                retained_by_policy.insert(source.source_generation_id.clone());
                retained_prior += 1;
            }
            let mut keep = protected_generation_ids.clone();
            keep.extend(retained_by_policy);
            keep.extend(
                sources
                    .iter()
                    .filter(|source| {
                        matches!(
                            source.state,
                            GitHistorySourceStateV1::Materializing
                                | GitHistorySourceStateV1::Publishing
                                | GitHistorySourceStateV1::Active
                        )
                    })
                    .map(|source| source.source_generation_id.clone()),
            );
            for source in sources {
                if keep.contains(&source.source_generation_id) {
                    continue;
                }
                remove_regular_file_if_present(
                    &self
                        .root
                        .join("generation-index")
                        .join(format!("{}.json", source.source_generation_id)),
                )?;
                remove_generation_directory(&history_dir.join(&source.source_generation_id))?;
                retired = retired.saturating_add(1);
            }
        }
        Ok(retired)
    }

    fn retire_old_provenance_generations(&self, retained: usize) -> Result<u64> {
        let mut sources_by_project: BTreeMap<String, Vec<StoredProvenanceImportV1>> =
            BTreeMap::new();
        for generation_dir in read_directories(&self.root.join("provenance-imports/generations"))? {
            let source = read_json::<StoredProvenanceImportV1>(
                &generation_dir,
                "source.json",
                MAX_GENERATION_RECORD_BYTES,
                "stored provenance import",
            )?
            .ok_or_else(|| anyhow!("provenance generation is missing source metadata"))?;
            if source.accepted_sequence == 0
                || generation_dir.file_name().and_then(|name| name.to_str())
                    != Some(source.import_generation_id.as_str())
            {
                bail!("provenance generation metadata does not match its durable location");
            }
            let index = self.load_provenance_generation_index(&source.import_generation_id)?;
            if index.producer_id != source.producer_id || index.project_id != source.project_id {
                bail!("provenance generation index disagrees with source metadata");
            }
            sources_by_project
                .entry(source.project_id.clone())
                .or_default()
                .push(source);
        }
        let journal_roots = self
            .list_provenance_import_journals()?
            .into_iter()
            .filter(|journal| !journal.stage.terminal())
            .map(|journal| journal.import_generation_id)
            .collect::<BTreeSet<_>>();
        let mut retired = 0_u64;
        for (project_id, mut sources) in sources_by_project {
            let current = self.load_provenance_ready_pointer(&project_id)?;
            sources.sort_by(|left, right| {
                right
                    .accepted_sequence
                    .cmp(&left.accepted_sequence)
                    .then_with(|| right.import_generation_id.cmp(&left.import_generation_id))
            });
            let mut seen_sequences = BTreeSet::new();
            for source in &sources {
                if !seen_sequences.insert(source.accepted_sequence) {
                    bail!("duplicate provenance acceptance sequence for one project");
                }
            }
            let mut keep = BTreeSet::new();
            if let Some(pointer) = current.as_ref() {
                keep.insert(pointer.import_generation_id.clone());
            }
            let mut retained_prior = 0_usize;
            for source in &sources {
                if keep.contains(&source.import_generation_id) {
                    continue;
                }
                if retained_prior >= retained {
                    break;
                }
                keep.insert(source.import_generation_id.clone());
                retained_prior += 1;
            }
            keep.extend(journal_roots.iter().cloned());
            keep.extend(
                sources
                    .iter()
                    .filter(|source| {
                        matches!(
                            source.state,
                            ProvenanceImportStateV1::Ready
                                | ProvenanceImportStateV1::Importing
                                | ProvenanceImportStateV1::Active
                                | ProvenanceImportStateV1::Quarantined
                        )
                    })
                    .map(|source| source.import_generation_id.clone()),
            );
            for source in sources {
                if keep.contains(&source.import_generation_id) {
                    continue;
                }
                remove_regular_file_if_present(
                    &self
                        .root
                        .join("provenance-imports/generation-index")
                        .join(format!("{}.json", source.import_generation_id)),
                )?;
                remove_generation_directory(
                    &self.provenance_generation_dir(&source.import_generation_id)?,
                )?;
                retired = retired.saturating_add(1);
            }
        }
        Ok(retired)
    }

    fn referenced_record_hashes(&self, limits: GitSourceLimits) -> Result<BTreeSet<String>> {
        let mut referenced = BTreeSet::new();
        for producer_dir in read_directories(&self.root.join("uploads"))? {
            for upload_dir in read_directories(&producer_dir)? {
                if let Some(manifest) = read_json::<Vec<GitHistoryManifestEntryV1>>(
                    &upload_dir,
                    "manifest.json",
                    MAX_MANIFEST_BYTES,
                    "Git-history manifest",
                )? {
                    let upload = read_json::<HistoryUploadRecordV1>(
                        &upload_dir,
                        "upload.json",
                        MAX_UPLOAD_RECORD_BYTES,
                        "Git-history upload record",
                    )?
                    .ok_or_else(|| anyhow!("Git-history upload manifest has no upload record"))?;
                    validate_history_manifest(&upload.descriptor, &manifest, limits)?;
                    referenced.extend(manifest.into_iter().map(|entry| entry.content_sha256));
                }
            }
        }
        for repo_dir in read_directories(&self.root.join("repos"))? {
            let history_dir = repo_dir.join("history");
            if NofollowDirectory::open_existing(&history_dir)?.is_none() {
                continue;
            }
            for generation_dir in read_child_directories(&history_dir, &["current-ready.json"])? {
                let source = read_json::<StoredHistorySourceV1>(
                    &generation_dir,
                    "source.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "stored Git-history source",
                )?
                .ok_or_else(|| anyhow!("Git-history generation is missing source metadata"))?;
                let descriptor = read_json::<GitHistoryDescriptorV1>(
                    &generation_dir,
                    "descriptor.json",
                    MAX_GENERATION_RECORD_BYTES,
                    "Git-history generation descriptor",
                )?
                .ok_or_else(|| anyhow!("Git-history generation is missing its descriptor"))?;
                if source.descriptor != descriptor {
                    bail!("Git-history generation descriptor disagrees with source metadata");
                }
                let manifest = read_json::<Vec<GitHistoryManifestEntryV1>>(
                    &generation_dir,
                    "manifest.json",
                    MAX_MANIFEST_BYTES,
                    "Git-history generation manifest",
                )?
                .ok_or_else(|| anyhow!("Git-history generation is missing its manifest"))?;
                validate_history_manifest(&descriptor, &manifest, limits)?;
                referenced.extend(manifest.into_iter().map(|entry| entry.content_sha256));
            }
        }
        Ok(referenced)
    }

    fn sweep_unreferenced_records(
        &self,
        referenced: &BTreeSet<String>,
        now: u64,
        grace_secs: u64,
    ) -> Result<(u64, u64)> {
        let records_root = self.root.join("records/sha256");
        let mut deleted = 0_u64;
        let mut deleted_bytes = 0_u64;
        for bucket in read_directories(&records_root)? {
            let mut bucket_changed = false;
            for entry in fs::read_dir(&bucket)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!("refusing unsafe Git-history record store member");
                }
                let hash = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow!("Git-history record name is not UTF-8"))?;
                validate_sha256(&hash)?;
                if referenced.contains(&hash) {
                    continue;
                }
                let modified = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if now.saturating_sub(modified) < grace_secs {
                    continue;
                }
                fs::remove_file(&path)?;
                bucket_changed = true;
                deleted = deleted.saturating_add(1);
                deleted_bytes = deleted_bytes.saturating_add(metadata.len());
            }
            if bucket_changed {
                fs::File::open(&bucket)?.sync_all()?;
            }
            remove_directory_if_empty(&bucket)?;
        }
        if deleted > 0 {
            fs::File::open(records_root)?.sync_all()?;
        }
        Ok((deleted, deleted_bytes))
    }

    fn referenced_provenance_document_hashes(
        &self,
        limits: GitSourceLimits,
    ) -> Result<BTreeSet<String>> {
        let mut referenced = BTreeSet::new();
        for producer_dir in read_directories(&self.root.join("provenance-imports/uploads"))? {
            for upload_dir in read_directories(&producer_dir)? {
                let Some(manifest) = read_json::<Vec<ProvenanceImportManifestEntryV1>>(
                    &upload_dir,
                    "manifest.json",
                    MAX_MANIFEST_BYTES,
                    "provenance import manifest",
                )?
                else {
                    continue;
                };
                let upload = read_json::<ProvenanceUploadRecordV1>(
                    &upload_dir,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "provenance import upload record",
                )?
                .ok_or_else(|| anyhow!("provenance upload manifest has no upload record"))?;
                validate_provenance_manifest(&upload.descriptor, &manifest, limits)?;
                referenced.extend(manifest.into_iter().map(|entry| entry.document_sha256));
            }
        }
        for generation_dir in read_directories(&self.root.join("provenance-imports/generations"))? {
            let source = read_json::<StoredProvenanceImportV1>(
                &generation_dir,
                "source.json",
                MAX_GENERATION_RECORD_BYTES,
                "stored provenance import",
            )?
            .ok_or_else(|| anyhow!("provenance generation is missing source metadata"))?;
            let manifest = read_json::<Vec<ProvenanceImportManifestEntryV1>>(
                &generation_dir,
                "manifest.json",
                MAX_MANIFEST_BYTES,
                "provenance import generation manifest",
            )?
            .ok_or_else(|| anyhow!("provenance generation is missing its manifest"))?;
            validate_provenance_manifest(&source.descriptor, &manifest, limits)?;
            referenced.extend(manifest.into_iter().map(|entry| entry.document_sha256));
        }
        Ok(referenced)
    }

    fn sweep_unreferenced_provenance_documents(
        &self,
        referenced: &BTreeSet<String>,
        now: u64,
        grace_secs: u64,
    ) -> Result<(u64, u64)> {
        let documents_root = self.root.join("provenance-imports/documents/sha256");
        let mut deleted = 0_u64;
        let mut deleted_bytes = 0_u64;
        for bucket in read_directories(&documents_root)? {
            let mut bucket_changed = false;
            for entry in fs::read_dir(&bucket)? {
                let entry = entry?;
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!("refusing unsafe provenance document store member");
                }
                let hash = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow!("provenance document name is not UTF-8"))?;
                validate_sha256(&hash)?;
                if referenced.contains(&hash) {
                    continue;
                }
                let modified = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if now.saturating_sub(modified) < grace_secs {
                    continue;
                }
                fs::remove_file(&path)?;
                bucket_changed = true;
                deleted = deleted.saturating_add(1);
                deleted_bytes = deleted_bytes.saturating_add(metadata.len());
            }
            if bucket_changed {
                fs::File::open(&bucket)?.sync_all()?;
            }
            remove_directory_if_empty(&bucket)?;
        }
        if deleted > 0 {
            fs::File::open(documents_root)?.sync_all()?;
        }
        Ok((deleted, deleted_bytes))
    }

    fn lock_mutation(&self) -> Result<MutationGuard<'_>> {
        let in_process = self
            .mutation
            .lock()
            .map_err(|_| anyhow!("Git-source mutation lock is poisoned"))?;
        let anchor = acquire_store_lock_nofollow(&self.root.join("store.json.lock"))?;
        Ok(MutationGuard {
            _anchor: anchor,
            _in_process: in_process,
        })
    }

    fn current_limits(&self) -> Result<StoreLimits> {
        self.limits
            .read()
            .map(|limits| *limits)
            .map_err(|_| anyhow!("Git-source limit lock is poisoned"))
    }

    fn maintain_locked(
        &self,
        protected_generation_ids: &BTreeSet<String>,
        now: u64,
    ) -> Result<MaintenanceReport> {
        let limits = self.current_limits()?;
        let expired_uploads = self
            .expire_stale_uploads(now)?
            .saturating_add(self.expire_stale_provenance_uploads(now)?);
        let mut protected_generation_ids = protected_generation_ids.clone();
        protected_generation_ids.extend(self.activation_source_roots()?);
        let retired_generations = self
            .retire_old_generations(
                &protected_generation_ids,
                limits.retained_history_generations,
            )?
            .saturating_add(
                self.retire_old_provenance_generations(limits.retained_history_generations)?,
            );
        let referenced_records = self.referenced_record_hashes(limits.contract)?;
        let (history_deleted, history_deleted_bytes) = self.sweep_unreferenced_records(
            &referenced_records,
            now,
            limits.unreferenced_record_grace_secs,
        )?;
        let referenced_provenance = self.referenced_provenance_document_hashes(limits.contract)?;
        let (provenance_deleted, provenance_deleted_bytes) = self
            .sweep_unreferenced_provenance_documents(
                &referenced_provenance,
                now,
                limits.unreferenced_record_grace_secs,
            )?;
        let deleted_records = history_deleted.saturating_add(provenance_deleted);
        let deleted_record_bytes = history_deleted_bytes.saturating_add(provenance_deleted_bytes);
        Ok(MaintenanceReport {
            expired_uploads,
            retired_generations,
            deleted_records,
            deleted_record_bytes,
        })
    }
}

fn begin_response(upload_id: String) -> BeginGitHistoryUploadResponseV1 {
    BeginGitHistoryUploadResponseV1 {
        upload_id,
        max_page_entries: MAX_HISTORY_MANIFEST_PAGE_ENTRIES,
        max_page_bytes: MAX_HISTORY_MANIFEST_PAGE_BYTES,
        max_record_bytes: MAX_HISTORY_RECORD_BYTES,
    }
}

fn begin_provenance_response(upload_id: String) -> BeginProvenanceImportResponseV1 {
    BeginProvenanceImportResponseV1 {
        upload_id,
        max_page_entries: MAX_PROVENANCE_MANIFEST_PAGE_ENTRIES,
        max_page_bytes: MAX_PROVENANCE_MANIFEST_PAGE_BYTES,
        max_document_bytes: MAX_PROVENANCE_DOCUMENT_BYTES,
    }
}

fn finalize_provenance_response(
    import_generation_id: String,
) -> FinalizeProvenanceImportResponseV1 {
    FinalizeProvenanceImportResponseV1 {
        status_url: format!(
            "/internal/code-source/v1/provenance/generations/{import_generation_id}/status"
        ),
        import_generation_id,
    }
}

fn validate_store_limits(limits: StoreLimits) -> Result<()> {
    if limits.max_open_uploads_per_producer == 0
        || limits.retained_history_generations == 0
        || limits.contract.max_history_commits == 0
        || limits.contract.max_history_logical_bytes == 0
        || limits.contract.max_provenance_documents == 0
        || limits.contract.max_provenance_logical_bytes == 0
    {
        bail!(StoreRequestError::LimitExceeded);
    }
    Ok(())
}

fn validate_receipt_authority(producer_id: &str, project_id: &str) -> Result<()> {
    validate_producer_authority(producer_id)?;
    bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_string())
        .map_err(|_| anyhow!(StoreRequestError::InvalidInput))?;
    Ok(())
}

fn validate_producer_authority(producer_id: &str) -> Result<()> {
    if producer_id.is_empty()
        || producer_id.len() > 128
        || !producer_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(())
}

fn validate_stored_provenance_receipt(
    stored: &StoredProvenanceExportReceiptV1,
    limits: GitSourceLimits,
) -> Result<()> {
    if stored.version != STORE_VERSION {
        bail!(StoreRequestError::InvalidInput);
    }
    validate_receipt_authority(&stored.producer_id, &stored.project_id)?;
    stored.receipt.validate(limits)?;
    Ok(())
}

fn finalize_response(source_generation_id: String) -> FinalizeGitHistoryUploadResponseV1 {
    FinalizeGitHistoryUploadResponseV1 {
        status_url: format!(
            "/internal/code-source/v1/git-history/generations/{source_generation_id}/status"
        ),
        source_generation_id,
    }
}

fn read_directories(path: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("refusing non-directory Git-source store member");
        }
        directories.push(entry.path());
    }
    directories.sort();
    Ok(directories)
}

fn read_child_directories(path: &Path, allowed_files: &[&str]) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            bail!("refusing symlink in Git-source store");
        }
        if metadata.is_dir() {
            directories.push(entry.path());
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("Git-source store member name is not UTF-8"))?;
        if !metadata.is_file() || !allowed_files.contains(&name.as_str()) {
            bail!("refusing unexpected Git-source store member");
        }
    }
    directories.sort();
    Ok(directories)
}

fn remove_upload_directory(path: &Path) -> Result<()> {
    let pages = path.join("pages");
    if NofollowDirectory::open_existing(&pages)?.is_some() {
        for entry in fs::read_dir(&pages)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow!("Git-source manifest page name is not UTF-8"))?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || name.len() != 13
                || !name.ends_with(".json")
                || !name[..8].bytes().all(|byte| byte.is_ascii_digit())
            {
                bail!("refusing unexpected Git-source manifest page member");
            }
            fs::remove_file(entry.path())?;
        }
        fs::File::open(&pages)?.sync_all()?;
        fs::remove_dir(&pages)?;
    }
    for name in ["upload.json", "manifest.json"] {
        remove_regular_file_if_present(&path.join(name))?;
    }
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        bail!("refusing to remove nonempty Git-source upload directory");
    }
    fs::remove_dir(path)?;
    sync_parent(path)
}

fn remove_generation_directory(path: &Path) -> Result<()> {
    NofollowDirectory::open_existing(path)?
        .ok_or_else(|| anyhow!("Git-history generation disappeared during maintenance"))?;
    for name in ["descriptor.json", "manifest.json", "source.json"] {
        remove_regular_file_if_present(&path.join(name))?;
    }
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        bail!("refusing to remove nonempty Git-history generation directory");
    }
    fs::remove_dir(path)?;
    sync_parent(path)
}

fn remove_regular_file_if_present(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing to remove unsafe Git-source store member");
    }
    fs::remove_file(path)?;
    sync_parent(path)
}

fn remove_directory_if_empty(path: &Path) -> Result<()> {
    if fs::read_dir(path)?.next().transpose()?.is_none() {
        fs::remove_dir(path)?;
        sync_parent(path)?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(
    directory: &Path,
    name: &str,
    max_bytes: usize,
    label: &str,
) -> Result<Option<T>> {
    let Some(directory) = NofollowDirectory::open_existing(directory)? else {
        return Ok(None);
    };
    let Some(bytes) = directory.read_regular(name, max_bytes, label)? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {label}"))?,
    ))
}

fn write_json<T: Serialize>(directory: &NofollowDirectory, name: &str, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    directory.atomic_replace(name, &bytes)
}

fn install_immutable_json<T: Serialize + DeserializeOwned + PartialEq>(
    directory: &NofollowDirectory,
    name: &str,
    value: &T,
) -> Result<()> {
    if let Some(bytes) =
        directory.read_regular(name, MAX_MANIFEST_BYTES, "immutable Git-source member")?
    {
        let existing: T = serde_json::from_slice(&bytes)?;
        if &existing != value {
            bail!(StoreRequestError::InvalidInput);
        }
        return Ok(());
    }
    write_json(directory, name, value)
}

fn validate_upload_id(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(StoreRequestError::NotFound);
    }
    Ok(())
}

fn validate_generation_id(value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("ghs_") else {
        bail!(StoreRequestError::NotFound);
    };
    validate_sha256(digest)
}

fn validate_provenance_generation_id(value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("pis_") else {
        bail!(StoreRequestError::NotFound);
    };
    validate_sha256(digest)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_git_source::{
        GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitObjectFormatV1,
        ProvenanceImportDescriptorV1, ProvenanceImportManifestEntryV1,
        ProvenanceImportManifestPageV1, SCHEMA_VERSION, encode_history_fragment,
        history_manifest_sha256, provenance_manifest_sha256,
    };

    #[test]
    fn existing_only_open_never_initializes_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("git-sources");
        assert!(GitSourceStore::open_existing(&root, StoreLimits::default()).is_err());
        assert!(!root.exists());

        GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        GitSourceStore::open_existing(&root, StoreLimits::default()).unwrap();
    }

    fn fixture() -> (
        GitHistoryDescriptorV1,
        Vec<GitHistoryManifestEntryV1>,
        Vec<Vec<u8>>,
    ) {
        fixture_for('1', '2')
    }

    fn provenance_fixture() -> (
        ProvenanceImportDescriptorV1,
        Vec<ProvenanceImportManifestEntryV1>,
        Vec<String>,
    ) {
        let commits = ["1".repeat(40), "2".repeat(40)];
        let documents = commits
            .iter()
            .map(|commit| {
                serde_json::json!({
                    "schema_version": 1,
                    "commit": commit,
                    "produced_by": {},
                    "tool_calls": [],
                    "knowledge_writes": []
                })
                .to_string()
            })
            .collect::<Vec<_>>();
        let manifest = commits
            .iter()
            .zip(&documents)
            .map(|(commit, document)| ProvenanceImportManifestEntryV1 {
                note_commit: commit.clone(),
                document_ordinal: 0,
                encoded_bytes: document.len() as u64,
                document_sha256: sha256(document.as_bytes()),
            })
            .collect::<Vec<_>>();
        let descriptor = ProvenanceImportDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            notes_ref: "refs/notes/bbox/provenance".into(),
            notes_tip: "3".repeat(40),
            manifest_sha256: provenance_manifest_sha256(&manifest),
            document_count: manifest.len() as u64,
            logical_bytes: documents.iter().map(|document| document.len() as u64).sum(),
        };
        (descriptor, manifest, documents)
    }

    #[test]
    fn provenance_import_is_resumable_verified_and_journaled() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let project_id = "p_00000000000000000000000000000001";
        let (descriptor, manifest, documents) = provenance_fixture();
        let begin = store
            .begin_provenance_import("producer-a", project_id, descriptor)
            .unwrap();
        for (page, entry) in manifest.iter().enumerate() {
            let body = ProvenanceImportManifestPageV1 {
                entries: vec![entry.clone()],
            };
            store
                .put_provenance_manifest_page("producer-a", &begin.upload_id, page as u32, &body)
                .unwrap();
            store
                .put_provenance_manifest_page("producer-a", &begin.upload_id, page as u32, &body)
                .unwrap();
        }
        let missing = store
            .complete_provenance_manifest("producer-a", &begin.upload_id)
            .unwrap();
        assert_eq!(missing.hashes.len(), 2);
        for (entry, document) in manifest.iter().zip(&documents) {
            store
                .install_provenance_document(
                    "producer-a",
                    &begin.upload_id,
                    &entry.document_sha256,
                    entry.encoded_bytes,
                    document.as_bytes(),
                )
                .unwrap();
        }
        assert!(
            store
                .missing_provenance_documents("producer-a", &begin.upload_id, None)
                .unwrap()
                .hashes
                .is_empty()
        );
        let finalized = store
            .finalize_provenance_import("producer-a", &begin.upload_id)
            .unwrap();
        let verified = store
            .verified_provenance_import(&finalized.import_generation_id)
            .unwrap();
        let mut visited = Vec::new();
        store
            .visit_verified_provenance_documents(&verified, |document| {
                visited.push(document.document);
                Ok(())
            })
            .unwrap();
        assert_eq!(visited, documents);

        let journal = ProvenanceImportJournalV1::new_prepared(
            &verified,
            7,
            "collected:project:generation".into(),
        )
        .unwrap();
        let journal = store.save_provenance_import_journal(journal).unwrap();
        let mut published = journal.clone();
        published.stage = ProvenanceImportStageV1::EdgesPublished;
        published.edge_count = 4;
        published.edge_keys_sha256 = "a".repeat(64);
        store.save_provenance_import_journal(published).unwrap();
        assert_eq!(
            store
                .read_provenance_import_journal(project_id)
                .unwrap()
                .unwrap()
                .edge_count,
            4
        );
    }

    #[test]
    fn newest_accepted_provenance_generation_alone_becomes_active() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = GitSourceStore::open(
            &root,
            StoreLimits {
                retained_history_generations: 1,
                ..StoreLimits::default()
            },
        )
        .unwrap();
        let project_id = "p_00000000000000000000000000000001";
        let producer_id = "producer-a";
        let (first_descriptor, manifest, documents) = provenance_fixture();
        let finalize = |descriptor: ProvenanceImportDescriptorV1| {
            let begin = store
                .begin_provenance_import(producer_id, project_id, descriptor)
                .unwrap();
            store
                .put_provenance_manifest_page(
                    producer_id,
                    &begin.upload_id,
                    0,
                    &ProvenanceImportManifestPageV1 {
                        entries: manifest.clone(),
                    },
                )
                .unwrap();
            store
                .complete_provenance_manifest(producer_id, &begin.upload_id)
                .unwrap();
            for (entry, document) in manifest.iter().zip(&documents) {
                store
                    .install_provenance_document(
                        producer_id,
                        &begin.upload_id,
                        &entry.document_sha256,
                        entry.encoded_bytes,
                        document.as_bytes(),
                    )
                    .unwrap();
            }
            store
                .finalize_provenance_import(producer_id, &begin.upload_id)
                .unwrap()
                .import_generation_id
        };
        let first = finalize(first_descriptor.clone());
        let mut second_descriptor = first_descriptor;
        second_descriptor.notes_tip = "4".repeat(40);
        let second = finalize(second_descriptor.clone());

        assert_eq!(
            store
                .current_ready_provenance_import_id(project_id)
                .unwrap()
                .as_deref(),
            Some(second.as_str())
        );
        assert!(
            store
                .load_provenance_generation(&first)
                .unwrap()
                .accepted_sequence
                < store
                    .load_provenance_generation(&second)
                    .unwrap()
                    .accepted_sequence
        );

        store
            .transition_provenance_import(&first, ProvenanceImportStateV1::Importing, 0, None)
            .unwrap();
        assert_eq!(
            store.settle_provenance_import(&first, 2).unwrap().state,
            ProvenanceImportStateV1::Superseded
        );
        store
            .transition_provenance_import(&second, ProvenanceImportStateV1::Importing, 0, None)
            .unwrap();
        assert_eq!(
            store.settle_provenance_import(&second, 2).unwrap().state,
            ProvenanceImportStateV1::Active
        );

        let mut third_descriptor = second_descriptor;
        third_descriptor.notes_tip = "5".repeat(40);
        let third = finalize(third_descriptor);
        store
            .transition_provenance_import(&third, ProvenanceImportStateV1::Importing, 0, None)
            .unwrap();
        assert_eq!(
            store.settle_provenance_import(&third, 2).unwrap().state,
            ProvenanceImportStateV1::Active
        );
        assert_eq!(
            store
                .maintain(&BTreeSet::new())
                .unwrap()
                .retired_generations,
            1
        );
        assert!(store.load_provenance_generation(&first).is_err());
        assert!(store.load_provenance_generation(&second).is_ok());
        assert_eq!(
            store
                .current_ready_provenance_import_id(project_id)
                .unwrap()
                .as_deref(),
            Some(third.as_str())
        );
    }

    #[test]
    fn provenance_manifest_retry_with_different_bytes_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let (descriptor, manifest, _) = provenance_fixture();
        let begin = store
            .begin_provenance_import(
                "producer-a",
                "p_00000000000000000000000000000001",
                descriptor,
            )
            .unwrap();
        let page = ProvenanceImportManifestPageV1 {
            entries: vec![manifest[0].clone()],
        };
        store
            .put_provenance_manifest_page("producer-a", &begin.upload_id, 0, &page)
            .unwrap();
        let conflicting = ProvenanceImportManifestPageV1 {
            entries: vec![manifest[1].clone()],
        };
        assert!(
            store
                .put_provenance_manifest_page("producer-a", &begin.upload_id, 0, &conflicting,)
                .is_err()
        );
    }

    #[test]
    fn provenance_maintenance_expires_upload_and_reclaims_orphan_documents() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = GitSourceStore::open(
            &root,
            StoreLimits {
                unreferenced_record_grace_secs: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap();
        let (descriptor, manifest, documents) = provenance_fixture();
        let producer_id = "producer-a";
        let begin = store
            .begin_provenance_import(
                producer_id,
                "p_00000000000000000000000000000001",
                descriptor,
            )
            .unwrap();
        store
            .put_provenance_manifest_page(
                producer_id,
                &begin.upload_id,
                0,
                &ProvenanceImportManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        store
            .complete_provenance_manifest(producer_id, &begin.upload_id)
            .unwrap();
        for (entry, document) in manifest.iter().zip(&documents) {
            store
                .install_provenance_document(
                    producer_id,
                    &begin.upload_id,
                    &entry.document_sha256,
                    entry.encoded_bytes,
                    document.as_bytes(),
                )
                .unwrap();
        }
        let upload_path = store
            .provenance_upload_dir(producer_id, &begin.upload_id)
            .unwrap();
        let mut upload = store
            .load_provenance_upload(&upload_path, producer_id, &begin.upload_id)
            .unwrap();
        upload.updated_unix_secs = 0;
        let upload_directory = NofollowDirectory::open_existing(&upload_path)
            .unwrap()
            .unwrap();
        write_json(&upload_directory, "upload.json", &upload).unwrap();

        let report = store.maintain(&BTreeSet::new()).unwrap();
        assert_eq!(report.expired_uploads, 1);
        assert_eq!(report.deleted_records, 2);
        for entry in manifest {
            assert!(
                store
                    .read_provenance_document_bytes(
                        &entry.document_sha256,
                        entry.encoded_bytes as usize,
                    )
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn provenance_receipt_is_durable_and_identical_retry_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let project_id = "p_00000000000000000000000000000001";
        let receipt = ProvenanceExportReceiptV1 {
            schema_version: bbox_git_source::SCHEMA_VERSION,
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            generation: "a".repeat(64),
            notes_ref: "refs/notes/bbox/provenance".into(),
            document_count: 0,
            ordered_document_commitment: "b".repeat(64),
            local_notes_tip: String::new(),
            written: 0,
            unchanged: 0,
        };
        let first = store
            .record_provenance_export_receipt("producer-a", project_id, receipt.clone())
            .unwrap();
        let second = store
            .record_provenance_export_receipt("producer-a", project_id, receipt.clone())
            .unwrap();
        assert_eq!(first, second);
        drop(store);

        let reopened = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(
            reopened
                .provenance_export_receipt(project_id)
                .unwrap()
                .unwrap()
                .receipt,
            receipt
        );
        assert_eq!(reopened.provenance_export_receipts().unwrap().len(), 1);
    }

    fn fixture_for(
        root_digit: char,
        head_digit: char,
    ) -> (
        GitHistoryDescriptorV1,
        Vec<GitHistoryManifestEntryV1>,
        Vec<Vec<u8>>,
    ) {
        let root = root_digit.to_string().repeat(40);
        let head = head_digit.to_string().repeat(40);
        let fragments = [
            GitHistoryCommitFragmentV1 {
                commit_oid: root.clone(),
                fragment_index: 0,
                fragment_count: 1,
                header: Some(GitHistoryCommitHeaderV1 {
                    parent_oids: vec![],
                    author_name: "A".into(),
                    author_email: "a@example.invalid".into(),
                    message: "root".into(),
                }),
                changed_paths: vec!["README.md".into()],
            },
            GitHistoryCommitFragmentV1 {
                commit_oid: head.clone(),
                fragment_index: 0,
                fragment_count: 1,
                header: Some(GitHistoryCommitHeaderV1 {
                    parent_oids: vec![root],
                    author_name: "A".into(),
                    author_email: "a@example.invalid".into(),
                    message: "head".into(),
                }),
                changed_paths: vec!["src/lib.rs".into()],
            },
        ];
        let records = fragments
            .iter()
            .map(encode_history_fragment)
            .collect::<Vec<_>>();
        let manifest = fragments
            .iter()
            .zip(&records)
            .map(|(fragment, bytes)| GitHistoryManifestEntryV1 {
                commit_oid: fragment.commit_oid.clone(),
                fragment_index: 0,
                encoded_bytes: bytes.len() as u64,
                content_sha256: sha256(bytes),
            })
            .collect::<Vec<_>>();
        let descriptor = GitHistoryDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: PublishedScope::try_new("repo-a", ".").unwrap(),
            repo_head: head,
            object_format: GitObjectFormatV1::Sha1,
            manifest_sha256: history_manifest_sha256(&manifest),
            commit_count: 2,
            fragment_count: 2,
            logical_bytes: manifest.iter().map(|entry| entry.encoded_bytes).sum(),
        };
        (descriptor, manifest, records)
    }

    fn ingest_fixture(
        store: &GitSourceStore,
        history: &RepoHistoryId,
        namespace: &CommitNamespace,
        fixture: (
            GitHistoryDescriptorV1,
            Vec<GitHistoryManifestEntryV1>,
            Vec<Vec<u8>>,
        ),
    ) -> (String, String) {
        let (descriptor, manifest, records) = fixture;
        let begin = store
            .begin_history_upload("producer-a", history, namespace, descriptor)
            .unwrap();
        store
            .put_history_manifest_page(
                "producer-a",
                &begin.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        store
            .complete_history_manifest("producer-a", &begin.upload_id)
            .unwrap();
        for (entry, bytes) in manifest.iter().zip(records) {
            store
                .install_history_record(
                    "producer-a",
                    &begin.upload_id,
                    &entry.content_sha256,
                    entry.encoded_bytes,
                    std::io::Cursor::new(bytes),
                )
                .unwrap();
        }
        let finalized = store
            .finalize_history_upload("producer-a", &begin.upload_id)
            .unwrap();
        (begin.upload_id, finalized.source_generation_id)
    }

    fn set_generation_created(
        store: &GitSourceStore,
        history: &RepoHistoryId,
        generation: &str,
        created_unix_secs: u64,
    ) {
        let path = store.generation_dir(history, generation).unwrap();
        let mut source = read_json::<StoredHistorySourceV1>(
            &path,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "test Git-history source",
        )
        .unwrap()
        .unwrap();
        source.created_unix_secs = created_unix_secs;
        let directory = NofollowDirectory::open_existing(&path).unwrap().unwrap();
        write_json(&directory, "source.json", &source).unwrap();
    }

    fn stored_record_count(root: &Path) -> usize {
        read_directories(&root.join("records/sha256"))
            .unwrap()
            .into_iter()
            .map(|bucket| fs::read_dir(bucket).unwrap().count())
            .sum()
    }

    fn activation_journal(
        source: &VerifiedGitHistorySourceV1,
        history: &RepoHistoryId,
    ) -> HistoryActivationJournalV1 {
        let p3 = format!("rhg_{}", "a".repeat(64));
        HistoryActivationJournalV1 {
            version: 1,
            stage: HistoryActivationStageV1::Prepared,
            source_generation_id: source.source_generation_id.clone(),
            producer_id: source.producer_id.clone(),
            source_evidence: source.source_evidence.clone(),
            grant_commitment: "b".repeat(64),
            catalog_epoch_prepared: 7,
            catalog_epoch_after: None,
            repo_history_id: history.clone(),
            prior_p3_generation_id: None,
            planned_p3_generation_id: p3.clone(),
            planned_p3_manifest_sha256: "c".repeat(64),
            code_selectors: BTreeMap::from([("p_one".into(), "code-one".into())]),
            overlays: vec![HistoryActivationOverlayV1 {
                project_id: "p_one".into(),
                snapshot_id: "snapshot-one".into(),
                selector: GitOverlaySelector {
                    project_id: "p_one".into(),
                    code_generation: "code-one".into(),
                    repo_history_generation: p3,
                    source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::ProducerTransport {
                        producer_id: source.producer_id.clone(),
                        source_generation_id: source.source_generation_id.clone(),
                    },
                    repo_head: source.repo_head.clone(),
                    commit_namespace: source.primary_namespace.as_str().to_string(),
                    overlay_generation: 1,
                },
                file_commitment: None,
            }],
            overlay_clears: Vec::new(),
            commit_document_count: 2,
            commit_document_commitment_sha256: "d".repeat(64),
            vector_input_count: 2,
            vector_input_commitment_sha256: "e".repeat(64),
            commit_view_commitment: None,
            diagnostic: None,
            checksum_sha256: String::new(),
        }
    }

    #[test]
    fn resumable_history_intake_reaches_ready_and_survives_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (descriptor, manifest, records) = fixture();
        let begin = store
            .begin_history_upload("producer-a", &history, &namespace, descriptor.clone())
            .unwrap();
        store
            .put_history_manifest_page(
                "producer-a",
                &begin.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        // Exact page replay is a no-op.
        store
            .put_history_manifest_page(
                "producer-a",
                &begin.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        let missing = store
            .complete_history_manifest("producer-a", &begin.upload_id)
            .unwrap();
        assert_eq!(missing.hashes.len(), 2);
        for (entry, bytes) in manifest.iter().zip(&records) {
            store
                .install_history_record(
                    "producer-a",
                    &begin.upload_id,
                    &entry.content_sha256,
                    entry.encoded_bytes,
                    std::io::Cursor::new(bytes),
                )
                .unwrap();
        }
        assert!(
            store
                .missing_history_records("producer-a", &begin.upload_id, None)
                .unwrap()
                .hashes
                .is_empty()
        );
        let finalized = store
            .finalize_history_upload("producer-a", &begin.upload_id)
            .unwrap();
        assert_eq!(
            store
                .history_status("producer-a", &finalized.source_generation_id)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Ready
        );
        drop(store);

        let reopened = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert!(
            reopened
                .probe_ready_history(
                    "producer-a",
                    &history,
                    &descriptor.repo_head,
                    descriptor.object_format,
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn producer_binding_and_content_hashes_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (descriptor, manifest, records) = fixture();
        let begin = store
            .begin_history_upload("producer-a", &history, &namespace, descriptor)
            .unwrap();
        assert!(
            store
                .put_history_manifest_page(
                    "producer-b",
                    &begin.upload_id,
                    0,
                    &GitHistoryManifestPageV1 {
                        entries: manifest.clone(),
                    },
                )
                .is_err()
        );
        store
            .put_history_manifest_page(
                "producer-a",
                &begin.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        store
            .complete_history_manifest("producer-a", &begin.upload_id)
            .unwrap();
        let mut corrupt = records[0].clone();
        corrupt[0] ^= 1;
        assert!(
            store
                .install_history_record(
                    "producer-a",
                    &begin.upload_id,
                    &manifest[0].content_sha256,
                    manifest[0].encoded_bytes,
                    std::io::Cursor::new(corrupt),
                )
                .is_err()
        );
    }

    #[test]
    fn verified_handoff_streams_reconstructed_commits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (_, generation) = ingest_fixture(&store, &history, &namespace, fixture());
        let source = store
            .verified_history_source("producer-a", &generation)
            .unwrap();
        let mut observed = Vec::new();
        store
            .visit_verified_history_commits(&source, |commit| {
                observed.push((commit.commit.sha, commit.changed_paths));
                Ok(())
            })
            .unwrap();
        assert_eq!(
            observed,
            vec![
                ("1".repeat(40), vec!["README.md".to_string()]),
                ("2".repeat(40), vec!["src/lib.rs".to_string()]),
            ]
        );
    }

    #[test]
    fn activation_journal_is_monotonic_and_roots_its_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(
            &root,
            StoreLimits {
                retained_history_generations: 1,
                unreferenced_record_grace_secs: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (_, generation_one) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        set_generation_created(&store, &history, &generation_one, 1);
        let source = store
            .verified_history_source("producer-a", &generation_one)
            .unwrap();
        let mut journal = store
            .save_activation_journal(activation_journal(&source, &history))
            .unwrap();

        let mut skipped = journal.clone();
        skipped.stage = HistoryActivationStageV1::MaterializationAdvanced;
        skipped.catalog_epoch_after = Some(8);
        assert!(store.save_activation_journal(skipped).is_err());

        journal.stage = HistoryActivationStageV1::GenerationVerified;
        journal = store.save_activation_journal(journal).unwrap();
        let mut drifted = journal.clone();
        drifted
            .code_selectors
            .insert("p_one".into(), "foreign".into());
        assert!(store.save_activation_journal(drifted).is_err());

        journal.stage = HistoryActivationStageV1::MaterializationAdvanced;
        journal.catalog_epoch_after = Some(8);
        journal = store.save_activation_journal(journal).unwrap();
        let mut incomplete = journal.clone();
        incomplete.stage = HistoryActivationStageV1::CommitViewPublished;
        assert!(store.save_activation_journal(incomplete).is_err());
        journal.stage = HistoryActivationStageV1::CommitViewPublished;
        journal.commit_view_commitment = Some("d".repeat(64));
        let missing_receipt = journal.clone();
        assert!(store.save_activation_journal(missing_receipt).is_err());
        journal.overlays[0].file_commitment = Some("f".repeat(64));
        journal = store.save_activation_journal(journal).unwrap();
        journal.stage = HistoryActivationStageV1::OverlaysPublished;
        let mut invalid_receipt = journal.clone();
        invalid_receipt.overlays[0].file_commitment = Some("transient-txn-token".into());
        assert!(store.save_activation_journal(invalid_receipt).is_err());
        journal = store.save_activation_journal(journal).unwrap();
        journal.stage = HistoryActivationStageV1::Committed;
        journal = store.save_activation_journal(journal).unwrap();
        let mut backwards = journal.clone();
        backwards.stage = HistoryActivationStageV1::OverlaysPublished;
        assert!(store.save_activation_journal(backwards).is_err());

        let (_, generation_two) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        set_generation_created(&store, &history, &generation_two, 2);
        let (_, generation_three) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '4'));
        set_generation_created(&store, &history, &generation_three, 3);
        store.maintain(&BTreeSet::new()).unwrap();
        assert!(store.history_status("producer-a", &generation_one).is_ok());
        assert!(store.history_status("producer-a", &generation_two).is_ok());
        assert!(
            store
                .history_status("producer-a", &generation_three)
                .is_ok()
        );
    }

    #[test]
    fn committing_a_source_supersedes_only_older_active_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (_, first) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        let (_, second) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        let (_, pending) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '4'));
        for generation in [&first, &second] {
            store
                .set_history_source_state(
                    "producer-a",
                    generation,
                    GitHistorySourceStateV1::Materializing,
                    None,
                )
                .unwrap();
            store
                .set_history_source_state(
                    "producer-a",
                    generation,
                    GitHistorySourceStateV1::Publishing,
                    None,
                )
                .unwrap();
            store
                .set_history_source_state(
                    "producer-a",
                    generation,
                    GitHistorySourceStateV1::Active,
                    None,
                )
                .unwrap();
        }

        assert_eq!(
            store
                .supersede_other_active_history_sources(&history, &second)
                .unwrap(),
            1
        );
        assert_eq!(
            store.history_status("producer-a", &first).unwrap().state,
            GitHistorySourceStateV1::Superseded
        );
        assert_eq!(
            store.history_status("producer-a", &second).unwrap().state,
            GitHistorySourceStateV1::Active
        );
        assert_eq!(
            store.history_status("producer-a", &pending).unwrap().state,
            GitHistorySourceStateV1::Ready
        );
    }

    #[test]
    fn maintenance_preserves_pins_then_reclaims_expired_unreferenced_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(
            &root,
            StoreLimits {
                retained_history_generations: 1,
                unreferenced_record_grace_secs: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (upload_one, generation_one) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        set_generation_created(&store, &history, &generation_one, 1);
        let (upload_two, generation_two) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        set_generation_created(&store, &history, &generation_two, 2);
        let (upload_three, generation_three) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '4'));
        set_generation_created(&store, &history, &generation_three, 3);

        let protected = BTreeSet::from([generation_one.clone()]);
        let report = store.maintain(&protected).unwrap();
        assert_eq!(report.retired_generations, 0);
        assert!(store.history_status("producer-a", &generation_one).is_ok());
        assert!(store.history_status("producer-a", &generation_two).is_ok());
        assert!(
            store
                .history_status("producer-a", &generation_three)
                .is_ok()
        );

        let report = store.maintain(&BTreeSet::new()).unwrap();
        assert_eq!(report.retired_generations, 1);
        assert!(store.history_status("producer-a", &generation_one).is_err());
        assert!(store.history_status("producer-a", &generation_two).is_ok());
        assert!(
            store
                .history_status("producer-a", &generation_three)
                .is_ok()
        );

        let (upload_four, generation_four) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '5'));
        set_generation_created(&store, &history, &generation_four, 4);
        assert_eq!(stored_record_count(&root), 5);
        let report = store.maintain(&BTreeSet::new()).unwrap();
        assert_eq!(report.retired_generations, 1);
        assert!(store.history_status("producer-a", &generation_two).is_err());
        assert!(
            store
                .history_status("producer-a", &generation_three)
                .is_ok()
        );
        assert!(store.history_status("producer-a", &generation_four).is_ok());

        for upload_id in [upload_one, upload_two, upload_three, upload_four] {
            let upload_dir = store.upload_dir("producer-a", &upload_id).unwrap();
            let mut upload = store
                .load_upload(&upload_dir, "producer-a", &upload_id)
                .unwrap();
            upload.updated_unix_secs = 0;
            let directory = NofollowDirectory::open_existing(&upload_dir)
                .unwrap()
                .unwrap();
            write_json(&directory, "upload.json", &upload).unwrap();
        }
        drop(store);

        let reopened = GitSourceStore::open(
            &root,
            StoreLimits {
                retained_history_generations: 1,
                unreferenced_record_grace_secs: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap();
        let report = reopened.maintain(&BTreeSet::new()).unwrap();
        assert_eq!(report.expired_uploads, 4);
        assert_eq!(report.deleted_records, 2);
        assert!(report.deleted_record_bytes > 0);
        assert_eq!(stored_record_count(&root), 3);
        assert!(
            reopened
                .history_status("producer-a", &generation_three)
                .is_ok()
        );
        assert!(
            reopened
                .history_status("producer-a", &generation_four)
                .is_ok()
        );
    }
}
