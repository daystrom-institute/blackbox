//! Durable content-addressed upload and generation store for code sources.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bbox_code_source::{
    BeginUploadResponse, DEFAULT_MAX_MANIFEST_FILES, DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
    GenerationDescriptor, GenerationState, GenerationStatus, MAX_MANIFEST_PAGE_ENTRIES,
    ManifestEntry, MissingBlobsPage, generation_id, scope_hash, validate_manifest,
    validate_producer_id, validate_sha256,
};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MISSING_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLimits {
    pub max_manifest_files: u64,
    pub max_manifest_logical_bytes: u64,
    pub max_open_uploads_per_producer: usize,
    pub retained_generations: usize,
    pub unreferenced_blob_grace_hours: u64,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_manifest_files: DEFAULT_MAX_MANIFEST_FILES,
            max_manifest_logical_bytes: DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
            max_open_uploads_per_producer: 2,
            retained_generations: 2,
            unreferenced_blob_grace_hours: 168,
        }
    }
}

struct SharedStoreState {
    limits: RwLock<StoreLimits>,
    mutation: Mutex<()>,
    verified_blobs: Mutex<HashMap<String, BlobIdentity>>,
    #[cfg(test)]
    blob_verifications: AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BlobIdentity {
    len: u64,
    modified_secs: i64,
    modified_nanos: i64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

pub struct CodeSourceStore {
    root: PathBuf,
    shared: Arc<SharedStoreState>,
}

static STORE_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<SharedStoreState>>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadRecord {
    version: u32,
    upload_id: String,
    producer_id: String,
    ordinal: u64,
    descriptor: GenerationDescriptor,
    state: GenerationState,
    next_page: u32,
    page_digests: BTreeMap<u32, String>,
    #[serde(default)]
    received_file_count: u64,
    #[serde(default)]
    received_logical_bytes: u64,
    #[serde(default)]
    last_relative_path: Option<String>,
    generation_id: Option<String>,
    updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredGeneration {
    pub version: u32,
    pub generation_id: String,
    pub producer_id: String,
    pub ordinal: u64,
    pub descriptor: GenerationDescriptor,
    pub state: GenerationState,
    pub diagnostic: Option<String>,
    pub created_unix_secs: u64,
    #[serde(default)]
    pub materialized_doc_count: Option<u64>,
    #[serde(default)]
    pub entity_inventory_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub version: u32,
    pub project_id: String,
    pub generation_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub current_chunk_targets: BTreeMap<String, EntityRef>,
    #[serde(default)]
    pub activated_unix_secs: u64,
    #[serde(default)]
    pub cutback_pending: bool,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaintenanceStats {
    pub expired_uploads: u64,
    pub scrubbed_blobs: u64,
    pub degraded_generations: u64,
    pub reclaimed_blobs: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeSourceHealthRecord {
    pub version: u32,
    pub project_id: String,
    pub code: String,
    pub diagnostic: String,
    pub updated_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetirementRecord {
    pub version: u32,
    pub project_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub generation_id: Option<String>,
}

impl CodeSourceStore {
    pub fn open(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        let mut root = root.into();
        create_private_dir(&root)?;
        for relative in [
            "blobs/sha256",
            "uploads",
            "scopes",
            "ordinals",
            "desired",
            "activations",
            "health",
            "retirements",
        ] {
            create_private_dir(&root.join(relative))?;
        }
        root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing code-source store {}", root.display()))?;
        let mut registry = STORE_REGISTRY
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow!("code-source store registry lock poisoned"))?;
        registry.retain(|_, state| state.strong_count() > 0);
        let shared = registry
            .get(&root)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let shared = Arc::new(SharedStoreState {
                    limits: RwLock::new(limits),
                    mutation: Mutex::new(()),
                    verified_blobs: Mutex::new(HashMap::new()),
                    #[cfg(test)]
                    blob_verifications: AtomicU64::new(0),
                });
                registry.insert(root.clone(), Arc::downgrade(&shared));
                shared
            });
        Ok(Self { root, shared })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn update_limits(&self, limits: StoreLimits) -> Result<()> {
        *self
            .shared
            .limits
            .write()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))? = limits;
        Ok(())
    }

    pub fn begin_upload(
        &self,
        producer_id: &str,
        descriptor: GenerationDescriptor,
    ) -> Result<BeginUploadResponse> {
        validate_producer_id(producer_id)?;
        descriptor.validate_header()?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        if descriptor.file_count > limits.max_manifest_files {
            bail!("manifest file count exceeds configured limit");
        }
        if descriptor.logical_bytes > limits.max_manifest_logical_bytes {
            bail!("manifest logical bytes exceed configured limit");
        }
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let producer_dir = self.upload_producer_dir(producer_id);
        create_private_dir(&producer_dir)?;
        let open = fs::read_dir(&producer_dir)?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("upload.json").is_file())
            .filter(|entry| {
                read_json::<UploadRecord>(&entry.path().join("upload.json"))
                    .map(|record| {
                        matches!(
                            record.state,
                            GenerationState::ReceivingManifest | GenerationState::MissingBlobs
                        )
                    })
                    .unwrap_or(false)
            })
            .count();
        if open >= limits.max_open_uploads_per_producer {
            bail!("producer already has the maximum number of open uploads");
        }
        let ordinal = self.next_ordinal(&descriptor.scope)?;
        let upload_id = Uuid::new_v4().to_string();
        let upload_dir = producer_dir.join(&upload_id);
        create_private_dir(&upload_dir.join("pages"))?;
        let record = UploadRecord {
            version: STORE_VERSION,
            upload_id: upload_id.clone(),
            producer_id: producer_id.to_string(),
            ordinal,
            descriptor,
            state: GenerationState::ReceivingManifest,
            next_page: 0,
            page_digests: BTreeMap::new(),
            received_file_count: 0,
            received_logical_bytes: 0,
            last_relative_path: None,
            generation_id: None,
            updated_unix_secs: now_unix_secs(),
        };
        atomic_write_json(&upload_dir.join("upload.json"), &record)?;
        Ok(BeginUploadResponse {
            upload_id,
            ordinal,
            max_page_entries: bbox_code_source::MAX_MANIFEST_PAGE_ENTRIES,
            max_page_bytes: bbox_code_source::MAX_MANIFEST_PAGE_BYTES,
        })
    }

    pub fn put_manifest_page(
        &self,
        producer_id: &str,
        upload_id: &str,
        page: u32,
        entries: &[ManifestEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            bail!("manifest pages must not be empty");
        }
        if entries.len() > MAX_MANIFEST_PAGE_ENTRIES {
            bail!("manifest page exceeds entry cap");
        }
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        validate_manifest(
            entries,
            limits.max_manifest_files,
            limits.max_manifest_logical_bytes,
        )?;
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        if record.state != GenerationState::ReceivingManifest {
            bail!("upload is not receiving manifest pages");
        }
        let raw = serde_json::to_vec(entries)?;
        if raw.len() > bbox_code_source::MAX_MANIFEST_PAGE_BYTES {
            bail!("manifest page exceeds byte cap");
        }
        let digest = sha256_hex(&raw);
        if page < record.next_page {
            if record.page_digests.get(&page) == Some(&digest) {
                return Ok(());
            }
            bail!("manifest page replay conflicts with stored page");
        }
        if page != record.next_page {
            bail!("manifest pages must be contiguous");
        }
        let page_file_count = entries.len() as u64;
        let page_logical_bytes = entries.iter().try_fold(0_u64, |sum, entry| {
            sum.checked_add(entry.size)
                .ok_or_else(|| anyhow!("manifest logical byte count overflow"))
        })?;
        let received_file_count = record
            .received_file_count
            .checked_add(page_file_count)
            .ok_or_else(|| anyhow!("manifest file count overflow"))?;
        let received_logical_bytes = record
            .received_logical_bytes
            .checked_add(page_logical_bytes)
            .ok_or_else(|| anyhow!("manifest logical byte count overflow"))?;
        if received_file_count > record.descriptor.file_count {
            bail!("manifest pages exceed the declared file count");
        }
        if received_logical_bytes > record.descriptor.logical_bytes {
            bail!("manifest pages exceed the declared logical bytes");
        }
        if record
            .last_relative_path
            .as_deref()
            .is_some_and(|previous| {
                entries
                    .first()
                    .is_some_and(|entry| entry.relative_path.as_str() <= previous)
            })
        {
            bail!("manifest entries are not strictly sorted across pages");
        }
        let path = self
            .upload_dir(producer_id, upload_id)
            .join("pages")
            .join(format!("{page:08}.json"));
        atomic_write(&path, &raw)?;
        record.page_digests.insert(page, digest);
        record.next_page += 1;
        record.received_file_count = received_file_count;
        record.received_logical_bytes = received_logical_bytes;
        record.last_relative_path = entries.last().map(|entry| entry.relative_path.clone());
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)
    }

    pub fn complete_manifest(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<MissingBlobsPage> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        if !matches!(
            record.state,
            GenerationState::ReceivingManifest | GenerationState::MissingBlobs
        ) {
            bail!("upload manifest cannot be completed in its current state");
        }
        let entries = self.load_upload_entries(&record)?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        record.descriptor.validate_manifest(
            &entries,
            limits.max_manifest_files,
            limits.max_manifest_logical_bytes,
        )?;
        let generation = generation_id(producer_id, &record.descriptor);
        let generation_dir = self.generation_dir(&record.descriptor.scope, &generation);
        create_private_dir(&generation_dir)?;
        let manifest_path = generation_dir.join("manifest.jsonl");
        if manifest_path.is_file() {
            if read_manifest_jsonl(&manifest_path)? != entries {
                bail!("generation manifest conflicts with immutable stored manifest");
            }
        } else {
            write_manifest_jsonl(&manifest_path, &entries)?;
        }
        let metadata_path = generation_dir.join("metadata.json");
        if metadata_path.is_file() {
            let stored: StoredGeneration = read_json(&metadata_path)?;
            if stored.producer_id != producer_id || stored.descriptor != record.descriptor {
                bail!("generation identity conflicts with stored metadata");
            }
        } else {
            let stored = StoredGeneration {
                version: STORE_VERSION,
                generation_id: generation.clone(),
                producer_id: producer_id.to_string(),
                ordinal: record.ordinal,
                descriptor: record.descriptor.clone(),
                state: GenerationState::MissingBlobs,
                diagnostic: None,
                created_unix_secs: now_unix_secs(),
                materialized_doc_count: None,
                entity_inventory_sha256: None,
            };
            atomic_write_json(&metadata_path, &stored)?;
        }
        let missing_path = self.upload_dir(producer_id, upload_id).join("missing.json");
        let missing = if missing_path.is_file() {
            read_json::<Vec<String>>(&missing_path)?
        } else {
            let missing = self.missing_hashes(&entries)?;
            atomic_write_json(&missing_path, &missing)?;
            missing
        };
        record.state = GenerationState::MissingBlobs;
        record.generation_id = Some(generation.clone());
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)?;
        self.missing_page(&generation, &missing, 0)
    }

    pub fn missing_blobs(
        &self,
        producer_id: &str,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingBlobsPage> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        if record.state != GenerationState::MissingBlobs {
            bail!("missing-blob cursor is stale for upload state");
        }
        let generation = record
            .generation_id
            .as_deref()
            .ok_or_else(|| anyhow!("manifest is not complete"))?;
        let offset = decode_cursor(generation, cursor)?;
        let missing = read_json::<Vec<String>>(
            &self.upload_dir(producer_id, upload_id).join("missing.json"),
        )?;
        let page = self.missing_page(generation, &missing, offset)?;
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)?;
        Ok(page)
    }

    pub fn install_blob<R: Read>(
        &self,
        producer_id: &str,
        upload_id: &str,
        expected_hash: &str,
        expected_size: u64,
        mut reader: R,
    ) -> Result<u64> {
        validate_sha256(expected_hash)?;
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        let generation = record
            .generation_id
            .as_deref()
            .ok_or_else(|| anyhow!("manifest is not complete"))?;
        let entries = self.load_generation_entries(&record.descriptor.scope, generation)?;
        if !entries
            .iter()
            .any(|entry| entry.content_sha256 == expected_hash && entry.size == expected_size)
        {
            bail!("blob is not referenced by this upload with the declared size");
        }
        let destination = self.blob_path(expected_hash);
        if destination.is_file() {
            if self
                .verified_blob_file(expected_hash, expected_size)
                .is_ok()
            {
                record.updated_unix_secs = now_unix_secs();
                self.save_upload(&record)?;
                return Ok(expected_size);
            }
            self.forget_verified_blob(expected_hash)?;
            quarantine_corrupt_blob(&destination)?;
        }
        create_private_dir(destination.parent().expect("blob path parent"))?;
        let temporary = destination.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            written = written
                .checked_add(read as u64)
                .ok_or_else(|| anyhow!("blob size overflow"))?;
            if written > expected_size {
                drop(file);
                let _ = fs::remove_file(&temporary);
                bail!("blob body exceeds declared size");
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
        if written != expected_size || hex::encode(hasher.finalize()) != expected_hash {
            drop(file);
            let _ = fs::remove_file(&temporary);
            bail!("blob body hash or size mismatch");
        }
        file.sync_all()?;
        drop(file);
        match fs::hard_link(&temporary, &destination) {
            Ok(()) => {
                fs::remove_file(&temporary)?;
                sync_parent(&destination)?;
                let file = open_blob_nofollow(&destination)?;
                let identity = blob_identity(&file, expected_size)?;
                self.remember_verified_blob(expected_hash, identity)?;
            }
            Err(error) if destination.is_file() => {
                let _ = fs::remove_file(&temporary);
                self.verified_blob_file(expected_hash, expected_size)?;
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    tracing_rename_race(&error);
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)?;
        Ok(written)
    }

    pub fn finalize_upload(&self, producer_id: &str, upload_id: &str) -> Result<StoredGeneration> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        let generation = record
            .generation_id
            .clone()
            .ok_or_else(|| anyhow!("manifest is not complete"))?;
        let entries = self.load_generation_entries(&record.descriptor.scope, &generation)?;
        let missing = self.missing_hashes(&entries)?;
        if !missing.is_empty() {
            bail!("generation still has {} missing blobs", missing.len());
        }
        let mut stored = self.load_generation(&record.descriptor.scope, &generation)?;
        let desired_path = self
            .root
            .join("desired")
            .join(format!("{}.json", scope_hash(&record.descriptor.scope)));
        let previous_desired = if desired_path.is_file() {
            Some(read_json::<StoredGeneration>(&desired_path)?)
        } else {
            None
        };
        let superseded = previous_desired
            .as_ref()
            .is_some_and(|desired| desired.ordinal > stored.ordinal);
        let already_activated = self.generation_is_activated(&generation)?;
        stored.state = if already_activated {
            GenerationState::Active
        } else if superseded {
            GenerationState::Superseded
        } else {
            GenerationState::Ready
        };
        stored.diagnostic = None;
        self.save_generation(&stored)?;
        record.state = stored.state;
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)?;
        if !superseded {
            if let Some(mut previous) = previous_desired
                && previous.generation_id != stored.generation_id
                && !self.generation_is_activated(&previous.generation_id)?
            {
                previous.state = GenerationState::Superseded;
                previous.diagnostic = None;
                self.save_generation(&previous)?;
            }
            atomic_write_json(&desired_path, &stored)?;
        }
        Ok(stored)
    }

    pub fn upload_scope(&self, producer_id: &str, upload_id: &str) -> Result<PublishedScope> {
        Ok(self.load_upload(producer_id, upload_id)?.descriptor.scope)
    }

    pub fn expected_blob_size(
        &self,
        producer_id: &str,
        upload_id: &str,
        hash: &str,
    ) -> Result<u64> {
        validate_sha256(hash)?;
        let record = self.load_upload(producer_id, upload_id)?;
        if record.state != GenerationState::MissingBlobs {
            bail!("upload is not accepting blob data");
        }
        let generation = record
            .generation_id
            .as_deref()
            .ok_or_else(|| anyhow!("manifest is not complete"))?;
        let entries = self.load_generation_entries(&record.descriptor.scope, generation)?;
        let mut sizes = entries
            .iter()
            .filter(|entry| entry.content_sha256 == hash)
            .map(|entry| entry.size);
        let size = sizes
            .next()
            .ok_or_else(|| anyhow!("blob is not referenced by this upload"))?;
        if sizes.any(|other| other != size) {
            bail!("manifest assigns conflicting sizes to one blob hash");
        }
        Ok(size)
    }

    pub fn generation_status(
        &self,
        producer_id: &str,
        generation: &str,
    ) -> Result<GenerationStatus> {
        validate_sha256(generation)?;
        for scope_entry in fs::read_dir(self.root.join("scopes"))? {
            let scope_entry = scope_entry?;
            let metadata = scope_entry
                .path()
                .join("generations")
                .join(generation)
                .join("metadata.json");
            if !metadata.is_file() {
                continue;
            }
            let stored: StoredGeneration = read_json(&metadata)?;
            if stored.producer_id != producer_id {
                bail!("generation belongs to another producer");
            }
            return Ok(GenerationStatus {
                generation_id: stored.generation_id,
                state: stored.state,
                file_count: stored.descriptor.file_count,
                logical_bytes: stored.descriptor.logical_bytes,
                diagnostic: stored.diagnostic,
            });
        }
        bail!("generation not found")
    }

    pub fn find_generation(&self, generation: &str) -> Result<StoredGeneration> {
        validate_sha256(generation)?;
        for scope_entry in fs::read_dir(self.root.join("scopes"))? {
            let metadata = scope_entry?
                .path()
                .join("generations")
                .join(generation)
                .join("metadata.json");
            if metadata.is_file() {
                return read_json(&metadata);
            }
        }
        bail!("generation not found")
    }

    pub fn load_generation(
        &self,
        scope: &PublishedScope,
        generation: &str,
    ) -> Result<StoredGeneration> {
        read_json(&self.generation_dir(scope, generation).join("metadata.json"))
    }

    pub fn save_generation(&self, generation: &StoredGeneration) -> Result<()> {
        atomic_write_json(
            &self
                .generation_dir(&generation.descriptor.scope, &generation.generation_id)
                .join("metadata.json"),
            generation,
        )
    }

    pub fn mark_generation_state(
        &self,
        scope: &PublishedScope,
        generation: &str,
        state: GenerationState,
        diagnostic: Option<String>,
    ) -> Result<StoredGeneration> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut stored = self.load_generation(scope, generation)?;
        stored.state = state;
        stored.diagnostic = diagnostic.map(|value| value.chars().take(512).collect());
        self.save_generation(&stored)?;
        let desired_path = self
            .root
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if desired_path.is_file() {
            let desired: StoredGeneration = read_json(&desired_path)?;
            if desired.generation_id == generation {
                atomic_write_json(&desired_path, &stored)?;
            }
        }
        Ok(stored)
    }

    pub fn record_materialization(
        &self,
        scope: &PublishedScope,
        generation: &str,
        document_count: u64,
        entity_inventory_sha256: String,
    ) -> Result<StoredGeneration> {
        validate_sha256(&entity_inventory_sha256)?;
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let mut stored = self.load_generation(scope, generation)?;
        stored.materialized_doc_count = Some(document_count);
        stored.entity_inventory_sha256 = Some(entity_inventory_sha256);
        self.save_generation(&stored)?;
        Ok(stored)
    }

    pub fn save_activation(&self, activation: &ActivationRecord) -> Result<()> {
        validate_sha256(&activation.generation_id)?;
        validate_sha256(&activation.entity_inventory_sha256)?;
        if activation.version != STORE_VERSION || activation.project_id.trim().is_empty() {
            bail!("invalid activation record");
        }
        atomic_write_json(
            &self
                .root
                .join("activations")
                .join(format!("{}.json", activation.project_id)),
            activation,
        )
    }

    pub fn load_activation(&self, project_id: &str) -> Result<Option<ActivationRecord>> {
        let path = self
            .root
            .join("activations")
            .join(format!("{project_id}.json"));
        if !path.is_file() {
            return Ok(None);
        }
        let record: ActivationRecord = read_json(&path)?;
        if record.version != STORE_VERSION || record.project_id != project_id {
            bail!("activation record identity mismatch");
        }
        Ok(Some(record))
    }

    pub fn activation_records(&self) -> Result<Vec<ActivationRecord>> {
        let mut records: Vec<ActivationRecord> = Vec::new();
        for entry in fs::read_dir(self.root.join("activations"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                records.push(read_json(&entry.path())?);
            }
        }
        records.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(records)
    }

    pub fn mark_cutback_pending(&self, project_id: &str, diagnostic: &str) -> Result<()> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let Some(mut record) = self.load_activation(project_id)? else {
            return Ok(());
        };
        record.cutback_pending = true;
        record.diagnostic = Some(diagnostic.chars().take(512).collect());
        self.save_activation(&record)
    }

    pub fn clear_activation(&self, project_id: &str) -> Result<()> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let path = self
            .root
            .join("activations")
            .join(format!("{project_id}.json"));
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn record_health_failure(
        &self,
        project_id: &str,
        code: &str,
        diagnostic: &str,
    ) -> Result<()> {
        let record = CodeSourceHealthRecord {
            version: STORE_VERSION,
            project_id: project_id.to_string(),
            code: code.to_string(),
            diagnostic: diagnostic.chars().take(512).collect(),
            updated_unix_secs: now_unix_secs(),
        };
        atomic_write_json(&self.health_path(project_id, code), &record)
    }

    pub fn clear_health_failure(&self, project_id: &str, code: &str) -> Result<()> {
        let path = self.health_path(project_id, code);
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn health_records(&self) -> Result<Vec<CodeSourceHealthRecord>> {
        let mut records: Vec<CodeSourceHealthRecord> = Vec::new();
        for entry in fs::read_dir(self.root.join("health"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                records.push(read_json(&entry.path())?);
            }
        }
        records.sort_by(|left, right| {
            left.project_id
                .cmp(&right.project_id)
                .then_with(|| left.code.cmp(&right.code))
        });
        Ok(records)
    }

    pub fn enqueue_retirement(&self, record: &RetirementRecord) -> Result<()> {
        if record.version != STORE_VERSION
            || record.project_id.trim().is_empty()
            || record.selector.trim().is_empty()
            || record.snapshot_id.trim().is_empty()
        {
            bail!("invalid code-source retirement record");
        }
        atomic_write_json(&self.retirement_path(&record.selector), record)
    }

    pub fn retirement_records(&self) -> Result<Vec<RetirementRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root.join("retirements"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                records.push(read_json(&entry.path())?);
            }
        }
        Ok(records)
    }

    pub fn complete_retirement(&self, selector: &str) -> Result<()> {
        let path = self.retirement_path(selector);
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn verified_blob_file(&self, hash: &str, size: u64) -> Result<File> {
        validate_sha256(hash)?;
        let path = self.blob_path(hash);
        let mut file =
            open_blob_nofollow(&path).with_context(|| format!("opening verified blob {hash}"))?;
        let identity = blob_identity(&file, size)?;
        let cache_hit = self
            .shared
            .verified_blobs
            .lock()
            .map_err(|_| anyhow!("verified blob cache lock poisoned"))?
            .get(hash)
            == Some(&identity);
        if !cache_hit {
            #[cfg(test)]
            self.shared
                .blob_verifications
                .fetch_add(1, Ordering::Relaxed);
            verify_open_blob(&mut file, hash)?;
            self.remember_verified_blob(hash, identity)?;
        }
        Ok(file)
    }

    fn remember_verified_blob(&self, hash: &str, identity: BlobIdentity) -> Result<()> {
        let mut cache = self
            .shared
            .verified_blobs
            .lock()
            .map_err(|_| anyhow!("verified blob cache lock poisoned"))?;
        if cache.len() >= 1_000_000 {
            cache.clear();
        }
        cache.insert(hash.to_string(), identity);
        Ok(())
    }

    fn forget_verified_blob(&self, hash: &str) -> Result<()> {
        self.shared
            .verified_blobs
            .lock()
            .map_err(|_| anyhow!("verified blob cache lock poisoned"))?
            .remove(hash);
        Ok(())
    }

    #[cfg(test)]
    fn blob_verification_count(&self) -> u64 {
        self.shared.blob_verifications.load(Ordering::Relaxed)
    }

    pub fn load_generation_entries(
        &self,
        scope: &PublishedScope,
        generation: &str,
    ) -> Result<Vec<ManifestEntry>> {
        read_manifest_jsonl(
            &self
                .generation_dir(scope, generation)
                .join("manifest.jsonl"),
        )
    }

    pub fn desired_generation(&self, scope: &PublishedScope) -> Result<Option<StoredGeneration>> {
        let path = self
            .root
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }

    pub fn expire_uploads(&self, max_idle_secs: u64) -> Result<u64> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let cutoff = now_unix_secs().saturating_sub(max_idle_secs);
        let mut expired = 0_u64;
        for producer in fs::read_dir(self.root.join("uploads"))? {
            let producer = producer?;
            if !producer.file_type()?.is_dir() {
                continue;
            }
            for upload in fs::read_dir(producer.path())? {
                let upload = upload?;
                if !upload.file_type()?.is_dir() {
                    continue;
                }
                let metadata = upload.path().join("upload.json");
                let should_expire = read_json::<UploadRecord>(&metadata)
                    .map(|record| record.updated_unix_secs <= cutoff)
                    .unwrap_or(true);
                if should_expire {
                    let upload_path = upload.path();
                    fs::remove_dir_all(&upload_path)?;
                    sync_parent(&upload_path)?;
                    expired += 1;
                }
            }
        }
        Ok(expired)
    }

    pub fn scrub_retained(&self) -> Result<MaintenanceStats> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        let generations = self.list_generations()?;
        let protected = self.protected_generation_ids(&generations, limits.retained_generations)?;
        let mut stats = MaintenanceStats::default();
        for mut generation in generations {
            if !protected.contains(&generation.generation_id) {
                continue;
            }
            let entries = self
                .load_generation_entries(&generation.descriptor.scope, &generation.generation_id)?;
            let mut degraded = false;
            for (hash, size) in unique_blob_sizes(&entries)? {
                stats.scrubbed_blobs += 1;
                let path = self.blob_path(&hash);
                if verify_blob(&path, &hash, size).is_err() {
                    if path.exists() {
                        self.forget_verified_blob(&hash)?;
                        quarantine_corrupt_blob(&path)?;
                    }
                    degraded = true;
                }
            }
            if degraded {
                generation.state = GenerationState::MissingBlobData;
                generation.diagnostic =
                    Some("one or more retained source blobs failed verification".to_string());
                self.save_generation(&generation)?;
                self.update_desired_if_same(&generation)?;
                self.record_health_failure(
                    &self
                        .activation_project_for_generation(&generation.generation_id)?
                        .unwrap_or_else(|| scope_hash(&generation.descriptor.scope)),
                    "missing_blob_data",
                    "one or more retained source blobs failed verification",
                )?;
                stats.degraded_generations += 1;
            }
        }
        Ok(stats)
    }

    pub fn gc_blobs(&self) -> Result<MaintenanceStats> {
        let _guard = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        let generations = self.list_generations()?;
        let protected = self.protected_generation_ids(&generations, limits.retained_generations)?;
        let mut marked = BTreeSet::new();
        for generation in &generations {
            if protected.contains(&generation.generation_id) {
                marked.extend(
                    self.load_generation_entries(
                        &generation.descriptor.scope,
                        &generation.generation_id,
                    )?
                    .into_iter()
                    .map(|entry| entry.content_sha256),
                );
            }
        }
        for producer in fs::read_dir(self.root.join("uploads"))? {
            let producer = producer?;
            if !producer.file_type()?.is_dir() {
                continue;
            }
            for upload in fs::read_dir(producer.path())? {
                let upload = upload?;
                if !upload.file_type()?.is_dir() {
                    continue;
                }
                let metadata = upload.path().join("upload.json");
                let Ok(record) = read_json::<UploadRecord>(&metadata) else {
                    continue;
                };
                if matches!(
                    record.state,
                    GenerationState::ReceivingManifest | GenerationState::MissingBlobs
                ) {
                    marked.extend(
                        self.load_upload_entries(&record)?
                            .into_iter()
                            .map(|entry| entry.content_sha256),
                    );
                }
            }
        }
        let cutoff = SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                limits.unreferenced_blob_grace_hours.saturating_mul(3_600),
            ))
            .unwrap_or(UNIX_EPOCH);
        let mut stats = MaintenanceStats::default();
        for prefix in fs::read_dir(self.root.join("blobs/sha256"))? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            for blob in fs::read_dir(prefix.path())? {
                let blob = blob?;
                let file_type = blob.file_type()?;
                if !file_type.is_file() || file_type.is_symlink() {
                    continue;
                }
                let Some(hash) = blob.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let is_blob = validate_sha256(&hash).is_ok();
                let is_abandoned = hash.contains(".corrupt-") || hash.ends_with(".tmp");
                if (!is_blob && !is_abandoned) || (is_blob && marked.contains(&hash)) {
                    continue;
                }
                let metadata = blob.metadata()?;
                if metadata.modified().unwrap_or(SystemTime::now()) > cutoff {
                    continue;
                }
                let bytes = metadata.len();
                let path = blob.path();
                fs::remove_file(&path)?;
                if is_blob {
                    self.forget_verified_blob(&hash)?;
                }
                sync_parent(&path)?;
                stats.reclaimed_blobs += 1;
                stats.reclaimed_bytes = stats.reclaimed_bytes.saturating_add(bytes);
            }
        }
        Ok(stats)
    }

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs/sha256").join(&hash[..2]).join(hash)
    }

    fn list_generations(&self) -> Result<Vec<StoredGeneration>> {
        let mut generations = Vec::new();
        for scope in fs::read_dir(self.root.join("scopes"))? {
            let scope = scope?;
            if !scope.file_type()?.is_dir() {
                continue;
            }
            let directory = scope.path().join("generations");
            if !directory.is_dir() {
                continue;
            }
            for generation in fs::read_dir(directory)? {
                let generation = generation?;
                if !generation.file_type()?.is_dir() {
                    continue;
                }
                generations.push(read_json(&generation.path().join("metadata.json"))?);
            }
        }
        Ok(generations)
    }

    fn protected_generation_ids(
        &self,
        generations: &[StoredGeneration],
        retained_generations: usize,
    ) -> Result<BTreeSet<String>> {
        let mut protected = BTreeSet::new();
        for activation in fs::read_dir(self.root.join("activations"))? {
            let activation = activation?;
            if activation.file_type()?.is_file() {
                protected.insert(read_json::<ActivationRecord>(&activation.path())?.generation_id);
            }
        }
        let mut by_scope = BTreeMap::<String, Vec<&StoredGeneration>>::new();
        for generation in generations {
            if matches!(
                generation.state,
                GenerationState::MissingBlobs
                    | GenerationState::Ready
                    | GenerationState::StagingIndex
                    | GenerationState::Active
                    | GenerationState::MissingBlobData
            ) {
                protected.insert(generation.generation_id.clone());
            }
            by_scope
                .entry(scope_hash(&generation.descriptor.scope))
                .or_default()
                .push(generation);
        }
        for scope_generations in by_scope.values_mut() {
            scope_generations.sort_by_key(|generation| std::cmp::Reverse(generation.ordinal));
            protected.extend(
                scope_generations
                    .iter()
                    .filter(|generation| generation.state == GenerationState::Superseded)
                    .take(retained_generations)
                    .map(|generation| generation.generation_id.clone()),
            );
        }
        Ok(protected)
    }

    fn update_desired_if_same(&self, generation: &StoredGeneration) -> Result<()> {
        let desired_path = self
            .root
            .join("desired")
            .join(format!("{}.json", scope_hash(&generation.descriptor.scope)));
        if desired_path.is_file()
            && read_json::<StoredGeneration>(&desired_path)?.generation_id
                == generation.generation_id
        {
            atomic_write_json(&desired_path, generation)?;
        }
        Ok(())
    }

    fn health_path(&self, project_id: &str, code: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update((project_id.len() as u64).to_be_bytes());
        hasher.update(project_id.as_bytes());
        hasher.update((code.len() as u64).to_be_bytes());
        hasher.update(code.as_bytes());
        self.root
            .join("health")
            .join(format!("{}.json", hex::encode(hasher.finalize())))
    }

    fn retirement_path(&self, selector: &str) -> PathBuf {
        self.root
            .join("retirements")
            .join(format!("{}.json", sha256_hex(selector.as_bytes())))
    }

    fn activation_project_for_generation(&self, generation_id: &str) -> Result<Option<String>> {
        for entry in fs::read_dir(self.root.join("activations"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let activation: ActivationRecord = read_json(&entry.path())?;
            if activation.generation_id == generation_id {
                return Ok(Some(activation.project_id));
            }
        }
        Ok(None)
    }

    fn generation_is_activated(&self, generation_id: &str) -> Result<bool> {
        Ok(self
            .activation_project_for_generation(generation_id)?
            .is_some())
    }

    fn missing_page(
        &self,
        generation: &str,
        missing: &[String],
        offset: usize,
    ) -> Result<MissingBlobsPage> {
        if offset > missing.len() {
            bail!("missing-blob cursor is out of range");
        }
        let stored = self.find_generation(generation)?;
        let sizes = self
            .load_generation_entries(&stored.descriptor.scope, generation)?
            .into_iter()
            .map(|entry| (entry.content_sha256, entry.size))
            .collect::<BTreeMap<_, _>>();
        let mut hashes = Vec::with_capacity(MISSING_PAGE_SIZE.min(missing.len() - offset));
        let mut position = offset;
        while position < missing.len() && hashes.len() < MISSING_PAGE_SIZE {
            let hash = &missing[position];
            let size = sizes
                .get(hash)
                .copied()
                .ok_or_else(|| anyhow!("missing-set hash is absent from generation manifest"))?;
            let path = self.blob_path(hash);
            if !path.is_file() || self.verified_blob_file(hash, size).is_err() {
                hashes.push(hash.clone());
            }
            position += 1;
        }
        let next_cursor = (position < missing.len()).then(|| encode_cursor(generation, position));
        Ok(MissingBlobsPage {
            generation_id: generation.to_string(),
            hashes,
            next_cursor,
        })
    }

    fn missing_hashes(&self, entries: &[ManifestEntry]) -> Result<Vec<String>> {
        let mut sizes = BTreeMap::<String, u64>::new();
        for entry in entries {
            if let Some(previous) = sizes.insert(entry.content_sha256.clone(), entry.size)
                && previous != entry.size
            {
                bail!("manifest assigns conflicting sizes to one blob hash");
            }
        }
        let mut missing = BTreeSet::new();
        for (hash, size) in sizes {
            let path = self.blob_path(&hash);
            if !path.is_file() || self.verified_blob_file(&hash, size).is_err() {
                if path.exists() {
                    self.forget_verified_blob(&hash)?;
                    quarantine_corrupt_blob(&path)?;
                }
                missing.insert(hash);
            }
        }
        Ok(missing.into_iter().collect())
    }

    fn next_ordinal(&self, scope: &PublishedScope) -> Result<u64> {
        let path = self
            .root
            .join("ordinals")
            .join(format!("{}.json", scope_hash(scope)));
        let current = if path.is_file() {
            read_json::<u64>(&path)?
        } else {
            0
        };
        let next = current
            .checked_add(1)
            .ok_or_else(|| anyhow!("producer ordinal exhausted"))?;
        atomic_write_json(&path, &next)?;
        Ok(next)
    }

    fn upload_producer_dir(&self, producer_id: &str) -> PathBuf {
        self.root.join("uploads").join(producer_hash(producer_id))
    }

    fn upload_dir(&self, producer_id: &str, upload_id: &str) -> PathBuf {
        self.upload_producer_dir(producer_id).join(upload_id)
    }

    fn generation_dir(&self, scope: &PublishedScope, generation: &str) -> PathBuf {
        self.root
            .join("scopes")
            .join(scope_hash(scope))
            .join("generations")
            .join(generation)
    }

    fn load_upload(&self, producer_id: &str, upload_id: &str) -> Result<UploadRecord> {
        validate_upload_id(upload_id)?;
        let record: UploadRecord =
            read_json(&self.upload_dir(producer_id, upload_id).join("upload.json"))?;
        if record.producer_id != producer_id || record.upload_id != upload_id {
            bail!("upload ownership mismatch");
        }
        Ok(record)
    }

    fn save_upload(&self, record: &UploadRecord) -> Result<()> {
        atomic_write_json(
            &self
                .upload_dir(&record.producer_id, &record.upload_id)
                .join("upload.json"),
            record,
        )
    }

    fn load_upload_entries(&self, record: &UploadRecord) -> Result<Vec<ManifestEntry>> {
        let mut entries = Vec::new();
        for page in 0..record.next_page {
            let path = self
                .upload_dir(&record.producer_id, &record.upload_id)
                .join("pages")
                .join(format!("{page:08}.json"));
            entries.extend(read_json::<Vec<ManifestEntry>>(&path)?);
        }
        Ok(entries)
    }
}

fn producer_hash(producer_id: &str) -> String {
    sha256_hex(producer_id.as_bytes())
}

fn unique_blob_sizes(entries: &[ManifestEntry]) -> Result<BTreeMap<String, u64>> {
    let mut sizes = BTreeMap::new();
    for entry in entries {
        if let Some(previous) = sizes.insert(entry.content_sha256.clone(), entry.size)
            && previous != entry.size
        {
            bail!("manifest assigns conflicting sizes to one blob hash");
        }
    }
    Ok(sizes)
}

fn validate_upload_id(upload_id: &str) -> Result<()> {
    let parsed = Uuid::parse_str(upload_id).context("invalid upload id")?;
    if parsed.to_string() != upload_id {
        bail!("upload id is not in canonical form");
    }
    Ok(())
}

fn encode_cursor(generation: &str, offset: usize) -> String {
    format!("{generation}:{offset}")
}

fn decode_cursor(generation: &str, cursor: Option<&str>) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let (cursor_generation, offset) = cursor
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid missing-blob cursor"))?;
    if cursor_generation != generation {
        bail!("stale missing-blob cursor");
    }
    offset.parse().context("invalid missing-blob cursor offset")
}

fn write_manifest_jsonl(path: &Path, entries: &[ManifestEntry]) -> Result<()> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry)?;
        bytes.push(b'\n');
    }
    atomic_write(path, &bytes)
}

fn read_manifest_jsonl(path: &Path) -> Result<Vec<ManifestEntry>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .filter(|line| line.as_ref().map_or(true, |line| !line.trim().is_empty()))
        .map(|line| {
            let line = line?;
            serde_json::from_str(&line).map_err(Into::into)
        })
        .collect()
}

fn verify_blob(path: &Path, expected_hash: &str, expected_size: u64) -> Result<()> {
    let mut file = open_blob_nofollow(path)?;
    blob_identity(&file, expected_size)?;
    verify_open_blob(&mut file, expected_hash)
}

fn open_blob_nofollow(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            bail!("stored blob path is a symlink");
        }
    }
    options.open(path).map_err(Into::into)
}

#[cfg(unix)]
fn blob_identity(file: &File, expected_size: u64) -> Result<BlobIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        bail!("stored blob metadata mismatch");
    }
    Ok(BlobIdentity {
        len: metadata.len(),
        modified_secs: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn blob_identity(file: &File, expected_size: u64) -> Result<BlobIdentity> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        bail!("stored blob metadata mismatch");
    }
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("stored blob modification time predates the epoch"))?;
    Ok(BlobIdentity {
        len: metadata.len(),
        modified_secs: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
        modified_nanos: i64::from(modified.subsec_nanos()),
    })
}

fn verify_open_blob(file: &mut File, expected_hash: &str) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut *file, &mut HashWriter(&mut hasher))?;
    if hex::encode(hasher.finalize()) != expected_hash {
        bail!("stored blob hash mismatch");
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn quarantine_corrupt_blob(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("blob path has no parent"))?;
    let quarantine = parent.join(format!(
        "{}.corrupt-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("blob"),
        Uuid::new_v4()
    ));
    fs::rename(path, &quarantine)?;
    sync_parent(&quarantine)
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    create_private_dir(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn tracing_rename_race(_error: &std::io::Error) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_code_source::{
        SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint, manifest_sha256, source_selector,
    };

    fn descriptor(entries: &[ManifestEntry]) -> GenerationDescriptor {
        let head = "b".repeat(40);
        GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope: PublishedScope {
                repo_id: "repo-family".into(),
                bbox_root_relpath: ".".into(),
            },
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, entries),
            manifest_sha256: manifest_sha256(entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.size).sum(),
        }
    }

    fn open_store(root: &Path) -> CodeSourceStore {
        CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap()
    }

    #[test]
    fn manifest_negotiation_and_blob_install_are_replayable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let bytes = b"fn main() {}\n";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/main.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let begin = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &begin.upload_id, 0, &entries)
            .unwrap();
        store
            .put_manifest_page("host-a", &begin.upload_id, 0, &entries)
            .unwrap();
        let missing = store.complete_manifest("host-a", &begin.upload_id).unwrap();
        assert_eq!(missing.hashes, vec![hash.clone()]);
        store
            .install_blob(
                "host-a",
                &begin.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        let missing = store
            .missing_blobs("host-a", &begin.upload_id, None)
            .unwrap();
        assert!(missing.hashes.is_empty());
        let ready = store.finalize_upload("host-a", &begin.upload_id).unwrap();
        assert_eq!(ready.state, GenerationState::Ready);
        assert_eq!(store.blob_verification_count(), 0);
    }

    #[test]
    fn unchanged_present_blob_is_hashed_once_across_publications() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"already cached source";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let blob = store.blob_path(&hash);
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::write(&blob, bytes).unwrap();

        for _ in 0..2 {
            let upload = store.begin_upload("host-a", descriptor(&entries)).unwrap();
            store
                .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
                .unwrap();
            let missing = store
                .complete_manifest("host-a", &upload.upload_id)
                .unwrap();
            assert!(missing.hashes.is_empty());
            store.finalize_upload("host-a", &upload.upload_id).unwrap();
        }

        assert_eq!(store.blob_verification_count(), 1);
    }

    #[test]
    fn blob_and_missing_page_activity_refresh_upload_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"upload activity";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let upload = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();

        let mut record = store.load_upload("host-a", &upload.upload_id).unwrap();
        record.updated_unix_secs = 0;
        store.save_upload(&record).unwrap();
        store
            .install_blob(
                "host-a",
                &upload.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        assert!(
            store
                .load_upload("host-a", &upload.upload_id)
                .unwrap()
                .updated_unix_secs
                > 0
        );

        let mut record = store.load_upload("host-a", &upload.upload_id).unwrap();
        record.updated_unix_secs = 0;
        store.save_upload(&record).unwrap();
        store
            .missing_blobs("host-a", &upload.upload_id, None)
            .unwrap();
        assert!(
            store
                .load_upload("host-a", &upload.upload_id)
                .unwrap()
                .updated_unix_secs
                > 0
        );
    }

    #[test]
    fn wrong_blob_is_rejected_without_install() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let good = b"good";
        let hash = sha256_hex(good);
        let entries = vec![ManifestEntry {
            relative_path: "a.rs".into(),
            content_sha256: hash.clone(),
            size: good.len() as u64,
        }];
        let begin = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &begin.upload_id, 0, &entries)
            .unwrap();
        store.complete_manifest("host-a", &begin.upload_id).unwrap();
        assert!(
            store
                .install_blob(
                    "host-a",
                    &begin.upload_id,
                    &hash,
                    good.len() as u64,
                    &b"evil"[..]
                )
                .is_err()
        );
        assert!(!store.blob_path(&hash).exists());
    }

    #[test]
    fn missing_cursor_pages_an_immutable_set_after_prior_blobs_arrive() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let entries = (0..=MISSING_PAGE_SIZE)
            .map(|index| {
                let bytes = format!("source-{index}");
                ManifestEntry {
                    relative_path: format!("src/{index:04}.rs"),
                    content_sha256: sha256_hex(bytes.as_bytes()),
                    size: bytes.len() as u64,
                }
            })
            .collect::<Vec<_>>();
        let begin = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &begin.upload_id, 0, &entries)
            .unwrap();
        let first = store.complete_manifest("host-a", &begin.upload_id).unwrap();
        assert_eq!(first.hashes.len(), MISSING_PAGE_SIZE);
        for hash in &first.hashes {
            let entry = entries
                .iter()
                .find(|entry| &entry.content_sha256 == hash)
                .unwrap();
            let bytes = entries
                .iter()
                .position(|candidate| candidate == entry)
                .map(|index| format!("source-{index}"))
                .unwrap();
            let path = store.blob_path(hash);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        let second = store
            .missing_blobs("host-a", &begin.upload_id, first.next_cursor.as_deref())
            .unwrap();
        assert_eq!(second.hashes.len(), 1);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn manifest_pages_cannot_exceed_descriptor_or_cross_page_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let first_entry = ManifestEntry {
            relative_path: "b.rs".into(),
            content_sha256: "b".repeat(64),
            size: 1,
        };
        let declared = descriptor(std::slice::from_ref(&first_entry));
        let upload = store.begin_upload("host-a", declared).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &[first_entry])
            .unwrap();
        assert!(
            store
                .put_manifest_page(
                    "host-a",
                    &upload.upload_id,
                    1,
                    &[ManifestEntry {
                        relative_path: "c.rs".into(),
                        content_sha256: "c".repeat(64),
                        size: 1,
                    }]
                )
                .is_err()
        );

        let ordered = vec![
            ManifestEntry {
                relative_path: "a.rs".into(),
                content_sha256: "a".repeat(64),
                size: 1,
            },
            ManifestEntry {
                relative_path: "b.rs".into(),
                content_sha256: "b".repeat(64),
                size: 1,
            },
        ];
        let upload = store.begin_upload("host-b", descriptor(&ordered)).unwrap();
        store
            .put_manifest_page("host-b", &upload.upload_id, 0, &ordered[1..])
            .unwrap();
        assert!(
            store
                .put_manifest_page("host-b", &upload.upload_id, 1, &ordered[..1])
                .is_err()
        );
    }

    #[test]
    fn active_generation_can_renegotiate_a_missing_blob() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"repair me";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let descriptor = descriptor(&entries);
        let first = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .put_manifest_page("host-a", &first.upload_id, 0, &entries)
            .unwrap();
        store.complete_manifest("host-a", &first.upload_id).unwrap();
        store
            .install_blob(
                "host-a",
                &first.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        let ready = store.finalize_upload("host-a", &first.upload_id).unwrap();
        store
            .mark_generation_state(
                &descriptor.scope,
                &ready.generation_id,
                GenerationState::Active,
                None,
            )
            .unwrap();
        store
            .save_activation(&ActivationRecord {
                version: STORE_VERSION,
                project_id: "project-a".into(),
                generation_id: ready.generation_id.clone(),
                selector: source_selector("project-a", &ready.generation_id),
                snapshot_id: "snapshot-a".into(),
                document_count: 1,
                entity_inventory_sha256: "c".repeat(64),
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: now_unix_secs(),
                cutback_pending: false,
                diagnostic: None,
            })
            .unwrap();
        fs::remove_file(store.blob_path(&hash)).unwrap();

        let repair = store.begin_upload("host-a", descriptor).unwrap();
        store
            .put_manifest_page("host-a", &repair.upload_id, 0, &entries)
            .unwrap();
        let missing = store
            .complete_manifest("host-a", &repair.upload_id)
            .unwrap();
        assert_eq!(missing.hashes, vec![hash.clone()]);
        assert_eq!(
            store
                .expected_blob_size("host-a", &repair.upload_id, &hash)
                .unwrap(),
            bytes.len() as u64
        );
        store
            .install_blob(
                "host-a",
                &repair.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        assert_eq!(
            store
                .finalize_upload("host-a", &repair.upload_id)
                .unwrap()
                .state,
            GenerationState::Active
        );
    }

    #[test]
    fn ordinals_are_scope_global_across_producer_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let descriptor = descriptor(&[]);
        for _ in 0..3 {
            let upload = store.begin_upload("old-host", descriptor.clone()).unwrap();
            store
                .complete_manifest("old-host", &upload.upload_id)
                .unwrap();
            store
                .finalize_upload("old-host", &upload.upload_id)
                .unwrap();
        }
        let replacement = store.begin_upload("new-host", descriptor).unwrap();
        assert_eq!(replacement.ordinal, 4);
        store
            .complete_manifest("new-host", &replacement.upload_id)
            .unwrap();
        let ready = store
            .finalize_upload("new-host", &replacement.upload_id)
            .unwrap();
        assert_eq!(ready.state, GenerationState::Ready);
    }

    #[test]
    fn expiry_removes_only_idle_upload_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let upload = store.begin_upload("host-a", descriptor(&[])).unwrap();
        let mut record = store.load_upload("host-a", &upload.upload_id).unwrap();
        record.updated_unix_secs = 0;
        store.save_upload(&record).unwrap();
        assert_eq!(store.expire_uploads(1).unwrap(), 1);
        assert!(store.load_upload("host-a", &upload.upload_id).is_err());
    }

    #[test]
    fn same_root_openers_share_limits_and_coordination_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.max_manifest_files = 1;
        let first = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let second = open_store(&root);
        assert!(Arc::ptr_eq(&first.shared, &second.shared));

        let entries = vec![
            ManifestEntry {
                relative_path: "a.rs".into(),
                content_sha256: "a".repeat(64),
                size: 1,
            },
            ManifestEntry {
                relative_path: "b.rs".into(),
                content_sha256: "b".repeat(64),
                size: 1,
            },
        ];
        assert!(second.begin_upload("host-a", descriptor(&entries)).is_err());
        first.update_limits(StoreLimits::default()).unwrap();
        assert!(second.begin_upload("host-a", descriptor(&entries)).is_ok());
    }

    #[test]
    fn gc_keeps_open_upload_blobs_and_reclaims_orphans() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let bytes = b"open upload";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let upload = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
            .unwrap();
        let protected = store.blob_path(&hash);
        fs::create_dir_all(protected.parent().unwrap()).unwrap();
        fs::write(&protected, bytes).unwrap();

        let orphan_bytes = b"orphan";
        let orphan_hash = sha256_hex(orphan_bytes);
        let orphan = store.blob_path(&orphan_hash);
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        fs::write(&orphan, orphan_bytes).unwrap();

        let stats = store.gc_blobs().unwrap();
        assert!(protected.is_file());
        assert!(!orphan.exists());
        assert_eq!(stats.reclaimed_blobs, 1);
        assert_eq!(stats.reclaimed_bytes, orphan_bytes.len() as u64);
    }

    #[test]
    fn gc_keeps_partial_generation_blobs_after_upload_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let bytes = b"partial generation";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let upload = store.begin_upload("host-a", descriptor(&entries)).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        store
            .install_blob(
                "host-a",
                &upload.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        let mut record = store.load_upload("host-a", &upload.upload_id).unwrap();
        record.updated_unix_secs = 0;
        store.save_upload(&record).unwrap();
        assert_eq!(store.expire_uploads(1).unwrap(), 1);

        let orphan_bytes = b"orphan";
        let orphan_hash = sha256_hex(orphan_bytes);
        let orphan = store.blob_path(&orphan_hash);
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        fs::write(&orphan, orphan_bytes).unwrap();

        let stats = store.gc_blobs().unwrap();
        assert!(store.blob_path(&hash).is_file());
        assert!(!orphan.exists());
        assert_eq!(stats.reclaimed_blobs, 1);
    }

    #[test]
    fn scrub_quarantines_corruption_and_degrades_active_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"good bytes";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let descriptor = descriptor(&entries);
        let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        store
            .install_blob(
                "host-a",
                &upload.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        let ready = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        store
            .save_activation(&ActivationRecord {
                version: STORE_VERSION,
                project_id: "project-a".into(),
                generation_id: ready.generation_id.clone(),
                selector: source_selector("project-a", &ready.generation_id),
                snapshot_id: "snapshot-a".into(),
                document_count: 1,
                entity_inventory_sha256: "c".repeat(64),
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: now_unix_secs(),
                cutback_pending: false,
                diagnostic: None,
            })
            .unwrap();
        store
            .mark_generation_state(
                &descriptor.scope,
                &ready.generation_id,
                GenerationState::Active,
                None,
            )
            .unwrap();
        fs::write(store.blob_path(&hash), b"bad bytes!").unwrap();

        let stats = store.scrub_retained().unwrap();
        assert_eq!(stats.degraded_generations, 1);
        assert_eq!(
            store
                .load_generation(&descriptor.scope, &ready.generation_id)
                .unwrap()
                .state,
            GenerationState::MissingBlobData
        );
        assert!(!store.blob_path(&hash).exists());
        assert!(
            store.health_records().unwrap().iter().any(
                |record| record.project_id == "project-a" && record.code == "missing_blob_data"
            )
        );
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        store.update_limits(limits).unwrap();
        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 1);
    }
}
