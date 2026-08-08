//! Durable intake store for complete typed Git-history snapshots.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
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
        ] {
            NofollowDirectory::open_or_create(&root.join(relative))?;
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
                    || source.state != GitHistorySourceStateV1::Ready
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
        let retired_generations = self.retire_old_generations(
            protected_generation_ids,
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

fn begin_response(upload_id: String) -> BeginGitHistoryUploadResponseV1 {
    BeginGitHistoryUploadResponseV1 {
        upload_id,
        max_page_entries: MAX_HISTORY_MANIFEST_PAGE_ENTRIES,
        max_page_bytes: MAX_HISTORY_MANIFEST_PAGE_BYTES,
        max_record_bytes: MAX_HISTORY_RECORD_BYTES,
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

    fn fixture() -> (
        GitHistoryDescriptorV1,
        Vec<GitHistoryManifestEntryV1>,
        Vec<Vec<u8>>,
    ) {
        fixture_for('1', '2')
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
