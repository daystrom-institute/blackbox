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
    BeginGitHistoryUploadResponseV1, FinalizeGitHistoryUploadResponseV1, GitHistoryDescriptorV1,
    GitHistoryManifestEntryV1, GitHistoryManifestPageV1, GitHistorySourceStateV1,
    GitHistorySourceStatusV1, GitSourceLimits, HistorySourceVerifier,
    MAX_HISTORY_MANIFEST_PAGE_BYTES, MAX_HISTORY_MANIFEST_PAGE_ENTRIES, MAX_HISTORY_RECORD_BYTES,
    MissingHistoryRecordsPageV1, history_source_generation_id, validate_history_manifest,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MAX_UPLOAD_RECORD_BYTES: usize = 256 * 1024;
const MAX_GENERATION_RECORD_BYTES: usize = 256 * 1024;
const MAX_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MISSING_PAGE_SIZE: usize = 1_000;
const HISTORY_UPLOAD_IDLE_TTL_SECS: u64 = 24 * 60 * 60;
/// Regular files a repository history root may hold beside its generations.
const HISTORY_ROOT_FILES: &[&str] = &["current-ready.json", "acceptance-sequence.json"];

#[cfg(any(test, feature = "fault-injection"))]
thread_local! {
    static FINALIZE_FAILURE_POINT: std::cell::RefCell<Option<&'static str>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only: make the next history finalize on this thread fail right after
/// the named durable step, simulating a crash there. Points, in order:
/// `immutable-installed`, `acceptance-counter`, `acceptance-checkpoint`,
/// `generation-index`, `source-reopened`, `ready-pointer`, `upload-ready`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn fail_history_finalize_after(point: Option<&'static str>) {
    FINALIZE_FAILURE_POINT.with(|current| current.replace(point));
}

/// Simulate a crash immediately after one durable finalize step.
#[cfg(any(test, feature = "fault-injection"))]
fn inject_finalize_failure(point: &'static str) -> Result<()> {
    let fail = FINALIZE_FAILURE_POINT.with(|current| {
        if *current.borrow() == Some(point) {
            current.replace(None);
            true
        } else {
            false
        }
    });
    if fail {
        bail!("injected Git-history finalize failure after {point}");
    }
    Ok(())
}

#[cfg(not(any(test, feature = "fault-injection")))]
fn inject_finalize_failure(_point: &'static str) -> Result<()> {
    Ok(())
}

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
    /// Durable per-repository acceptance checkpoint for this upload attempt.
    /// Absent until a verified finalize accepts the upload, and absent on
    /// every record written before ordered acceptance existed; a completed
    /// upload without one is a legacy no-op and never gains one by replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_sequence: Option<u64>,
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

/// Durable operator-facing dead letter for one Git-history activation that
/// cannot converge without catalog or grant-table action. Keyed by repo
/// history id under `activation-deadletter/`; the background worker stops
/// redriving the recorded source generation until an operator drops the
/// record or a newer ready pointer supersedes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationDeadletterV1 {
    pub version: u32,
    pub repo_history_id: RepoHistoryId,
    pub producer_id: String,
    pub source_generation_id: String,
    pub error_code: String,
    pub first_seen_unix_secs: u64,
    pub last_seen_unix_secs: u64,
    pub attempts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl ActivationDeadletterV1 {
    fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION
            || self.error_code.is_empty()
            || self.error_code.len() > 128
            || self.attempts == 0
            || self.last_seen_unix_secs < self.first_seen_unix_secs
        {
            bail!(StoreRequestError::InvalidState);
        }
        validate_producer_authority(&self.producer_id)?;
        validate_generation_id(&self.source_generation_id)?;
        Ok(())
    }
}

/// One repository's durable ready pointer, as the offline CLI and admin
/// surfaces observe it. Pure listing view: retirement revalidates the
/// pointer against its generation under the mutation lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyPointerViewV1 {
    pub repo_history_id: RepoHistoryId,
    pub source_generation_id: String,
    pub producer_id: String,
    pub repo_head: String,
}

/// A repository whose ready pointer the validating loader refused. The
/// listing reports it beside the readable pointers instead of failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MalformedReadyPointerV1 {
    pub repo_history_id: RepoHistoryId,
    pub error: String,
}

/// Every repository's ready pointer: the ones that read, and the ones that
/// did not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadyPointerListingV1 {
    pub pointers: Vec<ReadyPointerViewV1>,
    pub malformed: Vec<MalformedReadyPointerV1>,
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
    /// Acceptance sequence and upload attempt that published this pointer.
    /// Both are absent on a legacy pointer, which is only a baseline: any
    /// acceptance allocated by an upgraded writer is newer than it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_upload_id: Option<String>,
}

/// Ordering evidence carried by a history ready pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryPointerAcceptance<'a> {
    Legacy,
    Accepted { sequence: u64, upload_id: &'a str },
}

impl ReadyPointerV1 {
    fn acceptance(&self) -> Result<HistoryPointerAcceptance<'_>> {
        match (self.accepted_sequence, self.accepted_upload_id.as_deref()) {
            (None, None) => Ok(HistoryPointerAcceptance::Legacy),
            (Some(sequence), Some(upload_id)) if sequence > 0 => {
                validate_upload_id(upload_id).map_err(|_| StoreRequestError::InvalidState)?;
                Ok(HistoryPointerAcceptance::Accepted {
                    sequence,
                    upload_id,
                })
            }
            _ => bail!(StoreRequestError::InvalidState),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryAcceptanceSequenceV1 {
    version: u32,
    next_sequence: u64,
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

struct MutationGuard<'a> {
    _anchor: StoreLockGuard,
    _in_process: MutexGuard<'a, ()>,
}

impl GitSourceStore {
    pub fn open(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        validate_store_limits(limits)?;
        let root = root.into();
        NofollowDirectory::open_or_create(&root)?;
        remove_unowned_trees(&root)?;
        for relative in [
            "uploads",
            "records",
            "records/sha256",
            "repos",
            "generation-index",
            "activations",
            "activation-deadletter",
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
    /// transport estate look initialized merely by inspecting it. The
    /// activation dead-letter area is deliberately absent from the required
    /// member list: stores written before it existed lack the directory, and
    /// dead-letter reads treat a missing area as empty rather than refusing.
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
                return Ok(begin_response(record.upload_id, record.state));
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
                accepted_sequence: None,
            },
        )?;
        Ok(begin_response(
            upload_id,
            GitHistorySourceStateV1::ReceivingManifest,
        ))
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

    /// Accept one fully verified upload.
    ///
    /// Immutable generation files are content-addressed evidence: an existing
    /// generation with the same identity, descriptor, and manifest is reused
    /// with its original creation time and lifecycle state. Each accepted
    /// upload attempt receives a durable per-repository acceptance sequence,
    /// checkpointed in its upload record before the ready pointer moves, so a
    /// retry resumes the same acceptance and an older attempt can finish
    /// without rewinding a newer pointer. Every step is idempotent, so a
    /// retry after a crash at any point converges.
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
        // Replaying a completed upload is a no-op; it never repoints the
        // repository, so a delayed retry cannot undo a newer acceptance.
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
        let existing = read_json::<StoredHistorySourceV1>(
            &generation_path,
            "source.json",
            MAX_GENERATION_RECORD_BYTES,
            "stored Git-history source",
        )?;
        if let Some(existing) = existing.as_ref()
            && (existing.version != STORE_VERSION
                || existing.source_generation_id != source_generation_id
                || existing.producer_id != producer_id
                || existing.repo_history_id != upload.repo_history_id
                || existing.primary_namespace != upload.primary_namespace
                || existing.descriptor != upload.descriptor)
        {
            bail!(StoreRequestError::InvalidInput);
        }
        let descriptor_present =
            immutable_json_matches(&generation_dir, "descriptor.json", &upload.descriptor)?;
        let manifest_present = immutable_json_matches(&generation_dir, "manifest.json", &manifest)?;
        if !descriptor_present {
            write_json(&generation_dir, "descriptor.json", &upload.descriptor)?;
        }
        if !manifest_present {
            write_json(&generation_dir, "manifest.json", &manifest)?;
        }
        let mut source = match existing {
            Some(existing) => existing,
            None => {
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
                write_json(&generation_dir, "source.json", &stored)?;
                stored
            }
        };
        inject_finalize_failure("immutable-installed")?;

        let accepted_sequence = match upload.accepted_sequence {
            Some(sequence) => sequence,
            None => {
                let sequence =
                    self.allocate_history_acceptance_sequence(&upload.repo_history_id)?;
                inject_finalize_failure("acceptance-counter")?;
                upload.accepted_sequence = Some(sequence);
                upload.updated_unix_secs = now_unix_secs();
                write_json(&upload_directory, "upload.json", &upload)?;
                inject_finalize_failure("acceptance-checkpoint")?;
                sequence
            }
        };

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
        inject_finalize_failure("generation-index")?;

        let history_path = self.repo_history_root(&upload.repo_history_id)?;
        let current = load_history_ready_pointer(&history_path)?;
        let (wins, publish) = match current.as_ref() {
            None => (true, true),
            Some(pointer) => match pointer.acceptance()? {
                HistoryPointerAcceptance::Legacy => (true, true),
                HistoryPointerAcceptance::Accepted { sequence, .. }
                    if sequence < accepted_sequence =>
                {
                    (true, true)
                }
                HistoryPointerAcceptance::Accepted {
                    sequence,
                    upload_id: accepted_upload_id,
                } if sequence == accepted_sequence => {
                    if accepted_upload_id != upload_id
                        || pointer.source_generation_id != source_generation_id
                        || pointer.producer_id != producer_id
                    {
                        bail!(StoreRequestError::InvalidState);
                    }
                    (true, false)
                }
                HistoryPointerAcceptance::Accepted { .. } => (false, false),
            },
        };
        // Only the winning acceptance reopens a terminal source; activation
        // then re-plans against the current catalog, grant, and code state.
        // In-flight and Active lifecycle states are preserved as they are.
        // The reopen is durable before the pointer names the source, so an
        // interrupted acceptance never publishes a pointer to a source that
        // probes as current yet cannot activate. A crash in between leaves
        // the old pointer, so the producer resumes this upload and retries.
        if wins
            && matches!(
                source.state,
                GitHistorySourceStateV1::Superseded | GitHistorySourceStateV1::Failed
            )
        {
            source.state = GitHistorySourceStateV1::Ready;
            source.diagnostic = None;
            write_json(&generation_dir, "source.json", &source)?;
            inject_finalize_failure("source-reopened")?;
        }
        if publish {
            let history_root = NofollowDirectory::open_existing(&history_path)?
                .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
            write_json(
                &history_root,
                "current-ready.json",
                &ReadyPointerV1 {
                    version: STORE_VERSION,
                    source_generation_id: source_generation_id.clone(),
                    producer_id: producer_id.to_string(),
                    repo_head: upload.descriptor.repo_head.clone(),
                    accepted_sequence: Some(accepted_sequence),
                    accepted_upload_id: Some(upload_id.to_string()),
                },
            )?;
            inject_finalize_failure("ready-pointer")?;
        }

        upload.state = GitHistorySourceStateV1::Ready;
        upload.updated_unix_secs = now_unix_secs();
        write_json(&upload_directory, "upload.json", &upload)?;
        inject_finalize_failure("upload-ready")?;
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
        let Some(pointer) = load_history_ready_pointer(&history_root)? else {
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
            if let Some(pointer) = load_history_ready_pointer(&history_dir)? {
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
        let Some(pointer) = load_history_ready_pointer(&history_dir)? else {
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

    /// Every repository's ready pointer as a listing view, each read through
    /// the validating loader. A pointer the loader refuses is reported under
    /// `malformed` with its repository and the reason, and the listing goes
    /// on to the other repositories. Retirement and activation also
    /// revalidate a pointer against its generation under the mutation lock;
    /// this read does not.
    pub fn current_ready_pointers(&self) -> Result<ReadyPointerListingV1> {
        let mut listing = ReadyPointerListingV1::default();
        for repo_dir in read_directories(&self.root.join("repos"))? {
            let Some(repo_history_id) = repo_dir
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|raw| RepoHistoryId::parse(raw).ok())
            else {
                continue;
            };
            let history_dir = repo_dir.join("history");
            if NofollowDirectory::open_existing(&history_dir)?.is_none() {
                continue;
            }
            match load_history_ready_pointer(&history_dir) {
                Ok(Some(pointer)) => listing.pointers.push(ReadyPointerViewV1 {
                    repo_history_id,
                    source_generation_id: pointer.source_generation_id,
                    producer_id: pointer.producer_id,
                    repo_head: pointer.repo_head,
                }),
                Ok(None) => {}
                Err(error) => listing.malformed.push(MalformedReadyPointerV1 {
                    repo_history_id,
                    error: format!("ready pointer is malformed or unreadable: {error:#}"),
                }),
            }
        }
        listing
            .pointers
            .sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
        listing
            .malformed
            .sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
        Ok(listing)
    }

    /// Retire one repository's ready pointer under the same lock and
    /// protection discipline as maintenance: the pointer must pass the
    /// validating loader and still name an installed generation it agrees
    /// with (a malformed pointer is refused, never removed), the pointed
    /// generation is marked
    /// `Superseded` so its state stops claiming readiness, and the pointer
    /// file itself is removed as a durable regular file. Returns the retired
    /// source generation id, or `None` when no pointer existed.
    pub fn retire_current_ready_pointer(
        &self,
        repo_history_id: &RepoHistoryId,
    ) -> Result<Option<String>> {
        let _guard = self.lock_mutation()?;
        let history_dir = self.repo_history_root(repo_history_id)?;
        let Some(pointer) = load_history_ready_pointer(&history_dir).with_context(|| {
            format!(
                "ready pointer of repository {} is malformed or unreadable",
                repo_history_id.as_str()
            )
        })?
        else {
            return Ok(None);
        };
        let source = self.load_generation(repo_history_id, &pointer.source_generation_id)?;
        if source.producer_id != pointer.producer_id
            || source.descriptor.repo_head != pointer.repo_head
        {
            bail!("Git-history ready pointer disagrees with its generation");
        }
        remove_regular_file_if_present(&history_dir.join("current-ready.json"))?;
        if source.state != GitHistorySourceStateV1::Superseded {
            self.set_history_source_state_locked(
                &pointer.producer_id,
                &pointer.source_generation_id,
                GitHistorySourceStateV1::Superseded,
                Some(
                    "ready pointer retired by operator alongside its activation dead letter"
                        .to_string(),
                ),
            )?;
        }
        Ok(Some(pointer.source_generation_id))
    }

    pub fn read_activation_deadletter(
        &self,
        repo_history_id: &RepoHistoryId,
    ) -> Result<Option<ActivationDeadletterV1>> {
        let deadletter = read_json::<ActivationDeadletterV1>(
            &self.root.join("activation-deadletter"),
            &format!("{}.json", repo_history_id.as_str()),
            MAX_GENERATION_RECORD_BYTES,
            "Git-history activation dead letter",
        )?;
        if let Some(deadletter) = deadletter.as_ref() {
            deadletter.validate()?;
            if &deadletter.repo_history_id != repo_history_id {
                bail!(StoreRequestError::InvalidState);
            }
        }
        Ok(deadletter)
    }

    pub fn list_activation_deadletters(&self) -> Result<Vec<ActivationDeadletterV1>> {
        let root = self.root.join("activation-deadletter");
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
            _ => bail!("refusing non-directory Git-source dead-letter area"),
        }
        let mut deadletters = Vec::new();
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
            let Some(deadletter) = read_json::<ActivationDeadletterV1>(
                &root,
                &name,
                MAX_GENERATION_RECORD_BYTES,
                "Git-history activation dead letter",
            )?
            else {
                continue;
            };
            deadletter.validate()?;
            if name != format!("{}.json", deadletter.repo_history_id.as_str()) {
                bail!(StoreRequestError::InvalidState);
            }
            deadletters.push(deadletter);
        }
        deadletters.sort_by(|left, right| left.repo_history_id.cmp(&right.repo_history_id));
        Ok(deadletters)
    }

    /// Record or refresh one repository's activation dead letter. The record
    /// is keyed by repo history id: a repeat failure of the same source
    /// accumulates attempts and preserves `first_seen`, while a newer source
    /// generation replaces the recorded target. The worker consults the
    /// record before every redrive.
    pub fn record_activation_deadletter(
        &self,
        repo_history_id: &RepoHistoryId,
        producer_id: &str,
        source_generation_id: &str,
        error_code: &str,
        diagnostic: Option<String>,
    ) -> Result<ActivationDeadletterV1> {
        let _guard = self.lock_mutation()?;
        let diagnostic = diagnostic.map(|value| value.chars().take(512).collect::<String>());
        let now = now_unix_secs();
        let deadletter = match self.read_activation_deadletter(repo_history_id)? {
            Some(mut prior) => {
                prior.producer_id = producer_id.to_string();
                prior.source_generation_id = source_generation_id.to_string();
                prior.error_code = error_code.to_string();
                prior.last_seen_unix_secs = now;
                prior.attempts = prior.attempts.saturating_add(1);
                prior.diagnostic = diagnostic;
                prior
            }
            None => ActivationDeadletterV1 {
                version: STORE_VERSION,
                repo_history_id: repo_history_id.clone(),
                producer_id: producer_id.to_string(),
                source_generation_id: source_generation_id.to_string(),
                error_code: error_code.to_string(),
                first_seen_unix_secs: now,
                last_seen_unix_secs: now,
                attempts: 1,
                diagnostic,
            },
        };
        deadletter.validate()?;
        let area = NofollowDirectory::open_or_create(&self.root.join("activation-deadletter"))?;
        write_json(
            &area,
            &format!("{}.json", repo_history_id.as_str()),
            &deadletter,
        )?;
        Ok(deadletter)
    }

    /// Remove one repository's activation dead letter. Returns whether a
    /// record existed; the worker redrives the repository's current source on
    /// its next tick once the record is gone.
    pub fn drop_activation_deadletter(&self, repo_history_id: &RepoHistoryId) -> Result<bool> {
        let _guard = self.lock_mutation()?;
        let path = self
            .root
            .join("activation-deadletter")
            .join(format!("{}.json", repo_history_id.as_str()));
        let existed = matches!(fs::symlink_metadata(&path), Ok(metadata) if metadata.is_file());
        remove_regular_file_if_present(&path)?;
        Ok(existed)
    }

    pub fn set_history_source_state(
        &self,
        producer_id: &str,
        source_generation_id: &str,
        next: GitHistorySourceStateV1,
        diagnostic: Option<String>,
    ) -> Result<StoredHistorySourceV1> {
        let _guard = self.lock_mutation()?;
        self.set_history_source_state_locked(producer_id, source_generation_id, next, diagnostic)
    }

    fn set_history_source_state_locked(
        &self,
        producer_id: &str,
        source_generation_id: &str,
        next: GitHistorySourceStateV1,
        diagnostic: Option<String>,
    ) -> Result<StoredHistorySourceV1> {
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
        for generation_dir in read_child_directories(&history_dir, HISTORY_ROOT_FILES)? {
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
        validate_upload_acceptance(&record)?;
        Ok(record)
    }

    /// Allocate the next acceptance sequence for one repository.
    ///
    /// The durable counter is the primary high-water mark. The current
    /// pointer and retained upload checkpoints can only raise it, so a
    /// missing or lagging counter never re-issues a sequence already used,
    /// and a counter is never moved backwards.
    fn allocate_history_acceptance_sequence(&self, repo_history_id: &RepoHistoryId) -> Result<u64> {
        let history_path = self.repo_history_root(repo_history_id)?;
        let mut next = match read_json::<HistoryAcceptanceSequenceV1>(
            &history_path,
            "acceptance-sequence.json",
            MAX_GENERATION_RECORD_BYTES,
            "Git-history acceptance sequence",
        )? {
            Some(counter) if counter.version == STORE_VERSION && counter.next_sequence > 0 => {
                counter.next_sequence
            }
            Some(_) => bail!(StoreRequestError::InvalidState),
            None => 1,
        };
        let above = |sequence: u64| {
            sequence
                .checked_add(1)
                .ok_or_else(|| anyhow!(StoreRequestError::LimitExceeded))
        };
        if let Some(pointer) = load_history_ready_pointer(&history_path)?
            && let HistoryPointerAcceptance::Accepted { sequence, .. } = pointer.acceptance()?
        {
            next = next.max(above(sequence)?);
        }
        for producer_dir in read_directories(&self.root.join("uploads"))? {
            for upload_dir in read_directories(&producer_dir)? {
                let Some(upload) = read_json::<HistoryUploadRecordV1>(
                    &upload_dir,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "Git-history upload record",
                )?
                else {
                    continue;
                };
                validate_upload_acceptance(&upload)?;
                if upload.repo_history_id == *repo_history_id
                    && let Some(sequence) = upload.accepted_sequence
                {
                    next = next.max(above(sequence)?);
                }
            }
        }
        let directory = NofollowDirectory::open_existing(&history_path)?
            .ok_or_else(|| anyhow!(StoreRequestError::InvalidState))?;
        write_json(
            &directory,
            "acceptance-sequence.json",
            &HistoryAcceptanceSequenceV1 {
                version: STORE_VERSION,
                next_sequence: above(next)?,
            },
        )?;
        Ok(next)
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
            let current = load_history_ready_pointer(&history_dir)?;
            let mut sources = Vec::new();
            for generation_dir in read_child_directories(&history_dir, HISTORY_ROOT_FILES)? {
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
            for generation_dir in read_child_directories(&history_dir, HISTORY_ROOT_FILES)? {
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
        let expired_uploads = self.expire_stale_uploads(now)?;
        let mut protected_generation_ids = protected_generation_ids.clone();
        protected_generation_ids.extend(self.activation_source_roots()?);
        let retired_generations = self.retire_old_generations(
            &protected_generation_ids,
            limits.retained_history_generations,
        )?;
        let referenced_records = self.referenced_record_hashes(limits.contract)?;
        let (deleted_records, deleted_record_bytes) = self.sweep_unreferenced_records(
            &referenced_records,
            now,
            limits.unreferenced_record_grace_secs,
        )?;
        Ok(MaintenanceReport {
            expired_uploads,
            retired_generations,
            deleted_records,
            deleted_record_bytes,
        })
    }
}

/// Top-level trees that are not part of this store's layout. `open` removes
/// them so their bytes are reclaimed; nothing reads them.
const UNOWNED_TREES: [&str; 2] = ["provenance-receipts", "provenance-imports"];

// Runs inside `GitSourceStore::open`, which the daemon calls at startup and
// never on a tokio worker.
#[allow(clippy::disallowed_methods)]
fn remove_unowned_trees(root: &Path) -> Result<()> {
    for name in UNOWNED_TREES {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?
            }
            Ok(_) => {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        sync_parent(&path)?;
    }
    Ok(())
}

fn begin_response(
    upload_id: String,
    state: GitHistorySourceStateV1,
) -> BeginGitHistoryUploadResponseV1 {
    BeginGitHistoryUploadResponseV1 {
        upload_id,
        max_page_entries: MAX_HISTORY_MANIFEST_PAGE_ENTRIES,
        max_page_bytes: MAX_HISTORY_MANIFEST_PAGE_BYTES,
        max_record_bytes: MAX_HISTORY_RECORD_BYTES,
        state,
    }
}

fn validate_store_limits(limits: StoreLimits) -> Result<()> {
    if limits.max_open_uploads_per_producer == 0
        || limits.retained_history_generations == 0
        || limits.contract.max_history_commits == 0
        || limits.contract.max_history_logical_bytes == 0
    {
        bail!(StoreRequestError::LimitExceeded);
    }
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

/// `true` when an immutable member already holds exactly `value`, `false`
/// when it is absent; any other content is a conflict that fails closed.
fn immutable_json_matches<T: DeserializeOwned + PartialEq>(
    directory: &NofollowDirectory,
    name: &str,
    value: &T,
) -> Result<bool> {
    let Some(bytes) =
        directory.read_regular(name, MAX_MANIFEST_BYTES, "immutable Git-source member")?
    else {
        return Ok(false);
    };
    let existing: T = serde_json::from_slice(&bytes)?;
    if &existing != value {
        bail!(StoreRequestError::InvalidInput);
    }
    Ok(true)
}

fn load_history_ready_pointer(history_dir: &Path) -> Result<Option<ReadyPointerV1>> {
    let Some(pointer) = read_json::<ReadyPointerV1>(
        history_dir,
        "current-ready.json",
        MAX_GENERATION_RECORD_BYTES,
        "Git-history ready pointer",
    )?
    else {
        return Ok(None);
    };
    if pointer.version != STORE_VERSION {
        bail!(StoreRequestError::InvalidState);
    }
    validate_generation_id(&pointer.source_generation_id)
        .map_err(|_| StoreRequestError::InvalidState)?;
    pointer.acceptance()?;
    Ok(Some(pointer))
}

/// An acceptance checkpoint exists only on a verified upload, and zero is
/// never allocated; anything else is malformed and fails closed.
fn validate_upload_acceptance(upload: &HistoryUploadRecordV1) -> Result<()> {
    match upload.accepted_sequence {
        None => Ok(()),
        Some(0) => bail!(StoreRequestError::InvalidState),
        Some(_)
            if matches!(
                upload.state,
                GitHistorySourceStateV1::MissingRecords | GitHistorySourceStateV1::Ready
            ) =>
        {
            Ok(())
        }
        Some(_) => bail!(StoreRequestError::InvalidState),
    }
}

fn validate_upload_id(value: &str) -> Result<()> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(StoreRequestError::NotFound);
    }
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

fn validate_generation_id(value: &str) -> Result<()> {
    let Some(digest) = value.strip_prefix("ghs_") else {
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
        GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1, GitObjectFormatV1, SCHEMA_VERSION,
        encode_history_fragment, history_manifest_sha256,
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

    /// A store root that carries trees outside the store layout: open
    /// removes them and leaves every owned member in place.
    #[test]
    fn open_removes_unowned_trees_and_keeps_owned_members() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        ingest_fixture(&store, &history, &namespace, fixture());
        drop(store);
        let owned_records = stored_record_count(&root);
        assert!(owned_records > 0, "the owned record tree must be populated");
        for tree in UNOWNED_TREES {
            let nested = root.join(tree).join("nested");
            fs::create_dir_all(&nested).unwrap();
            fs::write(nested.join("member.json"), b"{}").unwrap();
        }
        fs::remove_dir_all(root.join(UNOWNED_TREES[1])).unwrap();
        fs::write(root.join(UNOWNED_TREES[1]), b"stray").unwrap();

        GitSourceStore::open(&root, StoreLimits::default()).unwrap();

        for tree in UNOWNED_TREES {
            assert!(
                fs::symlink_metadata(root.join(tree)).is_err(),
                "{tree} must be removed"
            );
        }
        assert_eq!(stored_record_count(&root), owned_records);
        GitSourceStore::open_existing(&root, StoreLimits::default()).unwrap();
    }

    /// A store that never carried unowned trees opens unchanged, and a
    /// repeated open over an already clean store is a no-op.
    #[test]
    fn open_over_a_clean_store_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("git-sources");
        GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let listing = |root: &Path| {
            let mut names = fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let first = listing(&root);
        assert!(first.contains(&"records".to_string()));
        GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        assert_eq!(listing(&root), first);
        for tree in UNOWNED_TREES {
            assert!(!first.contains(&tree.to_string()));
        }
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
        let upload_id = upload_to_missing_records(store, history, namespace, fixture);
        let finalized = store
            .finalize_history_upload("producer-a", &upload_id)
            .unwrap();
        (upload_id, finalized.source_generation_id)
    }

    /// Drive one upload to the point where only finalize remains.
    fn upload_to_missing_records(
        store: &GitSourceStore,
        history: &RepoHistoryId,
        namespace: &CommitNamespace,
        fixture: (
            GitHistoryDescriptorV1,
            Vec<GitHistoryManifestEntryV1>,
            Vec<Vec<u8>>,
        ),
    ) -> String {
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
        begin.upload_id
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

    #[test]
    fn activation_deadletter_records_accumulate_and_drop() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000007").unwrap();
        let (_, generation) = ingest_fixture(
            &store,
            &history,
            &CommitNamespace::parse("repo-a").unwrap(),
            fixture_for('1', '2'),
        );

        let first = store
            .record_activation_deadletter(
                &history,
                "producer-a",
                &generation,
                "repo_history_not_found",
                Some("no published project binds this repo history".to_string()),
            )
            .unwrap();
        assert_eq!(first.attempts, 1);
        assert_eq!(first.first_seen_unix_secs, first.last_seen_unix_secs);

        let second = store
            .record_activation_deadletter(
                &history,
                "producer-a",
                &generation,
                "repo_history_not_found",
                Some("no published project binds this repo history".to_string()),
            )
            .unwrap();
        assert_eq!(second.attempts, 2);
        assert_eq!(second.first_seen_unix_secs, first.first_seen_unix_secs);
        assert!(second.last_seen_unix_secs >= first.last_seen_unix_secs);

        let listed = store.list_activation_deadletters().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], second);
        assert_eq!(
            store.read_activation_deadletter(&history).unwrap(),
            Some(second)
        );

        assert!(store.drop_activation_deadletter(&history).unwrap());
        assert!(!store.drop_activation_deadletter(&history).unwrap());
        assert!(store.list_activation_deadletters().unwrap().is_empty());
        assert!(
            store
                .read_activation_deadletter(&history)
                .unwrap()
                .is_none()
        );

        // A dropped record starts fresh rather than resurrecting history.
        let revived = store
            .record_activation_deadletter(
                &history,
                "producer-a",
                &generation,
                "repo_history_not_found",
                None,
            )
            .unwrap();
        assert_eq!(revived.attempts, 1);
    }

    #[test]
    fn deadletter_area_is_optional_for_existing_stores() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        drop(store);
        // A store written before the dead-letter area existed has no such
        // directory; observational opens and reads must treat it as empty.
        std::fs::remove_dir(root.join("activation-deadletter")).unwrap();
        let reopened = GitSourceStore::open_existing(&root, StoreLimits::default()).unwrap();
        assert!(reopened.list_activation_deadletters().unwrap().is_empty());
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000008").unwrap();
        assert!(
            reopened
                .read_activation_deadletter(&history)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retire_current_ready_pointer_supersedes_the_pointed_generation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = GitSourceStore::open(&root, StoreLimits::default()).unwrap();
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000009").unwrap();
        let namespace = CommitNamespace::parse("repo-a").unwrap();
        let (_, generation) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));

        let pointers = store.current_ready_pointers().unwrap().pointers;
        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].repo_history_id, history);
        assert_eq!(pointers[0].source_generation_id, generation);
        assert_eq!(pointers[0].producer_id, "producer-a");

        let retired = store.retire_current_ready_pointer(&history).unwrap();
        assert_eq!(retired.as_deref(), Some(generation.as_str()));
        assert!(store.current_ready_pointers().unwrap().pointers.is_empty());
        assert!(store.current_ready_source_ids().unwrap().is_empty());
        assert_eq!(
            store
                .history_status("producer-a", &generation)
                .unwrap()
                .state,
            GitHistorySourceStateV1::Superseded
        );
        assert!(
            store
                .history_status("producer-a", &generation)
                .unwrap()
                .diagnostic
                .is_some()
        );

        // Retiring again is an idempotent no-op.
        assert_eq!(store.retire_current_ready_pointer(&history).unwrap(), None);
    }

    const HISTORY: &str = "rh_00000000000000000000000000000001";

    fn history_store(root: &Path) -> GitSourceStore {
        GitSourceStore::open(root, StoreLimits::default()).unwrap()
    }

    fn history_ids() -> (RepoHistoryId, CommitNamespace) {
        (
            RepoHistoryId::parse(HISTORY).unwrap(),
            CommitNamespace::parse("repo-a").unwrap(),
        )
    }

    fn request_error(error: &anyhow::Error) -> Option<StoreRequestError> {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreRequestError>())
            .copied()
    }

    fn rewrite_source(
        store: &GitSourceStore,
        history: &RepoHistoryId,
        generation: &str,
        edit: impl FnOnce(&mut StoredHistorySourceV1),
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
        edit(&mut source);
        let directory = NofollowDirectory::open_existing(&path).unwrap().unwrap();
        write_json(&directory, "source.json", &source).unwrap();
    }

    fn stored_source(
        store: &GitSourceStore,
        history: &RepoHistoryId,
        generation: &str,
    ) -> StoredHistorySourceV1 {
        store.load_generation(history, generation).unwrap()
    }

    fn ready_pointer(store: &GitSourceStore, history: &RepoHistoryId) -> ReadyPointerV1 {
        load_history_ready_pointer(&store.repo_history_root(history).unwrap())
            .unwrap()
            .unwrap()
    }

    fn write_ready_pointer(store: &GitSourceStore, history: &RepoHistoryId, raw: &str) {
        NofollowDirectory::open_existing(&store.repo_history_root(history).unwrap())
            .unwrap()
            .unwrap()
            .atomic_replace("current-ready.json", raw.as_bytes())
            .unwrap();
    }

    fn upload_record(store: &GitSourceStore, upload_id: &str) -> HistoryUploadRecordV1 {
        let upload_dir = store.upload_dir("producer-a", upload_id).unwrap();
        store
            .load_upload(&upload_dir, "producer-a", upload_id)
            .unwrap()
    }

    fn rewrite_upload_raw(store: &GitSourceStore, upload_id: &str, raw: &str) {
        NofollowDirectory::open_existing(&store.upload_dir("producer-a", upload_id).unwrap())
            .unwrap()
            .unwrap()
            .atomic_replace("upload.json", raw.as_bytes())
            .unwrap();
    }

    fn acceptance_counter(store: &GitSourceStore, history: &RepoHistoryId) -> Option<u64> {
        read_json::<HistoryAcceptanceSequenceV1>(
            &store.repo_history_root(history).unwrap(),
            "acceptance-sequence.json",
            MAX_GENERATION_RECORD_BYTES,
            "test acceptance sequence",
        )
        .unwrap()
        .map(|counter| counter.next_sequence)
    }

    fn accepted(pointer: &ReadyPointerV1) -> (u64, String) {
        match pointer.acceptance().unwrap() {
            HistoryPointerAcceptance::Accepted {
                sequence,
                upload_id,
            } => (sequence, upload_id.to_string()),
            HistoryPointerAcceptance::Legacy => panic!("pointer carries no acceptance"),
        }
    }

    /// Every upload checkpoint for the repository is unique: an acceptance
    /// sequence is never issued twice.
    fn assert_unique_acceptances(store: &GitSourceStore) {
        let mut seen = BTreeSet::new();
        for producer_dir in read_directories(&store.root.join("uploads")).unwrap() {
            for upload_dir in read_directories(&producer_dir).unwrap() {
                let upload = read_json::<HistoryUploadRecordV1>(
                    &upload_dir,
                    "upload.json",
                    MAX_UPLOAD_RECORD_BYTES,
                    "test upload record",
                )
                .unwrap()
                .unwrap();
                if let Some(sequence) = upload.accepted_sequence {
                    assert!(seen.insert(sequence), "sequence {sequence} issued twice");
                }
            }
        }
    }

    fn fail_finalize_at(point: &'static str) {
        FINALIZE_FAILURE_POINT.with(|current| current.replace(Some(point)));
    }

    fn clear_finalize_failure() {
        FINALIZE_FAILURE_POINT.with(|current| current.replace(None));
    }

    #[test]
    fn begin_reports_the_persisted_state_of_a_resumed_upload() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();
        let (descriptor, manifest, records) = fixture();
        let begin = |descriptor: GitHistoryDescriptorV1| {
            store
                .begin_history_upload("producer-a", &history, &namespace, descriptor)
                .unwrap()
        };
        let first = begin(descriptor.clone());
        assert_eq!(first.state, GitHistorySourceStateV1::ReceivingManifest);
        store
            .put_history_manifest_page(
                "producer-a",
                &first.upload_id,
                0,
                &GitHistoryManifestPageV1 {
                    entries: manifest.clone(),
                },
            )
            .unwrap();
        let resumed = begin(descriptor.clone());
        assert_eq!(resumed.upload_id, first.upload_id);
        assert_eq!(resumed.state, GitHistorySourceStateV1::ReceivingManifest);
        store
            .complete_history_manifest("producer-a", &first.upload_id)
            .unwrap();
        let resumed = begin(descriptor.clone());
        assert_eq!(resumed.upload_id, first.upload_id);
        assert_eq!(resumed.state, GitHistorySourceStateV1::MissingRecords);
        // A page PUT is refused once the manifest is complete; complete
        // itself stays idempotent, which is what a resumed producer calls.
        assert_eq!(
            request_error(
                &store
                    .put_history_manifest_page(
                        "producer-a",
                        &first.upload_id,
                        0,
                        &GitHistoryManifestPageV1 {
                            entries: manifest.clone(),
                        },
                    )
                    .unwrap_err()
            ),
            Some(StoreRequestError::InvalidState)
        );
        store
            .complete_history_manifest("producer-a", &first.upload_id)
            .unwrap();
        for (entry, bytes) in manifest.iter().zip(records) {
            store
                .install_history_record(
                    "producer-a",
                    &first.upload_id,
                    &entry.content_sha256,
                    entry.encoded_bytes,
                    std::io::Cursor::new(bytes),
                )
                .unwrap();
        }
        store
            .finalize_history_upload("producer-a", &first.upload_id)
            .unwrap();
        // A completed upload is never resumed: begin opens a fresh one.
        let fresh = begin(descriptor);
        assert_ne!(fresh.upload_id, first.upload_id);
        assert_eq!(fresh.state, GitHistorySourceStateV1::ReceivingManifest);
    }

    #[test]
    fn refinalize_reuses_existing_generation_in_every_lifecycle_state() {
        for state in [
            GitHistorySourceStateV1::Ready,
            GitHistorySourceStateV1::Active,
            GitHistorySourceStateV1::Materializing,
            GitHistorySourceStateV1::Publishing,
            GitHistorySourceStateV1::Superseded,
            GitHistorySourceStateV1::Failed,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
            let (history, namespace) = history_ids();
            let (_, generation_a) =
                ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
            let (_, generation_b) =
                ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
            let diagnostic = matches!(
                state,
                GitHistorySourceStateV1::Superseded | GitHistorySourceStateV1::Failed
            )
            .then(|| "obsolete diagnostic".to_string());
            rewrite_source(&store, &history, &generation_a, |source| {
                source.state = state;
                source.created_unix_secs = 11;
                source.diagnostic = diagnostic.clone();
            });

            // HEAD returns to A: a fresh upload of the retained generation.
            let (upload, generation) =
                ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
            assert_eq!(generation, generation_a, "{state:?}");
            let source = stored_source(&store, &history, &generation_a);
            assert_eq!(source.created_unix_secs, 11, "{state:?}");
            let reopened = matches!(
                state,
                GitHistorySourceStateV1::Superseded | GitHistorySourceStateV1::Failed
            );
            if reopened {
                assert_eq!(source.state, GitHistorySourceStateV1::Ready, "{state:?}");
                assert_eq!(source.diagnostic, None, "{state:?}");
            } else {
                assert_eq!(source.state, state, "in-flight state must be preserved");
                assert_eq!(source.diagnostic, None, "{state:?}");
            }
            let pointer = ready_pointer(&store, &history);
            assert_eq!(pointer.source_generation_id, generation_a, "{state:?}");
            assert_eq!(accepted(&pointer), (3, upload.clone()), "{state:?}");
            assert_eq!(upload_record(&store, &upload).accepted_sequence, Some(3));
            assert_eq!(
                store.current_ready_source_id(&history).unwrap().as_deref(),
                Some(generation_a.as_str())
            );
            assert_eq!(
                stored_source(&store, &history, &generation_b).state,
                GitHistorySourceStateV1::Ready
            );
            assert_eq!(acceptance_counter(&store, &history), Some(4));
        }
    }

    #[test]
    fn refinalize_refuses_immutable_conflicts() {
        type Tamper = fn(&GitSourceStore, &RepoHistoryId, &str);
        let cases: [(&str, Tamper); 4] = [
            ("source descriptor", |store, history, generation| {
                rewrite_source(store, history, generation, |source| {
                    source.descriptor.logical_bytes += 1;
                });
            }),
            ("source namespace", |store, history, generation| {
                rewrite_source(store, history, generation, |source| {
                    source.primary_namespace = CommitNamespace::parse("repo-b").unwrap();
                });
            }),
            ("stored descriptor", |store, history, generation| {
                let path = store.generation_dir(history, generation).unwrap();
                let directory = NofollowDirectory::open_existing(&path).unwrap().unwrap();
                let (mut descriptor, _, _) = fixture_for('1', '2');
                descriptor.commit_count = 3;
                write_json(&directory, "descriptor.json", &descriptor).unwrap();
            }),
            ("stored manifest", |store, history, generation| {
                let path = store.generation_dir(history, generation).unwrap();
                let directory = NofollowDirectory::open_existing(&path).unwrap().unwrap();
                let (_, manifest, _) = fixture_for('1', '3');
                write_json(&directory, "manifest.json", &manifest).unwrap();
            }),
        ];
        for (label, tamper) in cases {
            let temp = tempfile::tempdir().unwrap();
            let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
            let (history, namespace) = history_ids();
            let (_, generation) =
                ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
            let pointer = ready_pointer(&store, &history);
            tamper(&store, &history, &generation);
            let upload =
                upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
            let error = store
                .finalize_history_upload("producer-a", &upload)
                .unwrap_err();
            assert_eq!(
                request_error(&error),
                Some(StoreRequestError::InvalidInput),
                "{label}: {error:#}"
            );
            assert_eq!(ready_pointer(&store, &history), pointer, "{label}");
            let record = upload_record(&store, &upload);
            assert_eq!(record.state, GitHistorySourceStateV1::MissingRecords);
            assert_eq!(record.accepted_sequence, None, "{label}");
        }
    }

    #[test]
    fn older_acceptance_cannot_rewind_a_newer_pointer_but_a_fresh_upload_can() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();

        // A completed upload replayed after a newer acceptance is a no-op.
        let (completed_a, generation_a) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        let (completed_b, generation_b) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        let after_b = ready_pointer(&store, &history);
        assert_eq!(accepted(&after_b), (2, completed_b.clone()));
        store
            .finalize_history_upload("producer-a", &completed_a)
            .unwrap();
        assert_eq!(ready_pointer(&store, &history), after_b);

        // Interrupted A finalize after its pointer write, then B wins, then
        // the A retry finishes without rewinding B or reopening A.
        let interrupted_a =
            upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
        fail_finalize_at("ready-pointer");
        assert!(
            store
                .finalize_history_upload("producer-a", &interrupted_a)
                .is_err()
        );
        let pointer = ready_pointer(&store, &history);
        assert_eq!(pointer.source_generation_id, generation_a);
        assert_eq!(accepted(&pointer), (3, interrupted_a.clone()));
        let (newer_b, _) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        let after_newer_b = ready_pointer(&store, &history);
        assert_eq!(after_newer_b.source_generation_id, generation_b);
        assert_eq!(accepted(&after_newer_b), (4, newer_b));
        rewrite_source(&store, &history, &generation_a, |source| {
            source.state = GitHistorySourceStateV1::Superseded;
            source.diagnostic = Some("superseded by the repository current-ready source".into());
        });
        let retried = store
            .finalize_history_upload("producer-a", &interrupted_a)
            .unwrap();
        assert_eq!(retried.source_generation_id, generation_a);
        assert_eq!(ready_pointer(&store, &history), after_newer_b);
        let source_a = stored_source(&store, &history, &generation_a);
        assert_eq!(source_a.state, GitHistorySourceStateV1::Superseded);
        assert!(source_a.diagnostic.is_some());
        let record = upload_record(&store, &interrupted_a);
        assert_eq!(record.state, GitHistorySourceStateV1::Ready);
        assert_eq!(record.accepted_sequence, Some(3));
        store
            .finalize_history_upload("producer-a", &interrupted_a)
            .unwrap();
        assert_eq!(ready_pointer(&store, &history), after_newer_b);

        // A genuinely new upload of A is a newer acceptance and wins.
        let (fresh_a, generation) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        assert_eq!(generation, generation_a);
        let pointer = ready_pointer(&store, &history);
        assert_eq!(pointer.source_generation_id, generation_a);
        assert_eq!(accepted(&pointer), (5, fresh_a));
        let source_a = stored_source(&store, &history, &generation_a);
        assert_eq!(source_a.state, GitHistorySourceStateV1::Ready);
        assert_eq!(source_a.diagnostic, None);
        assert_unique_acceptances(&store);
    }

    /// Retirement removes the pointer and with it the baseline acceptances
    /// are ordered against. A completed upload replay stays a no-op, a fresh
    /// upload is a new acceptance, and an interrupted upload whose
    /// checkpoint is older than the retired pointer's publishes the pointer
    /// again when it resumes. The counter is untouched, so sequences stay
    /// unique.
    #[test]
    fn a_retired_pointer_is_republished_by_the_next_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();
        let (completed_a, generation_a) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        assert_eq!(
            accepted(&ready_pointer(&store, &history)),
            (1, completed_a.clone())
        );

        let retired = store.retire_current_ready_pointer(&history).unwrap();
        assert_eq!(retired.as_deref(), Some(generation_a.as_str()));
        assert!(store.current_ready_pointers().unwrap().pointers.is_empty());
        assert_eq!(acceptance_counter(&store, &history), Some(2));

        // A replay of the completed upload stays a no-op: no pointer, and
        // the source stays retired.
        store
            .finalize_history_upload("producer-a", &completed_a)
            .unwrap();
        assert!(store.current_ready_pointers().unwrap().pointers.is_empty());
        assert_eq!(
            stored_source(&store, &history, &generation_a).state,
            GitHistorySourceStateV1::Superseded
        );

        // A fresh upload of the same head is a new acceptance: it publishes
        // the pointer again and reopens the source.
        let (fresh_a, generation) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        assert_eq!(generation, generation_a);
        assert_ne!(fresh_a, completed_a);
        assert_eq!(accepted(&ready_pointer(&store, &history)), (2, fresh_a));
        let source = stored_source(&store, &history, &generation_a);
        assert_eq!(source.state, GitHistorySourceStateV1::Ready);
        assert_eq!(source.diagnostic, None);

        // An upload of A is checkpointed and interrupted, then B is accepted
        // with a newer sequence and its pointer is retired. Resuming the
        // interrupted upload meets no pointer, so its older checkpoint
        // publishes the pointer.
        let interrupted_a =
            upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
        fail_finalize_at("acceptance-checkpoint");
        assert!(
            store
                .finalize_history_upload("producer-a", &interrupted_a)
                .is_err()
        );
        assert_eq!(
            upload_record(&store, &interrupted_a).accepted_sequence,
            Some(3)
        );
        let (completed_b, generation_b) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        assert_eq!(accepted(&ready_pointer(&store, &history)), (4, completed_b));
        assert_eq!(
            store
                .retire_current_ready_pointer(&history)
                .unwrap()
                .as_deref(),
            Some(generation_b.as_str())
        );
        rewrite_source(&store, &history, &generation_a, |source| {
            source.state = GitHistorySourceStateV1::Superseded;
            source.diagnostic = Some("superseded by the repository current-ready source".into());
        });
        store
            .finalize_history_upload("producer-a", &interrupted_a)
            .unwrap();
        let republished = ready_pointer(&store, &history);
        assert_eq!(republished.source_generation_id, generation_a);
        assert_eq!(accepted(&republished), (3, interrupted_a));
        assert_eq!(
            stored_source(&store, &history, &generation_a).state,
            GitHistorySourceStateV1::Ready
        );
        assert_eq!(acceptance_counter(&store, &history), Some(5));
        assert_unique_acceptances(&store);
    }

    /// HEAD returns to A while an interrupted upload of A still holds an
    /// acceptance checkpoint older than the pointer. Begin resumes that
    /// upload; its finalize completes without rewinding the pointer or
    /// reopening A, and the upload after it is the one that repoints.
    #[test]
    fn a_stale_checkpoint_resumed_on_head_return_converges_with_the_next_upload() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();
        let (_, generation_a) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));

        // A second upload of A is checkpointed and then interrupted before
        // it reaches the pointer.
        let interrupted =
            upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
        fail_finalize_at("acceptance-checkpoint");
        assert!(
            store
                .finalize_history_upload("producer-a", &interrupted)
                .is_err()
        );
        assert_eq!(
            upload_record(&store, &interrupted).accepted_sequence,
            Some(2)
        );

        // HEAD moves to B, which becomes current; A is superseded.
        let (completed_b, generation_b) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        let after_b = ready_pointer(&store, &history);
        assert_eq!(accepted(&after_b), (3, completed_b));
        rewrite_source(&store, &history, &generation_a, |source| {
            source.state = GitHistorySourceStateV1::Superseded;
            source.diagnostic = Some("superseded by the repository current-ready source".into());
        });

        // HEAD returns to A. Begin hands back the interrupted upload with
        // the state it stopped in.
        let resumed = store
            .begin_history_upload("producer-a", &history, &namespace, fixture_for('1', '2').0)
            .unwrap();
        assert_eq!(resumed.upload_id, interrupted);
        assert_eq!(resumed.state, GitHistorySourceStateV1::MissingRecords);
        let finalized = store
            .finalize_history_upload("producer-a", &interrupted)
            .unwrap();
        assert_eq!(finalized.source_generation_id, generation_a);
        assert_eq!(ready_pointer(&store, &history), after_b);
        assert_eq!(after_b.source_generation_id, generation_b);
        assert_eq!(
            stored_source(&store, &history, &generation_a).state,
            GitHistorySourceStateV1::Superseded
        );
        assert_eq!(
            upload_record(&store, &interrupted).state,
            GitHistorySourceStateV1::Ready
        );

        // The next upload of A is a new attempt and a newer acceptance.
        let (fresh, generation) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        assert_ne!(fresh, interrupted);
        assert_eq!(generation, generation_a);
        let pointer = ready_pointer(&store, &history);
        assert_eq!(pointer.source_generation_id, generation_a);
        assert_eq!(accepted(&pointer), (4, fresh));
        let source = stored_source(&store, &history, &generation_a);
        assert_eq!(source.state, GitHistorySourceStateV1::Ready);
        assert_eq!(source.diagnostic, None);
        assert_unique_acceptances(&store);
    }

    /// The listing and the retirement read the pointer through the same
    /// validating loader as acceptance. A malformed pointer is never removed:
    /// retirement refuses it naming the repository, and the listing reports
    /// it as malformed while still listing every other repository.
    #[test]
    fn a_malformed_pointer_is_named_and_does_not_hide_other_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();
        ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        let other = RepoHistoryId::parse("rh_00000000000000000000000000000002").unwrap();
        let (_, other_generation) =
            ingest_fixture(&store, &other, &namespace, fixture_for('1', '3'));
        let mut pointer = serde_json::to_value(ready_pointer(&store, &history)).unwrap();
        pointer["accepted_sequence"] = serde_json::json!(0);
        let raw = serde_json::to_string(&pointer).unwrap();
        write_ready_pointer(&store, &history, &raw);

        let listing = store.current_ready_pointers().unwrap();
        assert_eq!(listing.pointers.len(), 1);
        assert_eq!(listing.pointers[0].repo_history_id, other);
        assert_eq!(listing.pointers[0].source_generation_id, other_generation);
        assert_eq!(listing.malformed.len(), 1);
        assert_eq!(listing.malformed[0].repo_history_id, history);
        assert!(
            listing.malformed[0]
                .error
                .contains("ready pointer is malformed"),
            "{}",
            listing.malformed[0].error
        );

        let refused = store.retire_current_ready_pointer(&history).unwrap_err();
        let message = format!("{refused:#}");
        assert!(message.contains(history.as_str()), "{message}");
        assert!(message.contains("ready pointer"), "{message}");
        assert_eq!(
            request_error(&refused),
            Some(StoreRequestError::InvalidState)
        );
        let on_disk = fs::read_to_string(
            store
                .repo_history_root(&history)
                .unwrap()
                .join("current-ready.json"),
        )
        .unwrap();
        assert_eq!(on_disk, raw);
        // The readable repository's pointer still retires.
        assert_eq!(
            store
                .retire_current_ready_pointer(&other)
                .unwrap()
                .as_deref(),
            Some(other_generation.as_str())
        );
    }

    #[test]
    fn conflicting_equal_sequence_identities_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = history_store(&temp.path().canonicalize().unwrap().join("git-sources"));
        let (history, namespace) = history_ids();
        let (_, generation_b) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
        let upload = upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
        fail_finalize_at("acceptance-checkpoint");
        assert!(
            store
                .finalize_history_upload("producer-a", &upload)
                .is_err()
        );
        assert_eq!(upload_record(&store, &upload).accepted_sequence, Some(2));
        let generation_a = upload_record(&store, &upload).source_generation_id.unwrap();
        for (label, generation, upload_id) in [
            ("foreign upload", generation_a.clone(), "f".repeat(32)),
            ("foreign generation", generation_b.clone(), upload.clone()),
        ] {
            let head = stored_source(&store, &history, &generation)
                .descriptor
                .repo_head;
            write_ready_pointer(
                &store,
                &history,
                &serde_json::to_string(&serde_json::json!({
                    "version": 1,
                    "source_generation_id": generation,
                    "producer_id": "producer-a",
                    "repo_head": head,
                    "accepted_sequence": 2,
                    "accepted_upload_id": upload_id,
                }))
                .unwrap(),
            );
            let error = store
                .finalize_history_upload("producer-a", &upload)
                .unwrap_err();
            assert_eq!(
                request_error(&error),
                Some(StoreRequestError::InvalidState),
                "{label}: {error:#}"
            );
            assert_eq!(
                upload_record(&store, &upload).state,
                GitHistorySourceStateV1::MissingRecords,
                "{label}"
            );
        }
    }

    #[test]
    fn finalize_recovers_after_a_crash_at_every_durable_step() {
        for retained in [
            None,
            Some(GitHistorySourceStateV1::Superseded),
            Some(GitHistorySourceStateV1::Failed),
        ] {
            for point in [
                "immutable-installed",
                "acceptance-counter",
                "acceptance-checkpoint",
                "generation-index",
                "source-reopened",
                "ready-pointer",
                "upload-ready",
            ] {
                if point == "source-reopened" && retained.is_none() {
                    continue;
                }
                let label = format!("{retained:?} {point}");
                let temp = tempfile::tempdir().unwrap();
                let root = temp.path().canonicalize().unwrap().join("git-sources");
                let store = history_store(&root);
                let (history, namespace) = history_ids();
                let earlier = if let Some(state) = retained {
                    let (_, generation_a) =
                        ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
                    ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
                    rewrite_source(&store, &history, &generation_a, |source| {
                        source.state = state;
                        source.diagnostic = Some("obsolete".into());
                    });
                    2
                } else {
                    // A crash before the index write leaves no index; the
                    // retry must repair it rather than trust the generation.
                    ingest_fixture(&store, &history, &namespace, fixture_for('1', '3'));
                    1
                };
                let upload =
                    upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
                let (descriptor, _, _) = fixture_for('1', '2');
                fail_finalize_at(point);
                let crashed = store.finalize_history_upload("producer-a", &upload);
                clear_finalize_failure();
                assert!(crashed.is_err(), "{label}: failpoint was not reached");
                drop(store);

                let store = history_store(&root);
                // Whatever the crash left behind, a source the probe reports
                // as current is never terminal: a producer that stops at a
                // current probe leaves an activatable source.
                if let Some(current) = store
                    .probe_ready_history(
                        "producer-a",
                        &history,
                        &descriptor.repo_head,
                        descriptor.object_format,
                    )
                    .unwrap()
                {
                    assert!(
                        !matches!(
                            current.state,
                            GitHistorySourceStateV1::Superseded | GitHistorySourceStateV1::Failed
                        ),
                        "{label}: probe reports terminal {:?} as current",
                        current.state
                    );
                    assert_eq!(current.diagnostic, None, "{label}");
                }
                for _ in 0..2 {
                    store
                        .finalize_history_upload("producer-a", &upload)
                        .unwrap_or_else(|error| panic!("{label}: {error:#}"));
                }
                let generation = upload_record(&store, &upload).source_generation_id.unwrap();
                let record = upload_record(&store, &upload);
                assert_eq!(record.state, GitHistorySourceStateV1::Ready, "{label}");
                let sequence = record.accepted_sequence.unwrap();
                assert!(sequence > earlier, "{label}: {sequence}");
                let pointer = ready_pointer(&store, &history);
                assert_eq!(pointer.source_generation_id, generation, "{label}");
                assert_eq!(accepted(&pointer), (sequence, upload.clone()), "{label}");
                assert_eq!(
                    store.current_ready_source_id(&history).unwrap().as_deref(),
                    Some(generation.as_str()),
                    "{label}"
                );
                assert_eq!(
                    store
                        .history_status("producer-a", &generation)
                        .unwrap()
                        .state,
                    GitHistorySourceStateV1::Ready,
                    "{label}: index and reopened source"
                );
                assert_eq!(
                    stored_source(&store, &history, &generation).diagnostic,
                    None,
                    "{label}"
                );
                assert_eq!(
                    acceptance_counter(&store, &history),
                    Some(sequence + 1),
                    "{label}"
                );
                assert_unique_acceptances(&store);
            }
        }
    }

    #[test]
    fn legacy_history_state_upgrades_without_reinterpretation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let store = history_store(&root);
        let (history, namespace) = history_ids();

        // Legacy estate: a completed A whose records and pointer predate
        // ordered acceptance, plus unfinished B and C uploads.
        let (legacy_a, generation_a) =
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
        let mut legacy_record = serde_json::to_value(upload_record(&store, &legacy_a)).unwrap();
        legacy_record
            .as_object_mut()
            .unwrap()
            .remove("accepted_sequence")
            .unwrap();
        rewrite_upload_raw(&store, &legacy_a, &legacy_record.to_string());
        let head_a = stored_source(&store, &history, &generation_a)
            .descriptor
            .repo_head;
        let legacy_pointer = serde_json::json!({
            "version": 1,
            "source_generation_id": generation_a,
            "producer_id": "producer-a",
            "repo_head": head_a,
        })
        .to_string();
        write_ready_pointer(&store, &history, &legacy_pointer);
        fs::remove_file(
            store
                .repo_history_root(&history)
                .unwrap()
                .join("acceptance-sequence.json"),
        )
        .unwrap();
        let upload_b =
            upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '3'));
        let upload_c =
            upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '4'));
        drop(store);

        let store = history_store(&root);
        assert_eq!(upload_record(&store, &legacy_a).accepted_sequence, None);
        assert_eq!(
            ready_pointer(&store, &history).acceptance().unwrap(),
            HistoryPointerAcceptance::Legacy
        );
        assert!(
            store
                .probe_ready_history("producer-a", &history, &head_a, GitObjectFormatV1::Sha1)
                .unwrap()
                .is_some()
        );

        // A completed legacy upload stays a no-op and gains no acceptance.
        store
            .finalize_history_upload("producer-a", &legacy_a)
            .unwrap();
        assert_eq!(upload_record(&store, &legacy_a).accepted_sequence, None);
        assert_eq!(
            ready_pointer(&store, &history).acceptance().unwrap(),
            HistoryPointerAcceptance::Legacy
        );
        assert_eq!(acceptance_counter(&store, &history), None);

        // The first verified post-upgrade finalize is an acceptance above
        // the legacy baseline.
        store
            .finalize_history_upload("producer-a", &upload_b)
            .unwrap();
        assert_eq!(accepted(&ready_pointer(&store, &history)), (1, upload_b));

        // Restart partway through the next acceptance: the counter moved
        // but the upload checkpoint did not. The retry allocates above it.
        fail_finalize_at("acceptance-counter");
        assert!(
            store
                .finalize_history_upload("producer-a", &upload_c)
                .is_err()
        );
        clear_finalize_failure();
        drop(store);
        let store = history_store(&root);
        store
            .finalize_history_upload("producer-a", &upload_c)
            .unwrap();
        assert_eq!(
            accepted(&ready_pointer(&store, &history)),
            (3, upload_c.clone())
        );
        assert_eq!(acceptance_counter(&store, &history), Some(4));

        // A missing or lagging counter is recovered from the pointer and the
        // retained upload checkpoints, never reset below them.
        let history_root = store.repo_history_root(&history).unwrap();
        fs::remove_file(history_root.join("acceptance-sequence.json")).unwrap();
        let (upload_d, _) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '5'));
        assert_eq!(accepted(&ready_pointer(&store, &history)), (4, upload_d));
        NofollowDirectory::open_existing(&history_root)
            .unwrap()
            .unwrap()
            .atomic_replace(
                "acceptance-sequence.json",
                br#"{"version":1,"next_sequence":1}"#,
            )
            .unwrap();
        let (upload_e, _) = ingest_fixture(&store, &history, &namespace, fixture_for('1', '6'));
        assert_eq!(accepted(&ready_pointer(&store, &history)), (5, upload_e));
        assert_eq!(acceptance_counter(&store, &history), Some(6));
        assert_unique_acceptances(&store);
    }

    #[test]
    fn malformed_acceptance_state_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("git-sources");
        let (history, namespace) = history_ids();

        // Malformed pointer acceptance fields.
        let generation_head = |store: &GitSourceStore, generation: &str| {
            stored_source(store, &history, generation)
                .descriptor
                .repo_head
        };
        for (label, fields) in [
            (
                "zero sequence",
                r#""accepted_sequence":0,"accepted_upload_id":"ffffffffffffffffffffffffffffffff""#,
            ),
            ("sequence without upload", r#""accepted_sequence":4"#),
            (
                "upload without sequence",
                r#""accepted_upload_id":"ffffffffffffffffffffffffffffffff""#,
            ),
            (
                "invalid upload id",
                r#""accepted_sequence":4,"accepted_upload_id":"../x""#,
            ),
        ] {
            let _ = fs::remove_dir_all(&root);
            let store = history_store(&root);
            let (_, generation) =
                ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
            let head = generation_head(&store, &generation);
            write_ready_pointer(
                &store,
                &history,
                &format!(
                    r#"{{"version":1,"source_generation_id":"{generation}","producer_id":"producer-a","repo_head":"{head}",{fields}}}"#
                ),
            );
            assert!(
                store
                    .probe_ready_history("producer-a", &history, &head, GitObjectFormatV1::Sha1)
                    .is_err(),
                "{label}"
            );
            assert!(store.current_ready_source_id(&history).is_err(), "{label}");
            let upload =
                upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '3'));
            let error = store
                .finalize_history_upload("producer-a", &upload)
                .unwrap_err();
            assert_eq!(
                request_error(&error),
                Some(StoreRequestError::InvalidState),
                "{label}: {error:#}"
            );
            assert_eq!(
                upload_record(&store, &upload).state,
                GitHistorySourceStateV1::MissingRecords
            );
        }

        // Malformed durable counters.
        for (label, raw) in [
            ("zero counter", r#"{"version":1,"next_sequence":0}"#),
            ("future counter", r#"{"version":9,"next_sequence":3}"#),
            ("string counter", r#"{"version":1,"next_sequence":"3"}"#),
        ] {
            let _ = fs::remove_dir_all(&root);
            let store = history_store(&root);
            ingest_fixture(&store, &history, &namespace, fixture_for('1', '2'));
            NofollowDirectory::open_existing(&store.repo_history_root(&history).unwrap())
                .unwrap()
                .unwrap()
                .atomic_replace("acceptance-sequence.json", raw.as_bytes())
                .unwrap();
            let upload =
                upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '3'));
            assert!(
                store
                    .finalize_history_upload("producer-a", &upload)
                    .is_err(),
                "{label}"
            );
            assert_eq!(upload_record(&store, &upload).accepted_sequence, None);
        }

        // Malformed upload checkpoints.
        for (label, state, sequence) in [
            ("zero checkpoint", "missing_records", 0),
            ("checkpoint before verification", "receiving_manifest", 2),
        ] {
            let _ = fs::remove_dir_all(&root);
            let store = history_store(&root);
            let upload =
                upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '2'));
            let mut record = serde_json::to_value(upload_record(&store, &upload)).unwrap();
            record["state"] = serde_json::json!(state);
            record["accepted_sequence"] = serde_json::json!(sequence);
            rewrite_upload_raw(&store, &upload, &record.to_string());
            let error = store
                .finalize_history_upload("producer-a", &upload)
                .unwrap_err();
            assert_eq!(
                request_error(&error),
                Some(StoreRequestError::InvalidState),
                "{label}: {error:#}"
            );
            // The malformed checkpoint also blocks allocation for any other
            // upload rather than being skipped as unknown.
            let other =
                upload_to_missing_records(&store, &history, &namespace, fixture_for('1', '3'));
            assert!(
                store.finalize_history_upload("producer-a", &other).is_err(),
                "{label}"
            );
        }
    }
}
