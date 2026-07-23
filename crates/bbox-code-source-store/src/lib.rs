//! Durable content-addressed upload and generation store for code sources.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bbox_code_source::{
    BeginUploadResponse, DEFAULT_MAX_MANIFEST_FILES, DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
    GenerationDescriptor, GenerationState, GenerationStatus, MAX_MANIFEST_PAGE_ENTRIES,
    ManifestEntry, MissingBlobsPage, generation_id, scope_hash,
    validate_collected_materialization_selector, validate_manifest, validate_producer_id,
    validate_sha256,
};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow,
};
use bbox_corpus_core::project_catalog::{MAX_PROJECT_CATALOG_ENTRIES, ProjectId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const STORE_VERSION: u32 = 1;
const MIGRATION_STORE_VERSION: u32 = 2;
const MISSING_PAGE_SIZE: usize = 1_000;
const MAX_RETIREMENT_SELECTOR_BYTES: usize = 1_024;
const MAX_SNAPSHOT_ID_BYTES: usize = 512;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const MAX_CHUNK_TARGET_KEY_BYTES: usize = 4_096;
const MAX_MIGRATION_RECORD_BYTES: usize = 512 * 1024 * 1024;
const MAX_STORED_GENERATION_RECORD_BYTES: usize = 64 * 1024;
const MAX_COLLISION_RETIREMENT_RECORD_BYTES: usize = 64 * 1024;
const MAX_MIGRATION_INVENTORY_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_MIGRATION_INVENTORY_ACTIVATIONS: usize = MAX_PROJECT_CATALOG_ENTRIES;
pub const MAX_MIGRATION_INVENTORY_GENERATIONS: usize = MAX_PROJECT_CATALOG_ENTRIES;
pub const MAX_MIGRATION_INVENTORY_COLLISION_RECORDS: usize = MAX_PROJECT_CATALOG_ENTRIES;
pub const MAX_MIGRATION_INVENTORY_RETIREMENTS: usize = MAX_PROJECT_CATALOG_ENTRIES;

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

/// Side-effect-free authority for the code-source store's cross-crate paths.
///
/// Construction validates the root lexically. It does not create, open, or
/// canonicalize any store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSourceStorePaths {
    root: PathBuf,
}

pub struct MigrationCodeSourceInventoryGuard<'a> {
    paths: &'a CodeSourceStorePaths,
    _anchor: StoreLockGuard,
}

impl CodeSourceStorePaths {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .context("resolving the code-source store root")?
                .join(root)
        };
        validate_store_root(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn anchor(&self) -> PathBuf {
        self.root.join("effective-source-manifest.json")
    }

    pub fn activation(&self, project_id: &ProjectId) -> PathBuf {
        self.root
            .join("activations")
            .join(format!("{project_id}.json"))
    }

    pub fn activation_for_str(&self, project_id: &str) -> Result<PathBuf> {
        let project_id =
            ProjectId::parse(project_id.to_string()).map_err(|error| anyhow!(error))?;
        Ok(self.activation(&project_id))
    }

    pub fn generation_metadata(
        &self,
        scope: &PublishedScope,
        generation_id: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .generation_directory(scope, generation_id)?
            .join("metadata.json"))
    }

    pub fn generation_manifest(
        &self,
        scope: &PublishedScope,
        generation_id: &str,
    ) -> Result<PathBuf> {
        Ok(self
            .generation_directory(scope, generation_id)?
            .join("manifest.jsonl"))
    }

    fn generation_directory(&self, scope: &PublishedScope, generation_id: &str) -> Result<PathBuf> {
        scope.validate()?;
        validate_sha256(generation_id)?;
        Ok(self
            .root
            .join("scopes")
            .join(scope_hash(scope))
            .join("generations")
            .join(generation_id))
    }

    pub fn collision_retirement_pending(&self, project_id: &ProjectId) -> PathBuf {
        self.root
            .join("collision-retirements")
            .join(format!("{project_id}.json"))
    }

    pub fn retirement_for_selector(&self, selector: &str) -> Result<PathBuf> {
        validate_retirement_selector(selector)?;
        Ok(self.retirement_for_validated_selector_hash(&sha256_hex(selector.as_bytes())))
    }

    pub fn retirement_for_selector_hash(&self, selector_hash: &str) -> Result<PathBuf> {
        validate_sha256(selector_hash)?;
        Ok(self.retirement_for_validated_selector_hash(selector_hash))
    }

    fn retirement_for_validated_selector_hash(&self, selector_hash: &str) -> PathBuf {
        self.root
            .join("retirements")
            .join(format!("{selector_hash}.json"))
    }

    /// Acquire the code-owned anchor lock for a coherent, side-effect-free
    /// migration inventory snapshot.
    pub fn lock_migration_inventory(&self) -> Result<MigrationCodeSourceInventoryGuard<'_>> {
        Ok(MigrationCodeSourceInventoryGuard {
            paths: self,
            _anchor: acquire_store_lock_nofollow(&self.anchor())?,
        })
    }
}

impl MigrationCodeSourceInventoryGuard<'_> {
    pub fn snapshot_legacy_v1(&self, limits: &StoreLimits) -> Result<MigrationLegacyInventoryV1> {
        enumerate_legacy_migration_inventory_locked(self.paths, limits)
    }

    pub fn snapshot_current_v2(&self, limits: &StoreLimits) -> Result<MigrationCurrentInventoryV1> {
        enumerate_current_migration_inventory_locked(self.paths, limits)
    }
}

pub struct CodeSourceStore {
    paths: CodeSourceStorePaths,
    shared: Arc<SharedStoreState>,
}

struct StoreMutationGuard<'a> {
    _anchor: StoreLockGuard,
    _in_process: MutexGuard<'a, ()>,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StoredGenerationV2 {
    pub version: u32,
    pub generation_id: String,
    pub producer_id: String,
    pub ordinal: u64,
    pub descriptor: GenerationDescriptor,
    pub published_scope: PublishedScope,
    pub state: GenerationState,
    pub diagnostic: Option<String>,
    pub created_unix_secs: u64,
    pub materialized_doc_count: Option<u64>,
    pub entity_inventory_sha256: Option<String>,
}

impl StoredGenerationV2 {
    pub fn from_v1_for_migration(
        legacy: StoredGeneration,
        published_scope: PublishedScope,
    ) -> Result<Self> {
        validate_stored_generation_v1(&legacy)?;
        let record = Self {
            version: MIGRATION_STORE_VERSION,
            generation_id: legacy.generation_id,
            producer_id: legacy.producer_id,
            ordinal: legacy.ordinal,
            descriptor: legacy.descriptor,
            published_scope,
            state: legacy.state,
            diagnostic: legacy.diagnostic,
            created_unix_secs: legacy.created_unix_secs,
            materialized_doc_count: legacy.materialized_doc_count,
            entity_inventory_sha256: legacy.entity_inventory_sha256,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != MIGRATION_STORE_VERSION {
            bail!("invalid stored generation v2 version");
        }
        validate_sha256(&self.generation_id)?;
        validate_producer_id(&self.producer_id)?;
        self.descriptor.validate_header()?;
        self.published_scope.validate()?;
        if self.published_scope != self.descriptor.scope {
            bail!("stored generation published scope does not match descriptor");
        }
        if generation_id(&self.producer_id, &self.descriptor) != self.generation_id {
            bail!("stored generation identity does not match descriptor");
        }
        validate_optional_diagnostic(self.diagnostic.as_deref())?;
        match (
            self.materialized_doc_count,
            self.entity_inventory_sha256.as_deref(),
        ) {
            (Some(_), Some(hash)) => validate_sha256(hash)?,
            (None, None) => {}
            _ => bail!("stored generation materialization evidence is incomplete"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivationRecordV2 {
    pub version: u32,
    pub project_id: ProjectId,
    pub published_scope: PublishedScope,
    pub generation_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub current_chunk_targets: BTreeMap<String, EntityRef>,
    pub activated_unix_secs: u64,
    pub cutback_pending: bool,
    pub diagnostic: Option<String>,
}

impl ActivationRecordV2 {
    pub fn from_v1_for_migration(
        legacy: ActivationRecord,
        generation: &StoredGenerationV2,
    ) -> Result<Self> {
        validate_activation_v1(&legacy)?;
        let record = Self {
            version: MIGRATION_STORE_VERSION,
            project_id: ProjectId::parse(legacy.project_id).map_err(|error| anyhow!(error))?,
            published_scope: generation.published_scope.clone(),
            generation_id: legacy.generation_id,
            selector: legacy.selector,
            snapshot_id: legacy.snapshot_id,
            document_count: legacy.document_count,
            entity_inventory_sha256: legacy.entity_inventory_sha256,
            current_chunk_targets: legacy.current_chunk_targets,
            activated_unix_secs: legacy.activated_unix_secs,
            cutback_pending: legacy.cutback_pending,
            diagnostic: legacy.diagnostic,
        };
        record.validate_against_generation(generation)?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != MIGRATION_STORE_VERSION {
            bail!("invalid activation record v2 version");
        }
        ProjectId::parse(self.project_id.to_string()).map_err(|error| anyhow!(error))?;
        self.published_scope.validate()?;
        validate_sha256(&self.generation_id)?;
        validate_retirement_selector(&self.selector)?;
        validate_collected_materialization_selector(
            self.project_id.as_str(),
            &self.generation_id,
            &self.selector,
        )?;
        validate_migration_snapshot_id(&self.snapshot_id)?;
        validate_sha256(&self.entity_inventory_sha256)?;
        validate_optional_diagnostic(self.diagnostic.as_deref())?;
        if self.current_chunk_targets.len() > DEFAULT_MAX_MANIFEST_FILES as usize {
            bail!("activation record has too many chunk targets");
        }
        for (key, target) in &self.current_chunk_targets {
            if key.trim().is_empty()
                || key.len() > MAX_CHUNK_TARGET_KEY_BYTES
                || key.chars().any(char::is_control)
            {
                bail!("activation record has an invalid chunk target key");
            }
            target
                .try_render()
                .map_err(|error| anyhow!("activation record has an invalid entity ref: {error}"))?;
        }
        Ok(())
    }

    pub fn validate_against_generation(&self, generation: &StoredGenerationV2) -> Result<()> {
        self.validate()?;
        generation.validate()?;
        if self.published_scope != generation.published_scope
            || self.generation_id != generation.generation_id
            || Some(self.document_count) != generation.materialized_doc_count
            || Some(self.entity_inventory_sha256.as_str())
                != generation.entity_inventory_sha256.as_deref()
        {
            bail!("activation record does not match stored generation");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollisionRetirementPendingV1 {
    pub version: u32,
    pub project_id: ProjectId,
    pub former_scope: PublishedScope,
    pub generation_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub manifest_sha256: String,
    pub inventory_hash: String,
    pub plan_hash: String,
}

impl CollisionRetirementPendingV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION {
            bail!("invalid collision retirement pending version");
        }
        ProjectId::parse(self.project_id.to_string()).map_err(|error| anyhow!(error))?;
        self.former_scope.validate()?;
        validate_sha256(&self.generation_id)?;
        validate_retirement_selector(&self.selector)?;
        validate_collected_materialization_selector(
            self.project_id.as_str(),
            &self.generation_id,
            &self.selector,
        )?;
        validate_migration_snapshot_id(&self.snapshot_id)?;
        validate_sha256(&self.manifest_sha256)?;
        validate_sha256(&self.inventory_hash)?;
        validate_sha256(&self.plan_hash)?;
        Ok(())
    }
}

/// Strict migration-owned view of the complete effective code-source set.
///
/// The ordinary code-source store does not infer this state from activation
/// filenames. Migration supplies and verifies the complete sorted selection
/// set so removals and quarantines are explicit post-image facts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationEffectiveSourceManifestV1 {
    pub version: u32,
    pub selections: Vec<MigrationEffectiveSourceSelectionV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MigrationEffectiveSourceSelectionV1 {
    pub project_id: ProjectId,
    pub published_scope: PublishedScope,
    pub generation_id: String,
    pub selector: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationLegacyAnchorEvidenceV1 {
    Missing,
    Present { bytes: Vec<u8>, sha256: String },
}

#[derive(Debug, Clone)]
pub struct MigrationLegacyActivationEvidenceV1 {
    pub project_id: ProjectId,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: ActivationRecord,
}

#[derive(Debug, Clone)]
pub struct MigrationLegacyGenerationEvidenceV1 {
    pub published_scope: PublishedScope,
    pub generation_id: String,
    pub metadata_bytes: Vec<u8>,
    pub metadata_sha256: String,
    pub record: StoredGeneration,
    pub manifest_bytes: Vec<u8>,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone)]
pub struct MigrationLegacyCollisionEvidenceV1 {
    pub project_id: ProjectId,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: CollisionRetirementPendingV1,
}

#[derive(Debug, Clone)]
pub struct MigrationLegacyInventoryV1 {
    pub anchor: MigrationLegacyAnchorEvidenceV1,
    pub activations: Vec<MigrationLegacyActivationEvidenceV1>,
    /// The bounded set of rows that migration must preserve. Rows outside this
    /// set are fully enumerated in the generation-set evidence below, but are
    /// classified as non-surviving GC candidates and never become transaction
    /// participants.
    pub generations: Vec<MigrationLegacyGenerationEvidenceV1>,
    pub collision_pending: Vec<MigrationLegacyCollisionEvidenceV1>,
    pub protected_generation_ids: BTreeSet<String>,
    pub generation_count: u64,
    pub generation_set_sha256: String,
    pub unprotected_generation_count: u64,
    pub unprotected_generation_set_sha256: String,
    pub canonical_sha256: String,
}

#[derive(Debug, Clone)]
pub struct MigrationCurrentActivationEvidenceV1 {
    pub project_id: ProjectId,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: ActivationRecordV2,
}

#[derive(Debug, Clone)]
pub struct MigrationCurrentGenerationEvidenceV1 {
    pub published_scope: PublishedScope,
    pub generation_id: String,
    pub metadata_bytes: Vec<u8>,
    pub metadata_sha256: String,
    pub record: StoredGenerationV2,
    pub manifest_bytes: Vec<u8>,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone)]
pub struct MigrationCurrentCollisionEvidenceV1 {
    pub project_id: ProjectId,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: CollisionRetirementPendingV1,
}

#[derive(Debug, Clone)]
pub struct MigrationCurrentRetirementEvidenceV1 {
    pub selector_sha256: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: RetirementRecord,
}

#[derive(Debug, Clone)]
pub struct MigrationCurrentInventoryV1 {
    pub effective_manifest_bytes: Vec<u8>,
    pub effective_manifest_sha256: String,
    pub effective_manifest: MigrationEffectiveSourceManifestV1,
    pub activations: Vec<MigrationCurrentActivationEvidenceV1>,
    pub generations: Vec<MigrationCurrentGenerationEvidenceV1>,
    pub collision_pending: Vec<MigrationCurrentCollisionEvidenceV1>,
    pub retirements: Vec<MigrationCurrentRetirementEvidenceV1>,
    pub canonical_sha256: String,
}

impl MigrationLegacyInventoryV1 {
    pub fn validate_evidence(&self) -> Result<()> {
        validate_sha256(&self.generation_set_sha256)?;
        validate_sha256(&self.unprotected_generation_set_sha256)?;
        validate_sha256(&self.canonical_sha256)?;
        let survivor_ids = self
            .generations
            .iter()
            .map(|row| row.generation_id.clone())
            .collect::<BTreeSet<_>>();
        if survivor_ids.len() != self.generations.len()
            || survivor_ids != self.protected_generation_ids
            || self.generation_count
                != (self.generations.len() as u64)
                    .checked_add(self.unprotected_generation_count)
                    .ok_or_else(|| anyhow!("legacy generation evidence count overflowed"))?
            || self.canonical_sha256 != legacy_inventory_digest(self)
        {
            bail!("legacy migration inventory evidence is incomplete or inconsistent");
        }
        Ok(())
    }
}

impl MigrationEffectiveSourceManifestV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("invalid migration effective-source manifest version");
        }
        if self.selections.len() > MAX_PROJECT_CATALOG_ENTRIES {
            bail!("migration effective-source manifest has too many selections");
        }
        let mut projects = BTreeSet::new();
        let mut selectors = BTreeSet::new();
        let mut prior_project = None;
        for selection in &self.selections {
            ProjectId::parse(selection.project_id.to_string()).map_err(|error| anyhow!(error))?;
            selection.published_scope.validate()?;
            validate_sha256(&selection.generation_id)?;
            validate_retirement_selector(&selection.selector)?;
            if validate_collected_materialization_selector(
                selection.project_id.as_str(),
                &selection.generation_id,
                &selection.selector,
            )
            .is_err()
                || !projects.insert(selection.project_id.clone())
                || !selectors.insert(selection.selector.clone())
                || prior_project
                    .as_ref()
                    .is_some_and(|project: &ProjectId| project >= &selection.project_id)
            {
                bail!("migration effective-source selections are invalid, duplicated, or unsorted");
            }
            prior_project = Some(selection.project_id.clone());
        }
        Ok(())
    }
}

pub fn encode_migration_effective_source_manifest_v1(
    manifest: &MigrationEffectiveSourceManifestV1,
) -> Result<Vec<u8>> {
    manifest.validate()?;
    encode_bounded_json(
        manifest,
        MAX_MIGRATION_RECORD_BYTES,
        "migration effective-source manifest",
    )
}

pub fn decode_migration_effective_source_manifest_v1(
    bytes: &[u8],
) -> Result<MigrationEffectiveSourceManifestV1> {
    let manifest: MigrationEffectiveSourceManifestV1 = decode_bounded_json(
        bytes,
        MAX_MIGRATION_RECORD_BYTES,
        "migration effective-source manifest",
    )?;
    manifest.validate()?;
    Ok(manifest)
}

#[derive(Default)]
struct MigrationGenerationSetAccumulator {
    count: u64,
    sum: [u8; 32],
}

impl MigrationGenerationSetAccumulator {
    fn add(&mut self, row: &MigrationLegacyGenerationEvidenceV1) -> Result<()> {
        fn field(hasher: &mut Sha256, value: &[u8]) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }

        let mut leaf = Sha256::new();
        field(&mut leaf, b"bbox-code-source-legacy-generation-row-v1");
        field(&mut leaf, row.published_scope.repo_id().as_bytes());
        field(
            &mut leaf,
            row.published_scope.bbox_root_relpath().as_bytes(),
        );
        field(&mut leaf, row.generation_id.as_bytes());
        field(&mut leaf, row.metadata_sha256.as_bytes());
        field(&mut leaf, row.manifest_sha256.as_bytes());
        field(&mut leaf, row.record.descriptor.manifest_sha256.as_bytes());
        let digest: [u8; 32] = leaf.finalize().into();
        let mut carry = 0_u16;
        for (target, source) in self.sum.iter_mut().rev().zip(digest.iter().rev()) {
            let value = u16::from(*target) + u16::from(*source) + carry;
            *target = value as u8;
            carry = value >> 8;
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| anyhow!("legacy generation count overflowed"))?;
        Ok(())
    }

    fn digest(&self, domain: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update((domain.len() as u64).to_be_bytes());
        hasher.update(domain);
        hasher.update(self.count.to_be_bytes());
        hasher.update(self.sum);
        hex::encode(hasher.finalize())
    }
}

fn walk_legacy_generation_rows(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    mut visit: impl FnMut(MigrationLegacyGenerationEvidenceV1) -> Result<()>,
) -> Result<()> {
    let scopes_path = paths.root().join("scopes");
    let Some(scopes_directory) = NofollowDirectory::open_existing(&scopes_path)? else {
        return Ok(());
    };
    for scope_entry in fs::read_dir(&scopes_path)? {
        let scope_entry = scope_entry?;
        let file_type = scope_entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            bail!("legacy scope directory contains an unexpected entry type");
        }
        let scope_name = scope_entry
            .file_name()
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("legacy scope directory contains a non-utf8 entry"))?;
        validate_sha256(&scope_name)?;
        let scope_path = scopes_path.join(&scope_name);
        let scope_entries = sorted_directory_entry_names(&scope_path, 1, "legacy scope")?;
        if scope_entries.len() != 1 || scope_entries[0] != "generations" {
            bail!("legacy scope directory has an incomplete or unexpected row set");
        }
        let generations_path = scope_path.join("generations");
        let generations_directory = NofollowDirectory::open_existing(&generations_path)?
            .ok_or_else(|| anyhow!("legacy generations directory disappeared"))?;
        for generation_entry in fs::read_dir(&generations_path)? {
            let generation_entry = generation_entry?;
            let file_type = generation_entry.file_type()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                bail!("legacy generation directory contains an unexpected entry type");
            }
            let generation_id = generation_entry
                .file_name()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("legacy generation directory contains a non-utf8 entry"))?;
            validate_sha256(&generation_id)?;
            let generation_path = generations_path.join(&generation_id);
            let directory = NofollowDirectory::open_existing(&generation_path)?
                .ok_or_else(|| anyhow!("legacy generation directory disappeared"))?;
            let entries = sorted_regular_entry_names(&generation_path, 2, "legacy generation")?;
            if entries.len() != 2 || entries[0] != "manifest.jsonl" || entries[1] != "metadata.json"
            {
                bail!("legacy generation directory has an incomplete or unexpected row set");
            }
            let metadata_bytes = directory
                .read_regular(
                    "metadata.json",
                    MAX_STORED_GENERATION_RECORD_BYTES,
                    "legacy generation metadata",
                )?
                .ok_or_else(|| anyhow!("legacy generation metadata is missing"))?;
            let record = decode_stored_generation_v1_for_migration(&metadata_bytes)?;
            if record.generation_id != generation_id
                || scope_hash(&record.descriptor.scope) != scope_name
            {
                bail!("legacy generation path and metadata disagree");
            }
            let manifest_bytes = directory
                .read_regular(
                    "manifest.jsonl",
                    MAX_MIGRATION_RECORD_BYTES,
                    "legacy generation manifest",
                )?
                .ok_or_else(|| anyhow!("legacy generation manifest is missing"))?;
            verify_generation_manifest_for_migration(
                &manifest_bytes,
                &record.descriptor,
                &record.producer_id,
                &record.generation_id,
                limits,
            )?;
            visit(MigrationLegacyGenerationEvidenceV1 {
                published_scope: record.descriptor.scope.clone(),
                generation_id,
                metadata_sha256: sha256_hex(&metadata_bytes),
                metadata_bytes,
                manifest_sha256: sha256_hex(&manifest_bytes),
                manifest_bytes,
                record,
            })?;
            directory.ensure_still_current()?;
        }
        generations_directory.ensure_still_current()?;
    }
    scopes_directory.ensure_still_current()?;
    Ok(())
}

/// Enumerate the complete legacy v1 source store without creating state.
///
/// The caller must hold the mutation lock for [`CodeSourceStorePaths::anchor`]
/// for the full call. Every file is opened through a held nofollow directory.
pub fn enumerate_legacy_migration_inventory_locked(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
) -> Result<MigrationLegacyInventoryV1> {
    let anchor_bytes = read_optional_regular_nofollow(
        &paths.anchor(),
        MAX_MIGRATION_RECORD_BYTES,
        "legacy effective source anchor",
    )?;
    let mut total_encoded_bytes = anchor_bytes.as_ref().map_or(0, Vec::len);
    let anchor = match anchor_bytes {
        Some(bytes) => MigrationLegacyAnchorEvidenceV1::Present {
            sha256: sha256_hex(&bytes),
            bytes,
        },
        None => MigrationLegacyAnchorEvidenceV1::Missing,
    };
    let Some(_root) = NofollowDirectory::open_existing(paths.root())? else {
        let mut inventory = MigrationLegacyInventoryV1 {
            anchor,
            activations: Vec::new(),
            generations: Vec::new(),
            collision_pending: Vec::new(),
            protected_generation_ids: BTreeSet::new(),
            generation_count: 0,
            generation_set_sha256: MigrationGenerationSetAccumulator::default()
                .digest(b"bbox-code-source-legacy-generation-set-v1"),
            unprotected_generation_count: 0,
            unprotected_generation_set_sha256: MigrationGenerationSetAccumulator::default()
                .digest(b"bbox-code-source-legacy-unprotected-generation-set-v1"),
            canonical_sha256: String::new(),
        };
        inventory.canonical_sha256 = legacy_inventory_digest(&inventory);
        inventory.validate_evidence()?;
        return Ok(inventory);
    };

    let mut activations = Vec::new();
    if let Some(directory) = NofollowDirectory::open_existing(&paths.root().join("activations"))? {
        for name in sorted_regular_entry_names(
            &paths.root().join("activations"),
            MAX_MIGRATION_INVENTORY_ACTIVATIONS,
            "legacy activation",
        )? {
            let project_name = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("legacy activation filename is not canonical"))?;
            let project_id =
                ProjectId::parse(project_name.to_string()).map_err(|error| anyhow!(error))?;
            let bytes = directory
                .read_regular(&name, MAX_MIGRATION_RECORD_BYTES, "legacy activation")?
                .ok_or_else(|| anyhow!("legacy activation disappeared during enumeration"))?;
            total_encoded_bytes = total_encoded_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| anyhow!("legacy inventory byte count overflowed"))?;
            if total_encoded_bytes > MAX_MIGRATION_INVENTORY_MANIFEST_BYTES {
                bail!("legacy inventory exceeds its aggregate byte limit");
            }
            let record = decode_activation_v1_for_migration(&bytes)?;
            if record.project_id != project_id.as_str() {
                bail!("legacy activation path and record project disagree");
            }
            activations.push(MigrationLegacyActivationEvidenceV1 {
                project_id,
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
        }
    }

    let mut collision_pending = Vec::new();
    let collision_path = paths.root().join("collision-retirements");
    if let Some(directory) = NofollowDirectory::open_existing(&collision_path)? {
        for name in sorted_regular_entry_names(
            &collision_path,
            MAX_MIGRATION_INVENTORY_COLLISION_RECORDS,
            "legacy collision retirement",
        )? {
            let project_name = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("collision retirement filename is not canonical"))?;
            let project_id =
                ProjectId::parse(project_name.to_string()).map_err(|error| anyhow!(error))?;
            let bytes = directory
                .read_regular(
                    &name,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "legacy collision retirement",
                )?
                .ok_or_else(|| anyhow!("collision retirement disappeared"))?;
            let record = decode_collision_retirement_pending_for_migration(&bytes)?;
            total_encoded_bytes = total_encoded_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| anyhow!("legacy inventory byte count overflowed"))?;
            if total_encoded_bytes > MAX_MIGRATION_INVENTORY_MANIFEST_BYTES {
                bail!("legacy inventory exceeds its aggregate byte limit");
            }
            if record.project_id != project_id {
                bail!("collision retirement path and record project disagree");
            }
            collision_pending.push(MigrationLegacyCollisionEvidenceV1 {
                project_id,
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
        }
    }

    let root_generation_ids = activations
        .iter()
        .map(|row| row.record.generation_id.clone())
        .chain(
            collision_pending
                .iter()
                .map(|row| row.record.generation_id.clone()),
        )
        .collect::<BTreeSet<_>>();
    let mut found_root_generation_ids = BTreeSet::new();
    let mut full_generation_set = MigrationGenerationSetAccumulator::default();
    let mut generations = Vec::new();
    let mut protected_identities = BTreeSet::new();
    let mut retained_by_scope =
        BTreeMap::<PublishedScope, Vec<MigrationLegacyGenerationEvidenceV1>>::new();
    walk_legacy_generation_rows(paths, limits, |row| {
        full_generation_set.add(&row)?;
        let rooted = root_generation_ids.contains(&row.generation_id);
        if rooted {
            found_root_generation_ids.insert(row.generation_id.clone());
        }
        let intrinsically_protected = rooted
            || matches!(
                row.record.state,
                GenerationState::MissingBlobs
                    | GenerationState::Ready
                    | GenerationState::StagingIndex
                    | GenerationState::Active
                    | GenerationState::MissingBlobData
            );
        if intrinsically_protected {
            if generations.len() >= MAX_MIGRATION_INVENTORY_GENERATIONS {
                bail!("protected legacy generation inventory exceeds its row limit");
            }
            protected_identities
                .insert((scope_hash(&row.published_scope), row.generation_id.clone()));
            generations.push(row);
        } else if row.record.state == GenerationState::Superseded && limits.retained_generations > 0
        {
            let scope = row.published_scope.clone();
            let candidates = retained_by_scope.entry(scope).or_default();
            candidates.push(row);
            candidates.sort_by(|left, right| {
                right
                    .record
                    .ordinal
                    .cmp(&left.record.ordinal)
                    .then_with(|| left.generation_id.cmp(&right.generation_id))
            });
            candidates.truncate(limits.retained_generations);
        }
        Ok(())
    })?;
    if found_root_generation_ids != root_generation_ids {
        bail!("legacy activation or collision references missing generation metadata");
    }
    for candidates in retained_by_scope.into_values() {
        for row in candidates {
            protected_identities
                .insert((scope_hash(&row.published_scope), row.generation_id.clone()));
            generations.push(row);
        }
    }
    generations.sort_by(|left, right| {
        left.published_scope
            .cmp(&right.published_scope)
            .then_with(|| left.generation_id.cmp(&right.generation_id))
    });
    let activation_records = activations
        .iter()
        .map(|row| row.record.clone())
        .collect::<Vec<_>>();
    let collision_records = collision_pending
        .iter()
        .map(|row| row.record.clone())
        .collect::<Vec<_>>();
    let generation_records = generations
        .iter()
        .map(|row| row.record.clone())
        .collect::<Vec<_>>();
    let protected_generation_ids = protected_generation_ids_from_records(
        &generation_records,
        &activation_records,
        &collision_records,
        limits.retained_generations,
    )?;
    if protected_generation_ids.iter().any(|generation_id| {
        !generations
            .iter()
            .any(|row| &row.generation_id == generation_id)
    }) {
        bail!("bounded legacy survivor materialization omits a protected generation");
    }
    generations.retain(|row| protected_generation_ids.contains(&row.generation_id));
    if generations.len() > MAX_MIGRATION_INVENTORY_GENERATIONS {
        bail!("protected legacy generation inventory exceeds its row limit");
    }
    protected_identities = generations
        .iter()
        .map(|row| (scope_hash(&row.published_scope), row.generation_id.clone()))
        .collect();
    let survivor_bytes = generations.iter().try_fold(0_usize, |total, row| {
        total
            .checked_add(row.metadata_bytes.len())
            .and_then(|value| value.checked_add(row.manifest_bytes.len()))
            .ok_or_else(|| anyhow!("protected legacy inventory byte count overflowed"))
    })?;
    total_encoded_bytes = total_encoded_bytes
        .checked_add(survivor_bytes)
        .ok_or_else(|| anyhow!("protected legacy inventory byte count overflowed"))?;
    if total_encoded_bytes > MAX_MIGRATION_INVENTORY_MANIFEST_BYTES {
        bail!("protected legacy inventory exceeds its aggregate byte limit");
    }

    let mut repeated_full_generation_set = MigrationGenerationSetAccumulator::default();
    let mut unprotected_generation_set = MigrationGenerationSetAccumulator::default();
    walk_legacy_generation_rows(paths, limits, |row| {
        repeated_full_generation_set.add(&row)?;
        let identity = (scope_hash(&row.published_scope), row.generation_id.clone());
        if !protected_identities.contains(&identity) {
            unprotected_generation_set.add(&row)?;
        }
        Ok(())
    })?;
    let generation_set_sha256 =
        full_generation_set.digest(b"bbox-code-source-legacy-generation-set-v1");
    if repeated_full_generation_set.count != full_generation_set.count
        || repeated_full_generation_set.digest(b"bbox-code-source-legacy-generation-set-v1")
            != generation_set_sha256
    {
        bail!("legacy generation row set changed during enumeration");
    }

    if let MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } = &anchor {
        let effective = decode_migration_effective_source_manifest_v1(bytes)?;
        let activations_by_project = activations
            .iter()
            .map(|row| (&row.project_id, &row.record))
            .collect::<BTreeMap<_, _>>();
        let generations_by_id = generations
            .iter()
            .map(|row| (row.generation_id.as_str(), row))
            .collect::<BTreeMap<_, _>>();
        if effective.selections.len() != activations.len() {
            bail!("legacy effective source manifest and activation sets differ");
        }
        for selection in &effective.selections {
            let activation = activations_by_project
                .get(&selection.project_id)
                .ok_or_else(|| anyhow!("legacy effective source selection lacks activation"))?;
            let generation = generations_by_id
                .get(selection.generation_id.as_str())
                .ok_or_else(|| anyhow!("legacy effective source selection lacks generation"))?;
            if activation.generation_id != selection.generation_id
                || activation.selector != selection.selector
                || generation.published_scope != selection.published_scope
            {
                bail!("legacy effective source selection rewrites activation evidence");
            }
        }
    }
    let mut inventory = MigrationLegacyInventoryV1 {
        anchor,
        activations,
        generations,
        collision_pending,
        protected_generation_ids,
        generation_count: full_generation_set.count,
        generation_set_sha256,
        unprotected_generation_count: unprotected_generation_set.count,
        unprotected_generation_set_sha256: unprotected_generation_set
            .digest(b"bbox-code-source-legacy-unprotected-generation-set-v1"),
        canonical_sha256: String::new(),
    };
    inventory.canonical_sha256 = legacy_inventory_digest(&inventory);
    inventory.validate_evidence()?;
    Ok(inventory)
}

/// Enumerate and validate the complete current v2 migration-owned source state.
///
/// The caller must hold the mutation lock for [`CodeSourceStorePaths::anchor`]
/// for the full call. Valid later generations and selections are accepted.
/// Scopeless v1 rows classified as non-surviving GC candidates are ignored;
/// any active, rooted, or retention-protected v1 row fails closed.
pub fn enumerate_current_migration_inventory_locked(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
) -> Result<MigrationCurrentInventoryV1> {
    let effective_manifest_bytes = read_optional_regular_nofollow(
        &paths.anchor(),
        MAX_MIGRATION_RECORD_BYTES,
        "current effective source anchor",
    )?
    .ok_or_else(|| anyhow!("current effective source anchor is missing"))?;
    let effective_manifest =
        decode_migration_effective_source_manifest_v1(&effective_manifest_bytes)?;
    let mut total_encoded_bytes = effective_manifest_bytes.len();

    let activation_path = paths.root().join("activations");
    let mut activations = Vec::new();
    if let Some(directory) = NofollowDirectory::open_existing(&activation_path)? {
        for name in sorted_regular_entry_names(
            &activation_path,
            MAX_MIGRATION_INVENTORY_ACTIVATIONS,
            "current activation",
        )? {
            let project_name = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("current activation filename is not canonical"))?;
            let project_id =
                ProjectId::parse(project_name.to_string()).map_err(|error| anyhow!(error))?;
            let bytes = directory
                .read_regular(&name, MAX_MIGRATION_RECORD_BYTES, "current activation")?
                .ok_or_else(|| anyhow!("current activation disappeared during enumeration"))?;
            total_encoded_bytes = checked_inventory_bytes(total_encoded_bytes, bytes.len())?;
            let record = decode_activation_v2_for_migration(&bytes)?;
            if record.project_id != project_id {
                bail!("current activation path and record project disagree");
            }
            activations.push(MigrationCurrentActivationEvidenceV1 {
                project_id,
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
        }
        directory.ensure_still_current()?;
    }

    let collision_path = paths.root().join("collision-retirements");
    let mut collision_pending = Vec::new();
    if let Some(directory) = NofollowDirectory::open_existing(&collision_path)? {
        for name in sorted_regular_entry_names(
            &collision_path,
            MAX_MIGRATION_INVENTORY_COLLISION_RECORDS,
            "current collision retirement",
        )? {
            let project_name = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("current collision filename is not canonical"))?;
            let project_id =
                ProjectId::parse(project_name.to_string()).map_err(|error| anyhow!(error))?;
            let bytes = directory
                .read_regular(
                    &name,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "current collision retirement",
                )?
                .ok_or_else(|| anyhow!("current collision retirement disappeared"))?;
            total_encoded_bytes = checked_inventory_bytes(total_encoded_bytes, bytes.len())?;
            let record = decode_collision_retirement_pending_for_migration(&bytes)?;
            if record.project_id != project_id {
                bail!("current collision retirement path and project disagree");
            }
            collision_pending.push(MigrationCurrentCollisionEvidenceV1 {
                project_id,
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
        }
        directory.ensure_still_current()?;
    }
    let current_root_generation_ids = activations
        .iter()
        .map(|row| row.record.generation_id.as_str())
        .chain(
            collision_pending
                .iter()
                .map(|row| row.record.generation_id.as_str()),
        )
        .collect::<BTreeSet<_>>();

    let scopes_path = paths.root().join("scopes");
    let mut generations = Vec::new();
    if NofollowDirectory::open_existing(&scopes_path)?.is_some() {
        for scope_name in sorted_directory_entry_names(
            &scopes_path,
            MAX_MIGRATION_INVENTORY_GENERATIONS,
            "current scope",
        )? {
            validate_sha256(&scope_name)?;
            let scope_path = scopes_path.join(&scope_name);
            let scope_entries = sorted_directory_entry_names(&scope_path, 1, "current scope")?;
            if scope_entries.len() != 1 || scope_entries[0] != "generations" {
                bail!("current scope directory has an incomplete or unexpected row set");
            }
            let generations_path = scope_path.join("generations");
            if NofollowDirectory::open_existing(&generations_path)?.is_none() {
                continue;
            }
            let mut retained_candidates = Vec::new();
            for generation_entry in fs::read_dir(&generations_path)? {
                let generation_entry = generation_entry?;
                let file_type = generation_entry.file_type()?;
                if !file_type.is_dir() || file_type.is_symlink() {
                    bail!("current generation directory contains an unexpected entry type");
                }
                let generation_id = generation_entry
                    .file_name()
                    .to_str()
                    .map(str::to_string)
                    .ok_or_else(|| {
                        anyhow!("current generation directory contains a non-utf8 entry")
                    })?;
                validate_sha256(&generation_id)?;
                let generation_path = generations_path.join(&generation_id);
                let directory = NofollowDirectory::open_existing(&generation_path)?
                    .ok_or_else(|| anyhow!("current generation directory disappeared"))?;
                let entries =
                    sorted_regular_entry_names(&generation_path, 2, "current generation")?;
                if entries.len() != 2
                    || entries[0] != "manifest.jsonl"
                    || entries[1] != "metadata.json"
                {
                    bail!("current generation directory has an incomplete or unexpected row set");
                }
                let metadata_bytes = directory
                    .read_regular(
                        "metadata.json",
                        MAX_STORED_GENERATION_RECORD_BYTES,
                        "current generation metadata",
                    )?
                    .ok_or_else(|| anyhow!("current generation metadata is missing"))?;
                let manifest_bytes = directory
                    .read_regular(
                        "manifest.jsonl",
                        MAX_MIGRATION_RECORD_BYTES,
                        "current generation manifest",
                    )?
                    .ok_or_else(|| anyhow!("current generation manifest is missing"))?;
                match decode_stored_generation_v2_for_migration(&metadata_bytes) {
                    Ok(record) => {
                        if record.generation_id != generation_id
                            || scope_hash(&record.published_scope) != scope_name
                        {
                            bail!("current generation path and metadata disagree");
                        }
                        verify_generation_manifest_for_migration(
                            &manifest_bytes,
                            &record.descriptor,
                            &record.producer_id,
                            &record.generation_id,
                            limits,
                        )?;
                        if record.state == GenerationState::Superseded {
                            retained_candidates.push((
                                record.ordinal,
                                generation_id.clone(),
                                false,
                            ));
                        }
                        if generations.len() >= MAX_MIGRATION_INVENTORY_GENERATIONS {
                            bail!("current v2 generation inventory exceeds its row limit");
                        }
                        total_encoded_bytes = checked_inventory_bytes(
                            checked_inventory_bytes(total_encoded_bytes, metadata_bytes.len())?,
                            manifest_bytes.len(),
                        )?;
                        generations.push(MigrationCurrentGenerationEvidenceV1 {
                            published_scope: record.published_scope.clone(),
                            generation_id,
                            metadata_sha256: sha256_hex(&metadata_bytes),
                            metadata_bytes,
                            manifest_sha256: sha256_hex(&manifest_bytes),
                            manifest_bytes,
                            record,
                        });
                    }
                    Err(v2_error) => {
                        let record = decode_stored_generation_v1_for_migration(&metadata_bytes)
                            .map_err(|_| v2_error)?;
                        if record.generation_id != generation_id
                            || scope_hash(&record.descriptor.scope) != scope_name
                        {
                            bail!("legacy leftover generation path and metadata disagree");
                        }
                        verify_generation_manifest_for_migration(
                            &manifest_bytes,
                            &record.descriptor,
                            &record.producer_id,
                            &record.generation_id,
                            limits,
                        )?;
                        if current_root_generation_ids.contains(generation_id.as_str())
                            || matches!(
                                record.state,
                                GenerationState::MissingBlobs
                                    | GenerationState::Ready
                                    | GenerationState::StagingIndex
                                    | GenerationState::Active
                                    | GenerationState::MissingBlobData
                            )
                        {
                            bail!("protected current generation retains scopeless legacy metadata");
                        }
                        if record.state == GenerationState::Superseded {
                            retained_candidates.push((record.ordinal, generation_id, true));
                        }
                    }
                }
                directory.ensure_still_current()?;
            }
            retained_candidates
                .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
            if retained_candidates
                .iter()
                .take(limits.retained_generations)
                .any(|candidate| candidate.2)
            {
                bail!("protected retained generation keeps scopeless legacy metadata");
            }
        }
    }
    generations.sort_by(|left, right| {
        left.published_scope
            .cmp(&right.published_scope)
            .then_with(|| left.generation_id.cmp(&right.generation_id))
    });
    let generations_by_id = generations
        .iter()
        .map(|row| (row.generation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let scope_ordinals = generations
        .iter()
        .map(|row| (scope_hash(&row.published_scope), row.record.ordinal))
        .collect::<BTreeSet<_>>();
    if generations_by_id.len() != generations.len() || scope_ordinals.len() != generations.len() {
        bail!("current generation inventory contains duplicate ids or scope ordinals");
    }

    for pending in &collision_pending {
        let generation = generations_by_id
            .get(pending.record.generation_id.as_str())
            .ok_or_else(|| anyhow!("current collision retirement lacks generation metadata"))?;
        if pending.record.former_scope != generation.published_scope
            || pending.record.manifest_sha256 != generation.record.descriptor.manifest_sha256
        {
            bail!("current collision retirement rewrites generation evidence");
        }
    }

    let retirement_path = paths.root().join("retirements");
    let mut retirements = Vec::new();
    if let Some(directory) = NofollowDirectory::open_existing(&retirement_path)? {
        for name in sorted_regular_entry_names(
            &retirement_path,
            MAX_MIGRATION_INVENTORY_RETIREMENTS,
            "current retirement",
        )? {
            let selector_sha256 = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("current retirement filename is not canonical"))?
                .to_string();
            validate_sha256(&selector_sha256)?;
            let bytes = directory
                .read_regular(&name, MAX_MIGRATION_RECORD_BYTES, "current retirement")?
                .ok_or_else(|| anyhow!("current retirement disappeared"))?;
            total_encoded_bytes = checked_inventory_bytes(total_encoded_bytes, bytes.len())?;
            let record: RetirementRecord =
                decode_bounded_json(&bytes, MAX_MIGRATION_RECORD_BYTES, "current retirement")?;
            validate_retirement_record(&record)?;
            if sha256_hex(record.selector.as_bytes()) != selector_sha256 {
                bail!("current retirement path and selector disagree");
            }
            retirements.push(MigrationCurrentRetirementEvidenceV1 {
                selector_sha256,
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
        }
        directory.ensure_still_current()?;
    }

    let activations_by_project = activations
        .iter()
        .map(|row| (&row.project_id, row))
        .collect::<BTreeMap<_, _>>();
    if activations_by_project.len() != activations.len()
        || effective_manifest.selections.len() != activations.len()
    {
        bail!("current effective source and activation sets differ");
    }
    let selected_projects = effective_manifest
        .selections
        .iter()
        .map(|selection| selection.project_id.clone())
        .collect::<BTreeSet<_>>();
    for selection in &effective_manifest.selections {
        let activation = activations_by_project
            .get(&selection.project_id)
            .ok_or_else(|| anyhow!("current effective selection lacks activation"))?;
        let generation = generations_by_id
            .get(selection.generation_id.as_str())
            .ok_or_else(|| anyhow!("current effective selection lacks generation"))?;
        activation
            .record
            .validate_against_generation(&generation.record)?;
        if activation.record.published_scope != selection.published_scope
            || activation.record.generation_id != selection.generation_id
            || activation.record.selector != selection.selector
        {
            bail!("current effective selection rewrites activation evidence");
        }
    }
    for pending in &collision_pending {
        if selected_projects.contains(&pending.project_id)
            || activations_by_project.contains_key(&pending.project_id)
        {
            bail!("current collision retirement remains active or effective");
        }
    }

    let mut inventory = MigrationCurrentInventoryV1 {
        effective_manifest_sha256: sha256_hex(&effective_manifest_bytes),
        effective_manifest_bytes,
        effective_manifest,
        activations,
        generations,
        collision_pending,
        retirements,
        canonical_sha256: String::new(),
    };
    inventory.canonical_sha256 = current_inventory_digest(&inventory);
    Ok(inventory)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationManifestEvidence {
    pub generation_id: String,
    pub manifest_sha256: String,
    pub raw_manifest_sha256: String,
    pub file_count: u64,
    pub logical_bytes: u64,
}

pub fn decode_activation_v1_for_migration(bytes: &[u8]) -> Result<ActivationRecord> {
    let record = decode_bounded_json(bytes, MAX_MIGRATION_RECORD_BYTES, "activation record v1")?;
    validate_activation_v1(&record)?;
    Ok(record)
}

pub fn decode_stored_generation_v1_for_migration(bytes: &[u8]) -> Result<StoredGeneration> {
    let record = decode_bounded_json(
        bytes,
        MAX_STORED_GENERATION_RECORD_BYTES,
        "stored generation v1",
    )?;
    validate_stored_generation_v1(&record)?;
    Ok(record)
}

pub fn encode_activation_v2_for_migration(record: &ActivationRecordV2) -> Result<Vec<u8>> {
    record.validate()?;
    encode_bounded_json(record, MAX_MIGRATION_RECORD_BYTES, "activation record v2")
}

pub fn decode_activation_v2_for_migration(bytes: &[u8]) -> Result<ActivationRecordV2> {
    let record: ActivationRecordV2 =
        decode_bounded_json(bytes, MAX_MIGRATION_RECORD_BYTES, "activation record v2")?;
    record.validate()?;
    Ok(record)
}

pub fn encode_stored_generation_v2_for_migration(record: &StoredGenerationV2) -> Result<Vec<u8>> {
    record.validate()?;
    encode_bounded_json(
        record,
        MAX_STORED_GENERATION_RECORD_BYTES,
        "stored generation v2",
    )
}

pub fn decode_stored_generation_v2_for_migration(bytes: &[u8]) -> Result<StoredGenerationV2> {
    let record: StoredGenerationV2 = decode_bounded_json(
        bytes,
        MAX_STORED_GENERATION_RECORD_BYTES,
        "stored generation v2",
    )?;
    record.validate()?;
    Ok(record)
}

pub fn encode_collision_retirement_pending_for_migration(
    record: &CollisionRetirementPendingV1,
) -> Result<Vec<u8>> {
    record.validate()?;
    encode_bounded_json(
        record,
        MAX_COLLISION_RETIREMENT_RECORD_BYTES,
        "collision retirement pending",
    )
}

pub fn decode_collision_retirement_pending_for_migration(
    bytes: &[u8],
) -> Result<CollisionRetirementPendingV1> {
    let record: CollisionRetirementPendingV1 = decode_bounded_json(
        bytes,
        MAX_COLLISION_RETIREMENT_RECORD_BYTES,
        "collision retirement pending",
    )?;
    record.validate()?;
    Ok(record)
}

pub fn verify_generation_manifest_for_migration(
    manifest_bytes: &[u8],
    descriptor: &GenerationDescriptor,
    producer_id: &str,
    expected_generation_id: &str,
    limits: &StoreLimits,
) -> Result<GenerationManifestEvidence> {
    if manifest_bytes.len() > MAX_MIGRATION_RECORD_BYTES {
        bail!("generation manifest exceeds the migration byte limit");
    }
    validate_producer_id(producer_id)?;
    validate_sha256(expected_generation_id)?;
    descriptor.validate_header()?;
    let expected = generation_id(producer_id, descriptor);
    if expected != expected_generation_id {
        bail!("generation manifest identity does not match descriptor");
    }
    let entries = decode_manifest_jsonl_for_migration(manifest_bytes, limits.max_manifest_files)?;
    descriptor.validate_manifest(
        &entries,
        limits.max_manifest_files,
        limits.max_manifest_logical_bytes,
    )?;
    Ok(GenerationManifestEvidence {
        generation_id: expected,
        manifest_sha256: descriptor.manifest_sha256.clone(),
        raw_manifest_sha256: sha256_hex(manifest_bytes),
        file_count: descriptor.file_count,
        logical_bytes: descriptor.logical_bytes,
    })
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
#[serde(deny_unknown_fields)]
pub struct RetirementRecord {
    pub version: u32,
    pub project_id: String,
    pub selector: String,
    pub snapshot_id: String,
    pub generation_id: Option<String>,
}

impl CodeSourceStore {
    pub fn open(root: impl Into<PathBuf>, limits: StoreLimits) -> Result<Self> {
        let paths = CodeSourceStorePaths::new(root)?;
        let _anchor = acquire_store_lock_nofollow(&paths.anchor())?;
        create_private_dir(paths.root())?;
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
            create_private_dir(&paths.root().join(relative))?;
        }
        let root = paths.root().canonicalize().with_context(|| {
            format!(
                "canonicalizing code-source store {}",
                paths.root().display()
            )
        })?;
        let paths = CodeSourceStorePaths::new(root)?;
        let mut registry = STORE_REGISTRY
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| anyhow!("code-source store registry lock poisoned"))?;
        registry.retain(|_, state| state.strong_count() > 0);
        let shared = registry
            .get(paths.root())
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let shared = Arc::new(SharedStoreState {
                    limits: RwLock::new(limits),
                    mutation: Mutex::new(()),
                    verified_blobs: Mutex::new(HashMap::new()),
                    #[cfg(test)]
                    blob_verifications: AtomicU64::new(0),
                });
                registry.insert(paths.root().to_path_buf(), Arc::downgrade(&shared));
                shared
            });
        Ok(Self { paths, shared })
    }

    pub fn root(&self) -> &Path {
        self.paths.root()
    }

    fn lock_mutation(&self) -> Result<StoreMutationGuard<'_>> {
        let in_process = self
            .shared
            .mutation
            .lock()
            .map_err(|_| anyhow!("code-source store lock poisoned"))?;
        let anchor = acquire_store_lock_nofollow(&self.paths.anchor())?;
        Ok(StoreMutationGuard {
            _anchor: anchor,
            _in_process: in_process,
        })
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
        let _guard = self.lock_mutation()?;
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
        let _guard = self.lock_mutation()?;
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
        let _guard = self.lock_mutation()?;
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
        let generation_dir = self.generation_dir(&record.descriptor.scope, &generation)?;
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
            let stored = read_stored_generation_v1(&metadata_path)?;
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
        let _guard = self.lock_mutation()?;
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
        let _guard = self.lock_mutation()?;
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
        let _guard = self.lock_mutation()?;
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
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(&record.descriptor.scope)));
        let previous_desired = if desired_path.is_file() {
            Some(read_stored_generation_v1(&desired_path)?)
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
        self.save_generation_locked(&stored)?;
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
                self.save_generation_locked(&previous)?;
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
        for scope_entry in fs::read_dir(self.root().join("scopes"))? {
            let scope_entry = scope_entry?;
            let metadata = scope_entry
                .path()
                .join("generations")
                .join(generation)
                .join("metadata.json");
            if !metadata.is_file() {
                continue;
            }
            let stored = read_stored_generation_v1(&metadata)?;
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
        for scope_entry in fs::read_dir(self.root().join("scopes"))? {
            let metadata = scope_entry?
                .path()
                .join("generations")
                .join(generation)
                .join("metadata.json");
            if metadata.is_file() {
                return read_stored_generation_v1(&metadata);
            }
        }
        bail!("generation not found")
    }

    pub fn load_generation(
        &self,
        scope: &PublishedScope,
        generation: &str,
    ) -> Result<StoredGeneration> {
        read_stored_generation_v1(&self.paths.generation_metadata(scope, generation)?)
    }

    pub fn save_generation(&self, generation: &StoredGeneration) -> Result<()> {
        let _guard = self.lock_mutation()?;
        self.save_generation_locked(generation)
    }

    fn save_generation_locked(&self, generation: &StoredGeneration) -> Result<()> {
        validate_stored_generation_v1(generation)?;
        atomic_write_json(
            &self
                .paths
                .generation_metadata(&generation.descriptor.scope, &generation.generation_id)?,
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
        let _guard = self.lock_mutation()?;
        let mut stored = self.load_generation(scope, generation)?;
        stored.state = state;
        stored.diagnostic = diagnostic.map(|value| value.chars().take(512).collect());
        self.save_generation_locked(&stored)?;
        let desired_path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if desired_path.is_file() {
            let desired = read_stored_generation_v1(&desired_path)?;
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
        let _guard = self.lock_mutation()?;
        let mut stored = self.load_generation(scope, generation)?;
        stored.materialized_doc_count = Some(document_count);
        stored.entity_inventory_sha256 = Some(entity_inventory_sha256);
        self.save_generation_locked(&stored)?;
        Ok(stored)
    }

    pub fn save_activation(&self, activation: &ActivationRecord) -> Result<()> {
        let _guard = self.lock_mutation()?;
        self.save_activation_locked(activation)
    }

    fn save_activation_locked(&self, activation: &ActivationRecord) -> Result<()> {
        validate_activation_v1(activation)?;
        atomic_write_json(
            &self.paths.activation_for_str(&activation.project_id)?,
            activation,
        )
    }

    pub fn load_activation(&self, project_id: &str) -> Result<Option<ActivationRecord>> {
        let path = self.paths.activation_for_str(project_id)?;
        if !path.is_file() {
            return Ok(None);
        }
        let record = read_activation_v1(&path)?;
        if record.project_id != project_id {
            bail!("activation record identity mismatch");
        }
        Ok(Some(record))
    }

    pub fn activation_records(&self) -> Result<Vec<ActivationRecord>> {
        let mut records: Vec<ActivationRecord> = Vec::new();
        for entry in fs::read_dir(self.root().join("activations"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                records.push(read_activation_v1(&entry.path())?);
            }
        }
        records.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(records)
    }

    pub fn mark_cutback_pending(&self, project_id: &str, diagnostic: &str) -> Result<()> {
        let _guard = self.lock_mutation()?;
        let Some(mut record) = self.load_activation(project_id)? else {
            return Ok(());
        };
        record.cutback_pending = true;
        record.diagnostic = Some(diagnostic.chars().take(512).collect());
        self.save_activation_locked(&record)
    }

    pub fn clear_activation(&self, project_id: &str) -> Result<()> {
        let _guard = self.lock_mutation()?;
        let path = self.paths.activation_for_str(project_id)?;
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
        let _guard = self.lock_mutation()?;
        self.record_health_failure_locked(project_id, code, diagnostic)
    }

    fn record_health_failure_locked(
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
        let _guard = self.lock_mutation()?;
        let path = self.health_path(project_id, code);
        match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn health_records(&self) -> Result<Vec<CodeSourceHealthRecord>> {
        let mut records: Vec<CodeSourceHealthRecord> = Vec::new();
        for entry in fs::read_dir(self.root().join("health"))? {
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
        let _guard = self.lock_mutation()?;
        validate_retirement_record(record)?;
        atomic_write_json(
            &self.paths.retirement_for_selector(&record.selector)?,
            record,
        )
    }

    pub fn retirement_records(&self) -> Result<Vec<RetirementRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root().join("retirements"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                records.push(read_json(&entry.path())?);
            }
        }
        Ok(records)
    }

    pub fn complete_retirement(&self, selector: &str) -> Result<()> {
        let _guard = self.lock_mutation()?;
        let path = self.paths.retirement_for_selector(selector)?;
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
        read_manifest_jsonl(&self.paths.generation_manifest(scope, generation)?)
    }

    pub fn desired_generation(&self, scope: &PublishedScope) -> Result<Option<StoredGeneration>> {
        let path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(read_stored_generation_v1(&path)?))
    }

    pub fn expire_uploads(&self, max_idle_secs: u64) -> Result<u64> {
        let _guard = self.lock_mutation()?;
        let cutoff = now_unix_secs().saturating_sub(max_idle_secs);
        let mut expired = 0_u64;
        for producer in fs::read_dir(self.root().join("uploads"))? {
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
        let _guard = self.lock_mutation()?;
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
                self.save_generation_locked(&generation)?;
                self.update_desired_if_same(&generation)?;
                self.record_health_failure_locked(
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
        let _guard = self.lock_mutation()?;
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
        for producer in fs::read_dir(self.root().join("uploads"))? {
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
        for prefix in fs::read_dir(self.root().join("blobs/sha256"))? {
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
        self.root().join("blobs/sha256").join(&hash[..2]).join(hash)
    }

    fn list_generations(&self) -> Result<Vec<StoredGeneration>> {
        let mut generations = Vec::new();
        for scope in fs::read_dir(self.root().join("scopes"))? {
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
                generations.push(read_stored_generation_v1(
                    &generation.path().join("metadata.json"),
                )?);
            }
        }
        Ok(generations)
    }

    fn protected_generation_ids(
        &self,
        generations: &[StoredGeneration],
        retained_generations: usize,
    ) -> Result<BTreeSet<String>> {
        let mut activations = Vec::new();
        for activation in fs::read_dir(self.root().join("activations"))? {
            let activation = activation?;
            if activation.file_type()?.is_file() {
                activations.push(read_activation_v1(&activation.path())?);
            }
        }
        protected_generation_ids_from_records(
            generations,
            &activations,
            &self.collision_retirement_pending_records_for_gc()?,
            retained_generations,
        )
    }

    fn collision_retirement_pending_records_for_gc(
        &self,
    ) -> Result<Vec<CollisionRetirementPendingV1>> {
        let directory = self.root().join("collision-retirements");
        let Some(held_directory) = NofollowDirectory::open_existing(&directory)? else {
            return Ok(Vec::new());
        };
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                bail!("collision retirement directory contains a non-utf8 entry");
            };
            if Path::new(&name)
                .extension()
                .and_then(|value| value.to_str())
                != Some("json")
            {
                bail!("collision retirement directory contains an unexpected entry");
            }
            let bytes = held_directory
                .read_regular(
                    &name,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "collision retirement pending",
                )?
                .ok_or_else(|| anyhow!("collision retirement pending disappeared"))?;
            let record = decode_collision_retirement_pending_for_migration(&bytes)?;
            let expected_path = self.paths.collision_retirement_pending(&record.project_id);
            let expected_name = expected_path
                .file_name()
                .and_then(|value| value.to_str())
                .expect("code-owned collision retirement path has a utf8 filename");
            if name != expected_name {
                bail!("collision retirement pending path does not match project id");
            }
            records.push(record);
        }
        held_directory.ensure_still_current()?;
        records.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        Ok(records)
    }

    fn update_desired_if_same(&self, generation: &StoredGeneration) -> Result<()> {
        let desired_path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(&generation.descriptor.scope)));
        if desired_path.is_file()
            && read_stored_generation_v1(&desired_path)?.generation_id == generation.generation_id
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
        self.root()
            .join("health")
            .join(format!("{}.json", hex::encode(hasher.finalize())))
    }

    fn activation_project_for_generation(&self, generation_id: &str) -> Result<Option<String>> {
        for entry in fs::read_dir(self.root().join("activations"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let activation = read_activation_v1(&entry.path())?;
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
            .root()
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
        self.root().join("uploads").join(producer_hash(producer_id))
    }

    fn upload_dir(&self, producer_id: &str, upload_id: &str) -> PathBuf {
        self.upload_producer_dir(producer_id).join(upload_id)
    }

    fn generation_dir(&self, scope: &PublishedScope, generation: &str) -> Result<PathBuf> {
        let manifest = self.paths.generation_manifest(scope, generation)?;
        Ok(manifest
            .parent()
            .expect("generation manifest path has a parent")
            .to_path_buf())
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

fn validate_store_root(root: &Path) -> Result<()> {
    use std::path::Component;

    if !root.is_absolute()
        || root.file_name().is_none()
        || root
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        bail!("code-source store root must be an absolute normalized path");
    }
    Ok(())
}

fn validate_retirement_selector(selector: &str) -> Result<()> {
    if selector.trim().is_empty()
        || selector.len() > MAX_RETIREMENT_SELECTOR_BYTES
        || selector.chars().any(char::is_control)
    {
        bail!("invalid code-source retirement selector");
    }
    Ok(())
}

fn validate_snapshot_id(snapshot_id: &str) -> Result<()> {
    if snapshot_id.trim().is_empty()
        || snapshot_id.len() > MAX_SNAPSHOT_ID_BYTES
        || snapshot_id.chars().any(char::is_control)
        || snapshot_id.contains('/')
        || snapshot_id.contains('\\')
        || matches!(snapshot_id, "." | "..")
    {
        bail!("invalid code-source snapshot id");
    }
    Ok(())
}

fn validate_migration_snapshot_id(snapshot_id: &str) -> Result<()> {
    validate_snapshot_id(snapshot_id)?;
    let Some(hash) = snapshot_id.strip_prefix("collected-") else {
        bail!("invalid collected snapshot id");
    };
    if hash.len() != 32
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid collected snapshot id");
    }
    Ok(())
}

fn validate_optional_diagnostic(diagnostic: Option<&str>) -> Result<()> {
    if diagnostic.is_some_and(|value| value.chars().count() > MAX_DIAGNOSTIC_CHARS) {
        bail!("invalid code-source diagnostic");
    }
    Ok(())
}

fn validate_stored_generation_v1(record: &StoredGeneration) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!("legacy code-source API refuses non-v1 stored generation");
    }
    validate_sha256(&record.generation_id)?;
    validate_producer_id(&record.producer_id)?;
    record.descriptor.validate_header()?;
    if generation_id(&record.producer_id, &record.descriptor) != record.generation_id {
        bail!("stored generation identity does not match descriptor");
    }
    validate_optional_diagnostic(record.diagnostic.as_deref())?;
    match (
        record.materialized_doc_count,
        record.entity_inventory_sha256.as_deref(),
    ) {
        (Some(_), Some(hash)) => validate_sha256(hash)?,
        (None, None) => {}
        _ => bail!("stored generation materialization evidence is incomplete"),
    }
    Ok(())
}

fn validate_activation_v1(record: &ActivationRecord) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!("legacy code-source API refuses non-v1 activation record");
    }
    let project_id = ProjectId::parse(record.project_id.clone()).map_err(|error| anyhow!(error))?;
    validate_sha256(&record.generation_id)?;
    validate_retirement_selector(&record.selector)?;
    validate_collected_materialization_selector(
        project_id.as_str(),
        &record.generation_id,
        &record.selector,
    )?;
    validate_snapshot_id(&record.snapshot_id)?;
    validate_sha256(&record.entity_inventory_sha256)?;
    validate_optional_diagnostic(record.diagnostic.as_deref())?;
    if record.current_chunk_targets.len() > DEFAULT_MAX_MANIFEST_FILES as usize {
        bail!("activation record has too many chunk targets");
    }
    for (key, target) in &record.current_chunk_targets {
        if key.trim().is_empty()
            || key.len() > MAX_CHUNK_TARGET_KEY_BYTES
            || key.chars().any(char::is_control)
        {
            bail!("activation record has an invalid chunk target key");
        }
        target
            .try_render()
            .map_err(|error| anyhow!("activation record has an invalid entity ref: {error}"))?;
    }
    Ok(())
}

fn decode_bounded_json<T: for<'de> Deserialize<'de>>(
    bytes: &[u8],
    max_bytes: usize,
    label: &str,
) -> Result<T> {
    if bytes.len() > max_bytes {
        bail!("{label} exceeds the encoded byte limit");
    }
    serde_json::from_slice(bytes).with_context(|| format!("parsing {label}"))
}

fn encode_bounded_json(value: &impl Serialize, max_bytes: usize, label: &str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(value).with_context(|| format!("encoding {label}"))?;
    if bytes.len() > max_bytes {
        bail!("{label} exceeds the encoded byte limit");
    }
    Ok(bytes)
}

fn decode_manifest_jsonl_for_migration(
    bytes: &[u8],
    max_manifest_files: u64,
) -> Result<Vec<ManifestEntry>> {
    let mut entries = Vec::new();
    let records = if bytes.last() == Some(&b'\n') {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    if records.is_empty() {
        if bytes.is_empty() {
            return Ok(entries);
        }
        bail!("generation manifest contains an empty record");
    }
    for (index, line) in records.split(|byte| *byte == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            bail!("generation manifest contains an empty record");
        }
        let entry = serde_json::from_slice(line)
            .with_context(|| format!("parsing generation manifest record {}", index + 1))?;
        entries.push(entry);
        if entries.len() as u64 > max_manifest_files {
            bail!("generation manifest exceeds the file count limit");
        }
    }
    Ok(entries)
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

fn read_optional_regular_nofollow(
    path: &Path,
    max_bytes: usize,
    label: &str,
) -> Result<Option<Vec<u8>>> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{label} path has no parent"))?;
    let Some(directory) = NofollowDirectory::open_existing(parent)? else {
        return Ok(None);
    };
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("{label} filename is not utf8"))?;
    let bytes = directory.read_regular(name, max_bytes, label)?;
    directory.ensure_still_current()?;
    Ok(bytes)
}

fn sorted_regular_entry_names(path: &Path, max_rows: usize, label: &str) -> Result<Vec<String>> {
    sorted_entry_names(path, max_rows, label, true)
}

fn sorted_directory_entry_names(path: &Path, max_rows: usize, label: &str) -> Result<Vec<String>> {
    sorted_entry_names(path, max_rows, label, false)
}

fn sorted_entry_names(
    path: &Path,
    max_rows: usize,
    label: &str,
    regular: bool,
) -> Result<Vec<String>> {
    let Some(directory) = NofollowDirectory::open_existing(path)? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
        let file_type = entry.file_type()?;
        let expected_type = if regular {
            file_type.is_file() && !file_type.is_symlink()
        } else {
            file_type.is_dir() && !file_type.is_symlink()
        };
        if !expected_type {
            bail!("{label} directory contains an unexpected entry type");
        }
        if names.len() >= max_rows {
            bail!("{label} directory exceeds its row limit");
        }
        names.push(name);
    }
    names.sort();
    directory.ensure_still_current()?;
    Ok(names)
}

fn checked_inventory_bytes(current: usize, added: usize) -> Result<usize> {
    let total = current
        .checked_add(added)
        .ok_or_else(|| anyhow!("migration inventory byte count overflowed"))?;
    if total > MAX_MIGRATION_INVENTORY_MANIFEST_BYTES {
        bail!("migration inventory exceeds its aggregate byte limit");
    }
    Ok(total)
}

fn validate_retirement_record(record: &RetirementRecord) -> Result<()> {
    if record.version != STORE_VERSION {
        bail!("invalid code-source retirement version");
    }
    let project_id = ProjectId::parse(record.project_id.clone()).map_err(|error| anyhow!(error))?;
    validate_retirement_selector(&record.selector)?;
    validate_migration_snapshot_id(&record.snapshot_id)?;
    if let Some(generation_id) = &record.generation_id {
        validate_sha256(generation_id)?;
        validate_collected_materialization_selector(
            project_id.as_str(),
            generation_id,
            &record.selector,
        )?;
    }
    Ok(())
}

fn protected_generation_ids_from_records(
    generations: &[StoredGeneration],
    activations: &[ActivationRecord],
    collision_pending: &[CollisionRetirementPendingV1],
    retained_generations: usize,
) -> Result<BTreeSet<String>> {
    let mut generations_by_id = BTreeMap::new();
    let mut ordinals_by_scope = BTreeSet::new();
    let mut by_scope = BTreeMap::<String, Vec<&StoredGeneration>>::new();
    for generation in generations {
        validate_stored_generation_v1(generation)?;
        let scope = scope_hash(&generation.descriptor.scope);
        if generations_by_id
            .insert(generation.generation_id.as_str(), generation)
            .is_some()
        {
            bail!("generation inventory contains a duplicate generation id");
        }
        if !ordinals_by_scope.insert((scope.clone(), generation.ordinal)) {
            bail!("generation inventory contains a duplicate scope ordinal");
        }
        by_scope.entry(scope).or_default().push(generation);
    }

    let mut protected = BTreeSet::new();
    let mut activation_projects = BTreeSet::new();
    for activation in activations {
        validate_activation_v1(activation)?;
        if !activation_projects.insert(activation.project_id.as_str()) {
            bail!("activation inventory contains a duplicate project");
        }
        let generation = generations_by_id
            .get(activation.generation_id.as_str())
            .ok_or_else(|| anyhow!("activation references missing generation metadata"))?;
        if Some(activation.document_count) != generation.materialized_doc_count
            || Some(activation.entity_inventory_sha256.as_str())
                != generation.entity_inventory_sha256.as_deref()
        {
            bail!("activation materialization evidence does not match generation metadata");
        }
        protected.insert(activation.generation_id.clone());
    }
    let mut pending_projects = BTreeSet::new();
    for pending in collision_pending {
        pending.validate()?;
        if !pending_projects.insert(pending.project_id.clone()) {
            bail!("collision retirement inventory contains a duplicate project");
        }
        let generation = generations_by_id
            .get(pending.generation_id.as_str())
            .ok_or_else(|| {
                anyhow!("collision retirement pending references missing generation metadata")
            })?;
        if pending.former_scope != generation.descriptor.scope
            || pending.manifest_sha256 != generation.descriptor.manifest_sha256
        {
            bail!("collision retirement pending does not match generation metadata");
        }
        protected.insert(pending.generation_id.clone());
    }
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
    }
    for scope_generations in by_scope.values_mut() {
        scope_generations.sort_by(|left, right| {
            right
                .ordinal
                .cmp(&left.ordinal)
                .then_with(|| left.generation_id.cmp(&right.generation_id))
        });
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

fn legacy_inventory_digest(inventory: &MigrationLegacyInventoryV1) -> String {
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    fn text(hasher: &mut Sha256, value: &str) {
        field(hasher, value.as_bytes());
    }

    let mut hasher = Sha256::new();
    field(&mut hasher, b"bbox-code-source-legacy-inventory-v1");
    match &inventory.anchor {
        MigrationLegacyAnchorEvidenceV1::Missing => field(&mut hasher, b"anchor:missing"),
        MigrationLegacyAnchorEvidenceV1::Present { sha256, .. } => {
            field(&mut hasher, b"anchor:present");
            text(&mut hasher, sha256);
        }
    }
    field(
        &mut hasher,
        &(inventory.activations.len() as u64).to_be_bytes(),
    );
    for row in &inventory.activations {
        field(&mut hasher, b"activation");
        text(&mut hasher, row.project_id.as_str());
        text(&mut hasher, &row.sha256);
    }
    field(&mut hasher, &inventory.generation_count.to_be_bytes());
    text(&mut hasher, &inventory.generation_set_sha256);
    field(
        &mut hasher,
        &inventory.unprotected_generation_count.to_be_bytes(),
    );
    text(&mut hasher, &inventory.unprotected_generation_set_sha256);
    field(
        &mut hasher,
        &(inventory.generations.len() as u64).to_be_bytes(),
    );
    for row in &inventory.generations {
        field(&mut hasher, b"generation");
        text(&mut hasher, row.published_scope.repo_id());
        text(&mut hasher, row.published_scope.bbox_root_relpath());
        text(&mut hasher, &row.generation_id);
        text(&mut hasher, &row.metadata_sha256);
        text(&mut hasher, &row.manifest_sha256);
        text(&mut hasher, &row.record.descriptor.manifest_sha256);
    }
    field(
        &mut hasher,
        &(inventory.collision_pending.len() as u64).to_be_bytes(),
    );
    for row in &inventory.collision_pending {
        field(&mut hasher, b"collision-pending");
        text(&mut hasher, row.project_id.as_str());
        text(&mut hasher, &row.sha256);
    }
    field(
        &mut hasher,
        &(inventory.protected_generation_ids.len() as u64).to_be_bytes(),
    );
    for generation_id in &inventory.protected_generation_ids {
        field(&mut hasher, b"protected");
        text(&mut hasher, generation_id);
    }
    hex::encode(hasher.finalize())
}

fn current_inventory_digest(inventory: &MigrationCurrentInventoryV1) -> String {
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    fn text(hasher: &mut Sha256, value: &str) {
        field(hasher, value.as_bytes());
    }

    let mut hasher = Sha256::new();
    field(&mut hasher, b"bbox-code-source-current-inventory-v2");
    text(&mut hasher, &inventory.effective_manifest_sha256);
    for row in &inventory.activations {
        field(&mut hasher, b"activation");
        text(&mut hasher, row.project_id.as_str());
        text(&mut hasher, &row.sha256);
    }
    for row in &inventory.generations {
        field(&mut hasher, b"generation");
        text(&mut hasher, row.published_scope.repo_id());
        text(&mut hasher, row.published_scope.bbox_root_relpath());
        text(&mut hasher, &row.generation_id);
        text(&mut hasher, &row.metadata_sha256);
        text(&mut hasher, &row.manifest_sha256);
    }
    for row in &inventory.collision_pending {
        field(&mut hasher, b"collision-pending");
        text(&mut hasher, row.project_id.as_str());
        text(&mut hasher, &row.sha256);
    }
    for row in &inventory.retirements {
        field(&mut hasher, b"retirement");
        text(&mut hasher, &row.selector_sha256);
        text(&mut hasher, &row.sha256);
    }
    hex::encode(hasher.finalize())
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

fn read_stored_generation_v1(path: &Path) -> Result<StoredGeneration> {
    let record = read_json(path)?;
    validate_stored_generation_v1(&record)?;
    Ok(record)
}

fn read_activation_v1(path: &Path) -> Result<ActivationRecord> {
    let record = read_json(path)?;
    validate_activation_v1(&record)?;
    Ok(record)
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
            scope: PublishedScope::try_new("repo-family", ".").unwrap(),
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

    fn manifest_bytes(entries: &[ManifestEntry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }

    fn materialized_selector(project_id: &str, generation_id: &str) -> String {
        format!(
            "{}:m0123456789abcdef",
            source_selector(project_id, generation_id)
        )
    }

    #[test]
    fn legacy_inventory_accepts_a_missing_effective_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        fs::create_dir_all(&root).unwrap();
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard.snapshot_legacy_v1(&StoreLimits::default()).unwrap();

        assert!(matches!(
            inventory.anchor,
            MigrationLegacyAnchorEvidenceV1::Missing
        ));
        assert!(inventory.activations.is_empty());
        assert!(inventory.generations.is_empty());
        validate_sha256(&inventory.canonical_sha256).unwrap();
    }

    fn stored_generation_v1(
        producer_id: &str,
        descriptor: GenerationDescriptor,
    ) -> StoredGeneration {
        StoredGeneration {
            version: STORE_VERSION,
            generation_id: generation_id(producer_id, &descriptor),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor,
            state: GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 7,
            materialized_doc_count: Some(1),
            entity_inventory_sha256: Some("c".repeat(64)),
        }
    }

    fn write_legacy_generation_fixture(
        paths: &CodeSourceStorePaths,
        producer_id: &str,
        ordinal: u64,
        state: GenerationState,
    ) -> StoredGeneration {
        let entries = Vec::new();
        let descriptor = descriptor(&entries);
        let mut record = stored_generation_v1(producer_id, descriptor.clone());
        record.ordinal = ordinal;
        record.state = state;
        if !matches!(
            state,
            GenerationState::Ready
                | GenerationState::StagingIndex
                | GenerationState::Active
                | GenerationState::Superseded
                | GenerationState::MissingBlobData
        ) {
            record.materialized_doc_count = None;
            record.entity_inventory_sha256 = None;
        }
        let metadata = paths
            .generation_metadata(&descriptor.scope, &record.generation_id)
            .unwrap();
        fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        fs::write(&metadata, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        fs::write(
            paths
                .generation_manifest(&descriptor.scope, &record.generation_id)
                .unwrap(),
            manifest_bytes(&entries),
        )
        .unwrap();
        record
    }

    #[test]
    fn generation_set_evidence_is_order_independent_and_detects_set_changes() {
        let row = |generation_id: String| MigrationLegacyGenerationEvidenceV1 {
            published_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id,
            metadata_bytes: Vec::new(),
            metadata_sha256: "a".repeat(64),
            record: stored_generation_v1("host-a", descriptor(&[])),
            manifest_bytes: Vec::new(),
            manifest_sha256: "b".repeat(64),
        };
        let rows = vec![
            row("1".repeat(64)),
            row("2".repeat(64)),
            row("3".repeat(64)),
        ];
        let digest = |rows: &[MigrationLegacyGenerationEvidenceV1]| {
            let mut accumulator = MigrationGenerationSetAccumulator::default();
            for row in rows {
                accumulator.add(row).unwrap();
            }
            accumulator.digest(b"test-generation-set")
        };
        let expected = digest(&rows);
        let mut reordered = rows.clone();
        reordered.reverse();
        assert_eq!(digest(&reordered), expected);
        assert_ne!(digest(&rows[..2]), expected);
        let mut swapped = rows.clone();
        swapped[2] = row("4".repeat(64));
        assert_ne!(digest(&swapped), expected);
    }

    #[test]
    fn unprotected_history_can_exceed_the_survivor_row_cap() {
        let mut accumulator = MigrationGenerationSetAccumulator::default();
        let mut row = MigrationLegacyGenerationEvidenceV1 {
            published_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id: "0".repeat(64),
            metadata_bytes: Vec::new(),
            metadata_sha256: "a".repeat(64),
            record: stored_generation_v1("host-a", descriptor(&[])),
            manifest_bytes: Vec::new(),
            manifest_sha256: "b".repeat(64),
        };
        for index in 0..=MAX_MIGRATION_INVENTORY_GENERATIONS {
            row.generation_id = format!("{index:064x}");
            accumulator.add(&row).unwrap();
        }
        assert_eq!(
            accumulator.count,
            MAX_MIGRATION_INVENTORY_GENERATIONS as u64 + 1
        );
        validate_sha256(&accumulator.digest(b"test-unprotected-generation-set")).unwrap();
    }

    #[test]
    fn inventory_refuses_an_omitted_protected_survivor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let generation =
            write_legacy_generation_fixture(&paths, "host-protected", 1, GenerationState::Active);
        let guard = paths.lock_migration_inventory().unwrap();
        let mut inventory = guard.snapshot_legacy_v1(&StoreLimits::default()).unwrap();
        assert_eq!(
            inventory.protected_generation_ids,
            BTreeSet::from([generation.generation_id])
        );

        inventory.generations.clear();
        assert!(inventory.validate_evidence().is_err());
    }

    #[test]
    fn current_inventory_ignores_unprotected_v1_leftovers_but_refuses_protected_ones() {
        let effective =
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap();

        let unprotected_directory = tempfile::tempdir().unwrap();
        let unprotected_root = unprotected_directory
            .path()
            .canonicalize()
            .unwrap()
            .join("source");
        let unprotected_paths = CodeSourceStorePaths::new(unprotected_root).unwrap();
        fs::create_dir_all(unprotected_paths.root()).unwrap();
        fs::write(unprotected_paths.anchor(), &effective).unwrap();
        write_legacy_generation_fixture(
            &unprotected_paths,
            "host-unprotected",
            1,
            GenerationState::Failed,
        );
        let guard = unprotected_paths.lock_migration_inventory().unwrap();
        let first = guard.snapshot_current_v2(&StoreLimits::default()).unwrap();
        let second = guard.snapshot_current_v2(&StoreLimits::default()).unwrap();
        assert!(first.generations.is_empty());
        assert!(first.effective_manifest.selections.is_empty());
        assert_eq!(first.canonical_sha256, second.canonical_sha256);
        drop(guard);
        CodeSourceStore::open(
            unprotected_paths.root().to_path_buf(),
            StoreLimits::default(),
        )
        .unwrap()
        .gc_blobs()
        .unwrap();

        let protected_directory = tempfile::tempdir().unwrap();
        let protected_root = protected_directory
            .path()
            .canonicalize()
            .unwrap()
            .join("source");
        let protected_paths = CodeSourceStorePaths::new(protected_root).unwrap();
        fs::create_dir_all(protected_paths.root()).unwrap();
        fs::write(protected_paths.anchor(), effective).unwrap();
        write_legacy_generation_fixture(
            &protected_paths,
            "host-protected-current",
            1,
            GenerationState::Active,
        );
        let guard = protected_paths.lock_migration_inventory().unwrap();
        let error = guard
            .snapshot_current_v2(&StoreLimits::default())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("protected current generation retains scopeless legacy metadata")
        );
    }

    fn activation_v1(generation_id: &str) -> ActivationRecord {
        ActivationRecord {
            version: STORE_VERSION,
            project_id: "project-a".into(),
            generation_id: generation_id.to_string(),
            selector: materialized_selector("project-a", generation_id),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            document_count: 1,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 8,
            cutback_pending: false,
            diagnostic: None,
        }
    }

    #[test]
    fn migration_effective_source_manifest_codec_is_strict_and_canonical() {
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        let project_a = ProjectId::parse("project-a").unwrap();
        let project_b = ProjectId::parse("project-b").unwrap();
        let generation_a = "a".repeat(64);
        let generation_b = "b".repeat(64);
        let manifest = MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections: vec![
                MigrationEffectiveSourceSelectionV1 {
                    project_id: project_a.clone(),
                    published_scope: scope.clone(),
                    generation_id: generation_a.clone(),
                    selector: materialized_selector(project_a.as_str(), &generation_a),
                },
                MigrationEffectiveSourceSelectionV1 {
                    project_id: project_b.clone(),
                    published_scope: scope.clone(),
                    generation_id: generation_b.clone(),
                    selector: materialized_selector(project_b.as_str(), &generation_b),
                },
            ],
        };
        let bytes = encode_migration_effective_source_manifest_v1(&manifest).unwrap();
        assert_eq!(
            decode_migration_effective_source_manifest_v1(&bytes).unwrap(),
            manifest
        );

        let mut unsorted = manifest.clone();
        unsorted.selections.reverse();
        assert!(encode_migration_effective_source_manifest_v1(&unsorted).is_err());
        let unknown = serde_json::json!({
            "version": 1,
            "selections": [],
            "extra": true,
        });
        assert!(
            decode_migration_effective_source_manifest_v1(&serde_json::to_vec(&unknown).unwrap())
                .is_err()
        );
    }

    #[test]
    fn store_paths_derive_closed_layout_without_creating_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store_root = root.join("not-created");
        let paths = CodeSourceStorePaths::new(store_root.clone()).unwrap();
        let project_id = ProjectId::parse("project-a").unwrap();
        let scope = PublishedScope::try_new("repo-family", "services/api").unwrap();
        let generation_id = "a".repeat(64);
        let selector = materialized_selector(project_id.as_str(), &generation_id);
        let selector_hash = sha256_hex(selector.as_bytes());

        assert_eq!(paths.root(), store_root);
        assert_eq!(
            paths.anchor(),
            store_root.join("effective-source-manifest.json")
        );
        assert_eq!(
            paths.activation(&project_id),
            store_root.join("activations/project-a.json")
        );
        assert_eq!(
            paths.activation_for_str("project-a").unwrap(),
            paths.activation(&project_id)
        );
        assert_eq!(
            paths.generation_metadata(&scope, &generation_id).unwrap(),
            store_root
                .join("scopes")
                .join(scope_hash(&scope))
                .join("generations")
                .join(&generation_id)
                .join("metadata.json")
        );
        assert_eq!(
            paths.generation_manifest(&scope, &generation_id).unwrap(),
            store_root
                .join("scopes")
                .join(scope_hash(&scope))
                .join("generations")
                .join(&generation_id)
                .join("manifest.jsonl")
        );
        assert_eq!(
            paths.collision_retirement_pending(&project_id),
            store_root.join("collision-retirements/project-a.json")
        );
        assert_eq!(
            paths.retirement_for_selector(&selector).unwrap(),
            store_root
                .join("retirements")
                .join(format!("{selector_hash}.json"))
        );
        assert_eq!(
            paths.retirement_for_selector_hash(&selector_hash).unwrap(),
            paths.retirement_for_selector(&selector).unwrap()
        );
        assert!(!store_root.exists());
    }

    #[test]
    fn store_paths_reject_unsafe_roots_and_unvalidated_dynamic_keys() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(CodeSourceStorePaths::new(root.join("nested/../escape")).is_err());

        let paths = CodeSourceStorePaths::new(root.join("not-created")).unwrap();
        let scope = PublishedScope::try_new("repo-family", ".").unwrap();
        assert!(paths.activation_for_str("../escape").is_err());
        assert!(paths.generation_metadata(&scope, &"A".repeat(64)).is_err());
        assert!(paths.generation_manifest(&scope, &"A".repeat(64)).is_err());
        assert!(paths.retirement_for_selector("").is_err());
        assert!(paths.retirement_for_selector("local:\n").is_err());
        assert!(
            paths
                .retirement_for_selector(&"x".repeat(MAX_RETIREMENT_SELECTOR_BYTES + 1))
                .is_err()
        );
        assert!(paths.retirement_for_selector_hash(&"A".repeat(64)).is_err());
        assert!(!paths.root().exists());
    }

    #[test]
    fn migration_v2_codecs_are_strict_deterministic_and_scope_bound() {
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "a".repeat(64),
            size: 1,
        }];
        let descriptor = descriptor(&entries);
        let scope = descriptor.scope.clone();
        let legacy_generation = stored_generation_v1("host-a", descriptor);
        let generation =
            StoredGenerationV2::from_v1_for_migration(legacy_generation.clone(), scope).unwrap();
        let encoded_generation = encode_stored_generation_v2_for_migration(&generation).unwrap();
        assert_eq!(
            encoded_generation,
            encode_stored_generation_v2_for_migration(&generation).unwrap()
        );
        assert_eq!(
            decode_stored_generation_v2_for_migration(&encoded_generation).unwrap(),
            generation
        );

        let activation = ActivationRecordV2::from_v1_for_migration(
            activation_v1(&legacy_generation.generation_id),
            &generation,
        )
        .unwrap();
        activation.validate_against_generation(&generation).unwrap();
        let encoded_activation = encode_activation_v2_for_migration(&activation).unwrap();
        assert_eq!(
            decode_activation_v2_for_migration(&encoded_activation).unwrap(),
            activation
        );

        let mut missing_scope: serde_json::Value =
            serde_json::from_slice(&encoded_generation).unwrap();
        missing_scope
            .as_object_mut()
            .unwrap()
            .remove("published_scope");
        assert!(
            decode_stored_generation_v2_for_migration(&serde_json::to_vec(&missing_scope).unwrap())
                .is_err()
        );

        let mut unknown: serde_json::Value = serde_json::from_slice(&encoded_activation).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        assert!(
            decode_activation_v2_for_migration(&serde_json::to_vec(&unknown).unwrap()).is_err()
        );

        let mut mismatched = activation;
        mismatched.document_count += 1;
        assert!(mismatched.validate_against_generation(&generation).is_err());

        let mut wrong_scope = generation.clone();
        wrong_scope.published_scope = PublishedScope::try_new("another-repo-family", ".").unwrap();
        assert!(encode_stored_generation_v2_for_migration(&wrong_scope).is_err());
        let mut wrong_generation_id = generation;
        wrong_generation_id.generation_id = "f".repeat(64);
        assert!(encode_stored_generation_v2_for_migration(&wrong_generation_id).is_err());
    }

    #[test]
    fn migration_v1_conversion_preserves_fields_and_v1_api_refuses_v2() {
        let descriptor = descriptor(&[]);
        let legacy = stored_generation_v1("host-a", descriptor.clone());
        let legacy_bytes = serde_json::to_vec_pretty(&legacy).unwrap();
        let decoded = decode_stored_generation_v1_for_migration(&legacy_bytes).unwrap();
        let converted =
            StoredGenerationV2::from_v1_for_migration(decoded, descriptor.scope.clone()).unwrap();
        assert_eq!(converted.generation_id, legacy.generation_id);
        assert_eq!(converted.producer_id, legacy.producer_id);
        assert_eq!(converted.ordinal, legacy.ordinal);
        assert_eq!(converted.descriptor, legacy.descriptor);
        assert_eq!(converted.state, legacy.state);
        assert_eq!(converted.created_unix_secs, legacy.created_unix_secs);
        assert_eq!(
            converted.materialized_doc_count,
            legacy.materialized_doc_count
        );
        assert_eq!(
            converted.entity_inventory_sha256,
            legacy.entity_inventory_sha256
        );

        let mut wrong_version = legacy;
        wrong_version.version = MIGRATION_STORE_VERSION;
        assert!(
            decode_stored_generation_v1_for_migration(&serde_json::to_vec(&wrong_version).unwrap())
                .is_err()
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        assert!(store.save_generation(&wrong_version).is_err());
        let generation_path = store
            .paths
            .generation_metadata(&descriptor.scope, &converted.generation_id)
            .unwrap();
        atomic_write(
            &generation_path,
            &encode_stored_generation_v2_for_migration(&converted).unwrap(),
        )
        .unwrap();
        assert!(
            store
                .load_generation(&descriptor.scope, &converted.generation_id)
                .is_err()
        );

        let legacy_activation = activation_v1(&converted.generation_id);
        let v2_activation =
            ActivationRecordV2::from_v1_for_migration(legacy_activation.clone(), &converted)
                .unwrap();
        atomic_write(
            &store
                .paths
                .activation(&ProjectId::parse("project-a").unwrap()),
            &encode_activation_v2_for_migration(&v2_activation).unwrap(),
        )
        .unwrap();
        assert!(store.load_activation("project-a").is_err());
        let mut wrong_activation_version = legacy_activation;
        wrong_activation_version.version = MIGRATION_STORE_VERSION;
        assert!(store.save_activation(&wrong_activation_version).is_err());
    }

    #[test]
    fn migration_manifest_verification_binds_descriptor_and_raw_bytes() {
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "a".repeat(64),
            size: 3,
        }];
        let descriptor = descriptor(&entries);
        let bytes = manifest_bytes(&entries);
        let generation = generation_id("host-a", &descriptor);
        let evidence = verify_generation_manifest_for_migration(
            &bytes,
            &descriptor,
            "host-a",
            &generation,
            &StoreLimits::default(),
        )
        .unwrap();
        assert_eq!(evidence.generation_id, generation);
        assert_eq!(evidence.manifest_sha256, descriptor.manifest_sha256);
        assert_eq!(evidence.raw_manifest_sha256, sha256_hex(&bytes));
        assert_eq!(evidence.file_count, 1);
        assert_eq!(evidence.logical_bytes, 3);
        let mut terminal_empty_record = bytes.clone();
        terminal_empty_record.push(b'\n');
        assert!(
            verify_generation_manifest_for_migration(
                &terminal_empty_record,
                &descriptor,
                "host-a",
                &generation,
                &StoreLimits::default(),
            )
            .is_err()
        );
        assert!(
            verify_generation_manifest_for_migration(
                &bytes,
                &descriptor,
                "host-a",
                &"f".repeat(64),
                &StoreLimits::default(),
            )
            .is_err()
        );
        let mut tampered = bytes;
        tampered.extend_from_slice(b"{}\n");
        assert!(
            verify_generation_manifest_for_migration(
                &tampered,
                &descriptor,
                "host-a",
                &generation,
                &StoreLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn collision_retirement_codec_is_strict() {
        let record = CollisionRetirementPendingV1 {
            version: STORE_VERSION,
            project_id: ProjectId::parse("project-a").unwrap(),
            former_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id: "a".repeat(64),
            selector: materialized_selector("project-a", &"a".repeat(64)),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            manifest_sha256: "b".repeat(64),
            inventory_hash: "c".repeat(64),
            plan_hash: "d".repeat(64),
        };
        let bytes = encode_collision_retirement_pending_for_migration(&record).unwrap();
        assert_eq!(
            decode_collision_retirement_pending_for_migration(&bytes).unwrap(),
            record
        );
        let mut unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        assert!(
            decode_collision_retirement_pending_for_migration(
                &serde_json::to_vec(&unknown).unwrap()
            )
            .is_err()
        );
        let mut invalid_selector = record.clone();
        invalid_selector.selector = "selector-a".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_selector).is_err());
        let mut invalid_hash = record;
        invalid_hash.plan_hash = "not-a-hash".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
        invalid_hash.plan_hash = "d".repeat(64);
        invalid_hash.snapshot_id = "snapshot-a".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
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
                selector: materialized_selector("project-a", &ready.generation_id),
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
    fn activation_writer_contends_on_effective_source_manifest_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let anchor =
            acquire_store_lock_nofollow(&store.root().join("effective-source-manifest.json"))
                .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = store.save_activation(&ActivationRecord {
                version: STORE_VERSION,
                project_id: "project-a".into(),
                generation_id: "a".repeat(64),
                selector: "selector-a".into(),
                snapshot_id: "snapshot-a".into(),
                document_count: 1,
                entity_inventory_sha256: "b".repeat(64),
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: now_unix_secs(),
                cutback_pending: false,
                diagnostic: None,
            });
            done_tx.send(result).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(200)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(anchor);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        writer.join().unwrap();

        let reopened = open_store(&root);
        assert_eq!(
            reopened
                .load_activation("project-a")
                .unwrap()
                .unwrap()
                .snapshot_id,
            "snapshot-a"
        );
    }

    #[test]
    fn collision_pending_is_a_gc_root_until_the_record_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.retained_generations = 0;
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let bytes = b"collision generation";
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
        let stored = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        store
            .mark_generation_state(
                &descriptor.scope,
                &stored.generation_id,
                GenerationState::Superseded,
                None,
            )
            .unwrap();

        let project_id = ProjectId::parse("project-a").unwrap();
        let selector = materialized_selector(project_id.as_str(), &stored.generation_id);
        let pending = CollisionRetirementPendingV1 {
            version: STORE_VERSION,
            project_id: project_id.clone(),
            former_scope: descriptor.scope,
            generation_id: stored.generation_id,
            selector,
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            manifest_sha256: descriptor.manifest_sha256,
            inventory_hash: "c".repeat(64),
            plan_hash: "d".repeat(64),
        };
        let pending_path = store.paths.collision_retirement_pending(&project_id);
        atomic_write(
            &pending_path,
            &encode_collision_retirement_pending_for_migration(&pending).unwrap(),
        )
        .unwrap();

        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 0);
        assert!(store.blob_path(&hash).is_file());
        fs::remove_file(&pending_path).unwrap();
        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 1);
        assert!(!store.blob_path(&hash).exists());
    }

    #[test]
    fn collision_pending_gc_scan_is_missing_safe_and_corruption_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let collision_directory = store.root().join("collision-retirements");
        assert!(!collision_directory.exists());
        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 0);
        assert!(!collision_directory.exists());

        let orphan_bytes = b"orphan";
        let orphan_hash = sha256_hex(orphan_bytes);
        let orphan = store.blob_path(&orphan_hash);
        fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        fs::write(&orphan, orphan_bytes).unwrap();
        fs::create_dir_all(&collision_directory).unwrap();
        let pending_path = collision_directory.join("project-a.json");
        fs::write(&pending_path, b"not-json").unwrap();
        assert!(store.gc_blobs().is_err());
        assert!(orphan.is_file());

        fs::write(
            &pending_path,
            vec![b'x'; MAX_COLLISION_RETIREMENT_RECORD_BYTES + 1],
        )
        .unwrap();
        assert!(store.gc_blobs().is_err());
        assert!(orphan.is_file());
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
                selector: materialized_selector("project-a", &ready.generation_id),
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
