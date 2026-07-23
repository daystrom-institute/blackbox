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
pub const MAX_SNAPSHOT_ID_BYTES: usize = 512;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const MAX_CHUNK_TARGET_KEY_BYTES: usize = 4_096;
const MAX_MIGRATION_RECORD_BYTES: usize = 512 * 1024 * 1024;
const MAX_STORED_GENERATION_RECORD_BYTES: usize = 64 * 1024;
const MAX_COLLISION_RETIREMENT_RECORD_BYTES: usize = 64 * 1024;
const MAX_RETIREMENT_RECORD_BYTES: usize = 64 * 1024;
const MAX_MIGRATION_INVENTORY_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const RADIX_BUCKET_MAX_NAMES: usize = 1_024;
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
    pub max_migration_survivor_rows: usize,
    pub max_migration_survivor_bytes: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_manifest_files: DEFAULT_MAX_MANIFEST_FILES,
            max_manifest_logical_bytes: DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
            max_open_uploads_per_producer: 2,
            retained_generations: 2,
            unreferenced_blob_grace_hours: 168,
            max_migration_survivor_rows: MAX_MIGRATION_INVENTORY_GENERATIONS,
            max_migration_survivor_bytes: MAX_MIGRATION_INVENTORY_MANIFEST_BYTES,
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

/// A migration snapshot captured by the live store owner under its mutation
/// lock and with the exact configured limits used by that owner.
pub struct MigrationOwnedLegacyInventoryV1<'a> {
    pub inventory: MigrationLegacyInventoryV1,
    pub limits: StoreLimits,
    _mutation: StoreMutationGuard<'a>,
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

    pub fn snapshot_legacy_v1_for_scopes(
        &self,
        limits: &StoreLimits,
        catalog_scopes: &BTreeSet<PublishedScope>,
    ) -> Result<MigrationLegacyInventoryV1> {
        enumerate_legacy_migration_inventory_for_scopes_locked(self.paths, limits, catalog_scopes)
    }

    pub fn snapshot_current_v2(&self, limits: &StoreLimits) -> Result<MigrationCurrentInventoryV1> {
        enumerate_current_migration_inventory_locked(self.paths, limits)
    }

    pub fn snapshot_current_v2_for_scopes(
        &self,
        limits: &StoreLimits,
        catalog_scopes: &BTreeSet<PublishedScope>,
        expected_retirement_selectors: &BTreeSet<String>,
    ) -> Result<MigrationCurrentInventoryV1> {
        enumerate_current_migration_inventory_for_scopes_locked(
            self.paths,
            limits,
            catalog_scopes,
            expected_retirement_selectors,
        )
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

#[derive(Debug, Clone)]
enum MixedStoredGeneration {
    LegacyV1(StoredGeneration),
    CurrentV2(StoredGenerationV2),
}

impl MixedStoredGeneration {
    fn validate(&self) -> Result<()> {
        match self {
            Self::LegacyV1(record) => validate_stored_generation_v1(record),
            Self::CurrentV2(record) => record.validate(),
        }
    }

    fn generation_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.generation_id,
            Self::CurrentV2(record) => &record.generation_id,
        }
    }

    fn ordinal(&self) -> u64 {
        match self {
            Self::LegacyV1(record) => record.ordinal,
            Self::CurrentV2(record) => record.ordinal,
        }
    }

    fn descriptor(&self) -> &GenerationDescriptor {
        match self {
            Self::LegacyV1(record) => &record.descriptor,
            Self::CurrentV2(record) => &record.descriptor,
        }
    }

    fn state(&self) -> GenerationState {
        match self {
            Self::LegacyV1(record) => record.state,
            Self::CurrentV2(record) => record.state,
        }
    }

    fn materialized_doc_count(&self) -> Option<u64> {
        match self {
            Self::LegacyV1(record) => record.materialized_doc_count,
            Self::CurrentV2(record) => record.materialized_doc_count,
        }
    }

    fn entity_inventory_sha256(&self) -> Option<&str> {
        match self {
            Self::LegacyV1(record) => record.entity_inventory_sha256.as_deref(),
            Self::CurrentV2(record) => record.entity_inventory_sha256.as_deref(),
        }
    }

    fn is_legacy_v1(&self) -> bool {
        matches!(self, Self::LegacyV1(_))
    }

    fn mark_missing_blob_data(&mut self) {
        let diagnostic = Some("one or more retained source blobs failed verification".to_string());
        match self {
            Self::LegacyV1(record) => {
                record.state = GenerationState::MissingBlobData;
                record.diagnostic = diagnostic;
            }
            Self::CurrentV2(record) => {
                record.state = GenerationState::MissingBlobData;
                record.diagnostic = diagnostic;
            }
        }
    }
}

#[derive(Debug, Clone)]
enum MixedActivationRecord {
    LegacyV1(ActivationRecord),
    CurrentV2(ActivationRecordV2),
}

impl MixedActivationRecord {
    fn project_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.project_id,
            Self::CurrentV2(record) => record.project_id.as_str(),
        }
    }

    fn generation_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.generation_id,
            Self::CurrentV2(record) => &record.generation_id,
        }
    }

    fn published_scope(&self) -> Option<&PublishedScope> {
        match self {
            Self::LegacyV1(_) => None,
            Self::CurrentV2(record) => Some(&record.published_scope),
        }
    }

    fn document_count(&self) -> u64 {
        match self {
            Self::LegacyV1(record) => record.document_count,
            Self::CurrentV2(record) => record.document_count,
        }
    }

    fn entity_inventory_sha256(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.entity_inventory_sha256,
            Self::CurrentV2(record) => &record.entity_inventory_sha256,
        }
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
pub struct CollisionRetirementLifecycleV1 {
    pub version: u32,
    pub state: CollisionRetirementLifecycleStateV1,
    pub project_id: ProjectId,
    pub former_scope: PublishedScope,
    pub generation_id: String,
    pub selector_evidence: CollisionRetirementSelectorEvidenceV1,
    pub snapshot_id: String,
    pub manifest_sha256: String,
    pub inventory_hash: String,
    pub plan_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollisionRetirementSelectorEvidenceV1 {
    ExactMaterialized(String),
    NoDurableSelector,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollisionRetirementLifecycleStateV1 {
    Pending,
    Queued,
    Completed,
}

impl CollisionRetirementLifecycleV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION {
            bail!("invalid collision retirement lifecycle version");
        }
        ProjectId::parse(self.project_id.to_string()).map_err(|error| anyhow!(error))?;
        self.former_scope.validate()?;
        validate_sha256(&self.generation_id)?;
        if let CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) =
            &self.selector_evidence
        {
            validate_retirement_selector(selector)?;
            validate_collected_materialization_selector(
                self.project_id.as_str(),
                &self.generation_id,
                selector,
            )?;
        }
        validate_migration_snapshot_id(&self.snapshot_id)?;
        validate_sha256(&self.manifest_sha256)?;
        validate_sha256(&self.inventory_hash)?;
        validate_sha256(&self.plan_hash)?;
        Ok(())
    }

    fn matches_queue(&self, record: &RetirementRecord) -> bool {
        self.exact_selector().is_some_and(|selector| {
            record.project_id == self.project_id.as_str()
                && record.selector == selector
                && record.snapshot_id == self.snapshot_id
                && record.generation_id.as_deref() == Some(self.generation_id.as_str())
        })
    }

    pub fn exact_selector(&self) -> Option<&str> {
        match &self.selector_evidence {
            CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) => Some(selector),
            CollisionRetirementSelectorEvidenceV1::NoDurableSelector => None,
        }
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
    pub record: CollisionRetirementLifecycleV1,
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
    pub record: CollisionRetirementLifecycleV1,
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
    pub collision_lifecycle_count: u64,
    pub collision_lifecycle_set_sha256: String,
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

struct CanonicalGenerationSetCommitment {
    count: u64,
    hasher: Sha256,
    prior_key: Option<String>,
}

struct CanonicalCollisionLifecycleCommitment {
    count: u64,
    hasher: Sha256,
    prior_project_id: Option<ProjectId>,
}

impl CanonicalCollisionLifecycleCommitment {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        let domain = b"bbox-code-source-collision-lifecycle-set-v1";
        hasher.update((domain.len() as u64).to_be_bytes());
        hasher.update(domain);
        Self {
            count: 0,
            hasher,
            prior_project_id: None,
        }
    }

    fn add(&mut self, project_id: &ProjectId, bytes: &[u8]) -> Result<()> {
        if self
            .prior_project_id
            .as_ref()
            .is_some_and(|prior| prior >= project_id)
        {
            bail!("collision lifecycle rows are duplicated or out of order");
        }
        self.hasher
            .update((project_id.as_str().len() as u64).to_be_bytes());
        self.hasher.update(project_id.as_str().as_bytes());
        let digest = sha256_hex(bytes);
        self.hasher.update((digest.len() as u64).to_be_bytes());
        self.hasher.update(digest.as_bytes());
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| anyhow!("collision lifecycle row count overflowed"))?;
        self.prior_project_id = Some(project_id.clone());
        Ok(())
    }

    fn finish(mut self) -> (u64, String) {
        self.hasher.update(self.count.to_be_bytes());
        (self.count, hex::encode(self.hasher.finalize()))
    }
}

impl CanonicalGenerationSetCommitment {
    fn new(domain: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update((domain.len() as u64).to_be_bytes());
        hasher.update(domain);
        Self {
            count: 0,
            hasher,
            prior_key: None,
        }
    }

    #[cfg(test)]
    fn add(&mut self, row: &MigrationLegacyGenerationEvidenceV1) -> Result<()> {
        self.add_fields(
            &row.published_scope,
            &row.generation_id,
            &row.metadata_sha256,
            &row.manifest_sha256,
            &row.record.descriptor.manifest_sha256,
        )
    }

    fn add_summary(&mut self, row: &LegacyGenerationRowSummary) -> Result<()> {
        self.add_fields(
            &row.published_scope,
            &row.generation_id,
            &row.metadata_sha256,
            &row.manifest_sha256,
            &row.record.descriptor.manifest_sha256,
        )
    }

    fn add_fields(
        &mut self,
        published_scope: &PublishedScope,
        generation_id: &str,
        metadata_sha256: &str,
        manifest_sha256: &str,
        descriptor_manifest_sha256: &str,
    ) -> Result<()> {
        fn field(hasher: &mut Sha256, value: &[u8]) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }

        let key = format!("{}/{}", scope_hash(published_scope), generation_id);
        if self.prior_key.as_ref().is_some_and(|prior| prior >= &key) {
            bail!("canonical generation rows are duplicated or out of order");
        }
        field(
            &mut self.hasher,
            b"bbox-code-source-legacy-generation-row-v1",
        );
        field(&mut self.hasher, key.as_bytes());
        field(&mut self.hasher, published_scope.repo_id().as_bytes());
        field(
            &mut self.hasher,
            published_scope.bbox_root_relpath().as_bytes(),
        );
        field(&mut self.hasher, metadata_sha256.as_bytes());
        field(&mut self.hasher, manifest_sha256.as_bytes());
        field(&mut self.hasher, descriptor_manifest_sha256.as_bytes());
        self.prior_key = Some(key);
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| anyhow!("legacy generation count overflowed"))?;
        Ok(())
    }

    fn finish(mut self) -> String {
        self.hasher.update(self.count.to_be_bytes());
        hex::encode(self.hasher.finalize())
    }
}

fn walk_sha256_names_lexically(
    path: &Path,
    label: &str,
    directories: bool,
    visit: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let Some(directory) = NofollowDirectory::open_existing(path)? else {
        return Ok(());
    };
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
        validate_sha256(&name)?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink()
            || if directories {
                !file_type.is_dir()
            } else {
                !file_type.is_file()
            }
        {
            bail!("{label} directory contains an unexpected entry type");
        }
    }

    fn walk_prefix(
        path: &Path,
        label: &str,
        prefix: &mut String,
        visit: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let mut count = 0_usize;
        let mut next_digits = [false; 16];
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
            validate_sha256(&name)?;
            if !name.starts_with(prefix.as_str()) {
                continue;
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("{label} directory row count overflowed"))?;
            if prefix.len() < 64 {
                let digit = name.as_bytes()[prefix.len()];
                let index = match digit {
                    b'0'..=b'9' => usize::from(digit - b'0'),
                    b'a'..=b'f' => usize::from(digit - b'a') + 10,
                    _ => unreachable!("validated sha256 names contain only lowercase hex"),
                };
                next_digits[index] = true;
            }
        }
        if count == 0 {
            return Ok(());
        }
        if count <= RADIX_BUCKET_MAX_NAMES {
            let mut names = Vec::with_capacity(count);
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .to_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
                validate_sha256(&name)?;
                if name.starts_with(prefix.as_str()) {
                    names.push(name);
                }
            }
            names.sort_unstable();
            for name in names {
                visit(&name)?;
            }
            return Ok(());
        }
        for (index, present) in next_digits.into_iter().enumerate() {
            if present {
                let digit = if index < 10 {
                    b'0' + index as u8
                } else {
                    b'a' + (index - 10) as u8
                };
                prefix.push(char::from(digit));
                walk_prefix(path, label, prefix, visit)?;
                prefix.pop();
            }
        }
        Ok(())
    }

    walk_prefix(path, label, &mut String::with_capacity(64), visit)?;
    directory.ensure_still_current()?;
    Ok(())
}

fn walk_sha256_json_files_lexically(
    path: &Path,
    label: &str,
    visit: &mut impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let Some(directory) = NofollowDirectory::open_existing(path)? else {
        return Ok(());
    };
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
        let hash = name
            .strip_suffix(".json")
            .ok_or_else(|| anyhow!("{label} filename is not canonical"))?;
        validate_sha256(hash)?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            bail!("{label} directory contains an unexpected entry type");
        }
    }

    fn walk_prefix(
        path: &Path,
        label: &str,
        prefix: &mut String,
        visit: &mut impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<()> {
        let mut count = 0_usize;
        let mut next_digits = [false; 16];
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
            let hash = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("{label} filename is not canonical"))?;
            validate_sha256(hash)?;
            if !hash.starts_with(prefix.as_str()) {
                continue;
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| anyhow!("{label} directory row count overflowed"))?;
            if prefix.len() < 64 {
                let digit = hash.as_bytes()[prefix.len()];
                let index = match digit {
                    b'0'..=b'9' => usize::from(digit - b'0'),
                    b'a'..=b'f' => usize::from(digit - b'a') + 10,
                    _ => unreachable!("validated sha256 names contain only lowercase hex"),
                };
                next_digits[index] = true;
            }
        }
        if count == 0 {
            return Ok(());
        }
        if count <= RADIX_BUCKET_MAX_NAMES {
            let mut hashes = Vec::with_capacity(count);
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .to_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
                let hash = name
                    .strip_suffix(".json")
                    .ok_or_else(|| anyhow!("{label} filename is not canonical"))?;
                validate_sha256(hash)?;
                if hash.starts_with(prefix.as_str()) {
                    hashes.push(hash.to_string());
                }
            }
            hashes.sort_unstable();
            for hash in hashes {
                visit(&hash, &format!("{hash}.json"))?;
            }
            return Ok(());
        }
        for (index, present) in next_digits.into_iter().enumerate() {
            if present {
                let digit = if index < 10 {
                    b'0' + index as u8
                } else {
                    b'a' + (index - 10) as u8
                };
                prefix.push(char::from(digit));
                walk_prefix(path, label, prefix, visit)?;
                prefix.pop();
            }
        }
        Ok(())
    }

    walk_prefix(path, label, &mut String::with_capacity(64), visit)?;
    directory.ensure_still_current()?;
    Ok(())
}

fn walk_json_files_lexically(
    path: &Path,
    label: &str,
    mut validate_stem: impl FnMut(&str) -> Result<()>,
    mut visit: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    let Some(directory) = NofollowDirectory::open_existing(path)? else {
        return Ok(());
    };
    let mut cursor = None::<String>;
    loop {
        let mut page = BTreeSet::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{label} directory contains a non-utf8 entry"))?;
            let stem = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("{label} filename is not canonical"))?;
            validate_stem(stem)?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                bail!("{label} directory contains an unexpected entry type");
            }
            if cursor.as_ref().is_some_and(|cursor| &name <= cursor) {
                continue;
            }
            page.insert(name);
            if page.len() > RADIX_BUCKET_MAX_NAMES {
                page.pop_last();
            }
        }
        if page.is_empty() {
            break;
        }
        for name in &page {
            let stem = name
                .strip_suffix(".json")
                .expect("validated json filename keeps its suffix");
            visit(stem, name)?;
        }
        cursor = page.last().cloned();
    }
    directory.ensure_still_current()?;
    Ok(())
}

fn walk_collision_lifecycle_records(
    paths: &CodeSourceStorePaths,
    label: &str,
    mut visit: impl FnMut(ProjectId, Vec<u8>, CollisionRetirementLifecycleV1) -> Result<()>,
) -> Result<()> {
    let lifecycle_path = paths.root().join("collision-retirements");
    walk_json_files_lexically(
        &lifecycle_path,
        label,
        |project_name| {
            ProjectId::parse(project_name.to_string())
                .map(|_| ())
                .map_err(|error| anyhow!(error))
        },
        |project_name, name| {
            let project_id =
                ProjectId::parse(project_name.to_string()).map_err(|error| anyhow!(error))?;
            let directory = NofollowDirectory::open_existing(&lifecycle_path)?
                .ok_or_else(|| anyhow!("{label} directory disappeared"))?;
            let bytes = directory
                .read_regular(name, MAX_COLLISION_RETIREMENT_RECORD_BYTES, label)?
                .ok_or_else(|| anyhow!("{label} disappeared"))?;
            let record = decode_collision_retirement_pending_for_migration(&bytes)?;
            if record.project_id != project_id {
                bail!("{label} path and project disagree");
            }
            visit(project_id, bytes, record)?;
            directory.ensure_still_current()
        },
    )
}

struct LegacyGenerationRowSummary {
    published_scope: PublishedScope,
    generation_id: String,
    generation_path: PathBuf,
    metadata_bytes: Vec<u8>,
    metadata_sha256: String,
    record: StoredGeneration,
    manifest_len: usize,
    manifest_sha256: String,
}

impl LegacyGenerationRowSummary {
    fn encoded_bytes(&self) -> Result<usize> {
        self.metadata_bytes
            .len()
            .checked_add(self.manifest_len)
            .ok_or_else(|| anyhow!("legacy generation row byte count overflowed"))
    }

    fn materialize(self, max_row_bytes: usize) -> Result<MigrationLegacyGenerationEvidenceV1> {
        let encoded_bytes = self.encoded_bytes()?;
        if encoded_bytes > max_row_bytes {
            bail!("legacy generation row exceeds its configured survivor byte limit");
        }
        let directory = NofollowDirectory::open_existing(&self.generation_path)?
            .ok_or_else(|| anyhow!("legacy generation directory disappeared"))?;
        let manifest_bytes = directory
            .read_regular(
                "manifest.jsonl",
                self.manifest_len,
                "legacy generation manifest",
            )?
            .ok_or_else(|| anyhow!("legacy generation manifest disappeared"))?;
        if manifest_bytes.len() != self.manifest_len
            || sha256_hex(&manifest_bytes) != self.manifest_sha256
        {
            bail!("legacy generation manifest changed during enumeration");
        }
        directory.ensure_still_current()?;
        Ok(MigrationLegacyGenerationEvidenceV1 {
            published_scope: self.published_scope,
            generation_id: self.generation_id,
            metadata_bytes: self.metadata_bytes,
            metadata_sha256: self.metadata_sha256,
            record: self.record,
            manifest_bytes,
            manifest_sha256: self.manifest_sha256,
        })
    }
}

struct CurrentGenerationRowSummary {
    published_scope: PublishedScope,
    generation_id: String,
    generation_path: PathBuf,
    metadata_bytes: Vec<u8>,
    metadata_sha256: String,
    record: StoredGenerationV2,
    manifest_len: usize,
    manifest_sha256: String,
}

impl CurrentGenerationRowSummary {
    fn encoded_bytes(&self) -> Result<usize> {
        self.metadata_bytes
            .len()
            .checked_add(self.manifest_len)
            .ok_or_else(|| anyhow!("current generation row byte count overflowed"))
    }

    fn materialize(self, max_row_bytes: usize) -> Result<MigrationCurrentGenerationEvidenceV1> {
        if self.encoded_bytes()? > max_row_bytes {
            bail!("current generation row exceeds its configured survivor byte limit");
        }
        let directory = NofollowDirectory::open_existing(&self.generation_path)?
            .ok_or_else(|| anyhow!("current generation directory disappeared"))?;
        let manifest_bytes = directory
            .read_regular(
                "manifest.jsonl",
                self.manifest_len,
                "current generation manifest",
            )?
            .ok_or_else(|| anyhow!("current generation manifest disappeared"))?;
        if manifest_bytes.len() != self.manifest_len
            || sha256_hex(&manifest_bytes) != self.manifest_sha256
        {
            bail!("current generation manifest changed during enumeration");
        }
        directory.ensure_still_current()?;
        Ok(MigrationCurrentGenerationEvidenceV1 {
            published_scope: self.published_scope,
            generation_id: self.generation_id,
            metadata_bytes: self.metadata_bytes,
            metadata_sha256: self.metadata_sha256,
            record: self.record,
            manifest_bytes,
            manifest_sha256: self.manifest_sha256,
        })
    }
}

fn read_bounded_manifest_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<usize> {
    line.clear();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(line.len());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line
            .len()
            .checked_add(take)
            .is_none_or(|bytes| bytes > max_bytes)
        {
            bail!("generation manifest record exceeds its byte limit");
        }
        line.extend_from_slice(&available[..take]);
        let ended = available[take - 1] == b'\n';
        reader.consume(take);
        if ended {
            return Ok(line.len());
        }
    }
}

fn stream_verify_generation_manifest_for_migration(
    path: &Path,
    descriptor: &GenerationDescriptor,
    producer_id: &str,
    expected_generation_id: &str,
    limits: &StoreLimits,
) -> Result<(usize, String)> {
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    validate_producer_id(producer_id)?;
    validate_sha256(expected_generation_id)?;
    descriptor.validate_header()?;
    if generation_id(producer_id, descriptor) != expected_generation_id {
        bail!("generation manifest identity does not match descriptor");
    }
    let file = open_regular_nofollow(path, "legacy generation manifest")?;
    let manifest_len = usize::try_from(file.metadata()?.len())
        .map_err(|_| anyhow!("generation manifest length exceeds usize"))?;
    if manifest_len > MAX_MIGRATION_RECORD_BYTES {
        bail!("generation manifest exceeds the migration byte limit");
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut raw_hasher = Sha256::new();
    let mut manifest_hasher = Sha256::new();
    field(&mut manifest_hasher, b"bbox-code-source-manifest-v1");
    let mut previous_path = None::<String>;
    let mut file_count = 0_u64;
    let mut logical_bytes = 0_u64;
    let max_line_bytes = 64 * 1024;
    loop {
        let read = read_bounded_manifest_line(&mut reader, &mut line, max_line_bytes)?;
        if read == 0 {
            break;
        }
        raw_hasher.update(&line);
        let record = if line.last() == Some(&b'\n') {
            &line[..line.len() - 1]
        } else {
            line.as_slice()
        };
        if record.iter().all(u8::is_ascii_whitespace) {
            bail!("generation manifest contains an empty record");
        }
        let entry: ManifestEntry = serde_json::from_slice(record)
            .with_context(|| format!("parsing generation manifest record {}", file_count + 1))?;
        entry.validate()?;
        if previous_path
            .as_ref()
            .is_some_and(|previous| entry.relative_path.as_str() <= previous.as_str())
        {
            bail!("generation manifest paths are duplicated or out of order");
        }
        file_count = file_count
            .checked_add(1)
            .ok_or_else(|| anyhow!("generation manifest file count overflowed"))?;
        if file_count > limits.max_manifest_files {
            bail!("generation manifest exceeds the file count limit");
        }
        logical_bytes = logical_bytes
            .checked_add(entry.size)
            .ok_or_else(|| anyhow!("generation manifest logical byte count overflowed"))?;
        if logical_bytes > limits.max_manifest_logical_bytes {
            bail!("generation manifest exceeds the logical byte limit");
        }
        field(&mut manifest_hasher, entry.relative_path.as_bytes());
        field(&mut manifest_hasher, entry.content_sha256.as_bytes());
        manifest_hasher.update(entry.size.to_be_bytes());
        previous_path = Some(entry.relative_path);
    }
    if file_count != descriptor.file_count || logical_bytes != descriptor.logical_bytes {
        bail!("generation manifest counts do not match descriptor");
    }
    let manifest_sha256 = hex::encode(manifest_hasher.finalize());
    if manifest_sha256 != descriptor.manifest_sha256 {
        bail!("generation manifest digest does not match descriptor");
    }
    let mut dirty_hasher = Sha256::new();
    field(&mut dirty_hasher, b"bbox-code-source-dirty-v1");
    field(&mut dirty_hasher, descriptor.head_commit.as_bytes());
    field(&mut dirty_hasher, manifest_sha256.as_bytes());
    if hex::encode(dirty_hasher.finalize()) != descriptor.dirty_fingerprint {
        bail!("generation manifest dirty fingerprint does not match descriptor");
    }
    Ok((manifest_len, hex::encode(raw_hasher.finalize())))
}

fn walk_legacy_generation_rows(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    mut visit: impl FnMut(LegacyGenerationRowSummary) -> Result<()>,
) -> Result<()> {
    let scopes_path = paths.root().join("scopes");
    walk_sha256_names_lexically(&scopes_path, "legacy scope", true, &mut |scope_name| {
        let scope_path = scopes_path.join(&scope_name);
        let scope_entries = sorted_directory_entry_names(&scope_path, 1, "legacy scope")?;
        if scope_entries.len() != 1 || scope_entries[0] != "generations" {
            bail!("legacy scope directory has an incomplete or unexpected row set");
        }
        let generations_path = scope_path.join("generations");
        walk_sha256_names_lexically(
            &generations_path,
            "legacy generation",
            true,
            &mut |generation_id| {
                let generation_path = generations_path.join(&generation_id);
                let directory = NofollowDirectory::open_existing(&generation_path)?
                    .ok_or_else(|| anyhow!("legacy generation directory disappeared"))?;
                let entries = sorted_regular_entry_names(&generation_path, 2, "legacy generation")?;
                if entries.len() != 2
                    || entries[0] != "manifest.jsonl"
                    || entries[1] != "metadata.json"
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
                let manifest_path = generation_path.join("manifest.jsonl");
                let (manifest_len, manifest_sha256) =
                    stream_verify_generation_manifest_for_migration(
                        &manifest_path,
                        &record.descriptor,
                        &record.producer_id,
                        &record.generation_id,
                        limits,
                    )?;
                visit(LegacyGenerationRowSummary {
                    published_scope: record.descriptor.scope.clone(),
                    generation_id: generation_id.to_string(),
                    generation_path,
                    metadata_sha256: sha256_hex(&metadata_bytes),
                    metadata_bytes,
                    manifest_len,
                    manifest_sha256,
                    record,
                })?;
                directory.ensure_still_current()?;
                Ok(())
            },
        )?;
        Ok(())
    })
}

struct LegacyRetentionCandidate {
    ordinal: u64,
    generation_id: String,
    summary: Option<LegacyGenerationRowSummary>,
}

impl LegacyRetentionCandidate {
    fn evidence_bytes(&self) -> Result<usize> {
        self.summary
            .as_ref()
            .map_or(Ok(0), LegacyGenerationRowSummary::encoded_bytes)
    }
}

fn insert_legacy_retention_candidate(
    candidates_by_scope: &mut BTreeMap<PublishedScope, Vec<LegacyRetentionCandidate>>,
    scope: PublishedScope,
    candidate: LegacyRetentionCandidate,
    retained_generations: usize,
    materialized_candidate_count: &mut usize,
    materialized_candidate_bytes: &mut usize,
    intrinsic_count: usize,
    intrinsic_bytes: usize,
    base_bytes: usize,
    limits: &StoreLimits,
) -> Result<()> {
    if retained_generations == 0 {
        return Ok(());
    }
    let added_bytes = candidate.evidence_bytes()?;
    let added_materialized = usize::from(candidate.summary.is_some());
    let candidates = candidates_by_scope.entry(scope).or_default();
    let compare = |left: &LegacyRetentionCandidate, right: &LegacyRetentionCandidate| {
        right
            .ordinal
            .cmp(&left.ordinal)
            .then_with(|| left.generation_id.cmp(&right.generation_id))
    };
    let position = candidates
        .binary_search_by(|existing| compare(existing, &candidate))
        .unwrap_or_else(|position| position);
    if position >= retained_generations {
        return Ok(());
    }
    if added_bytes > limits.max_migration_survivor_bytes {
        bail!("retained legacy generation row exceeds its configured survivor byte limit");
    }
    let removed = (candidates.len() == retained_generations).then(|| {
        candidates
            .last()
            .expect("full retained candidate set has a worst row")
    });
    let removed_count = removed.map_or(0, |row| usize::from(row.summary.is_some()));
    let removed_bytes = removed.map_or(Ok(0), LegacyRetentionCandidate::evidence_bytes)?;
    let next_count = materialized_candidate_count
        .checked_sub(removed_count)
        .and_then(|count| count.checked_add(added_materialized))
        .ok_or_else(|| anyhow!("retained legacy candidate count overflowed"))?;
    let next_bytes = materialized_candidate_bytes
        .checked_sub(removed_bytes)
        .and_then(|bytes| bytes.checked_add(added_bytes))
        .ok_or_else(|| anyhow!("retained legacy candidate byte count overflowed"))?;
    if intrinsic_count
        .checked_add(next_count)
        .is_none_or(|rows| rows > limits.max_migration_survivor_rows)
    {
        bail!("protected legacy generation inventory exceeds its row limit");
    }
    if base_bytes
        .checked_add(intrinsic_bytes)
        .and_then(|bytes| bytes.checked_add(next_bytes))
        .is_none_or(|bytes| bytes > limits.max_migration_survivor_bytes)
    {
        bail!("protected legacy inventory exceeds its aggregate byte limit");
    }
    candidates.insert(position, candidate);
    if candidates.len() > retained_generations {
        candidates.pop();
    }
    *materialized_candidate_count = next_count;
    *materialized_candidate_bytes = next_bytes;
    Ok(())
}

enum CurrentRetentionEvidence {
    RootMarker,
    LegacyV1,
    CurrentV2(CurrentGenerationRowSummary),
}

struct CurrentRetentionCandidate {
    ordinal: u64,
    generation_id: String,
    evidence: CurrentRetentionEvidence,
}

impl CurrentRetentionCandidate {
    fn evidence_bytes(&self) -> Result<usize> {
        match &self.evidence {
            CurrentRetentionEvidence::RootMarker | CurrentRetentionEvidence::LegacyV1 => Ok(0),
            CurrentRetentionEvidence::CurrentV2(row) => row.encoded_bytes(),
        }
    }

    fn materialized(&self) -> usize {
        usize::from(matches!(
            &self.evidence,
            CurrentRetentionEvidence::CurrentV2(_)
        ))
    }
}

fn insert_current_retention_candidate(
    candidates: &mut Vec<CurrentRetentionCandidate>,
    candidate: CurrentRetentionCandidate,
    retained_generations: usize,
    materialized_count: &mut usize,
    materialized_bytes: &mut usize,
) -> Result<()> {
    if retained_generations == 0 {
        return Ok(());
    }

    let position = candidates
        .binary_search_by(|existing| {
            candidate
                .ordinal
                .cmp(&existing.ordinal)
                .then_with(|| existing.generation_id.cmp(&candidate.generation_id))
        })
        .unwrap_or_else(|position| position);
    if position >= retained_generations {
        return Ok(());
    }

    let (removed_count, removed_bytes) = if candidates.len() == retained_generations {
        let removed = candidates
            .last()
            .expect("full current retained candidate set has a worst row");
        (removed.materialized(), removed.evidence_bytes()?)
    } else {
        (0, 0)
    };
    let candidate_bytes = candidate.evidence_bytes()?;
    let next_count = (*materialized_count)
        .checked_sub(removed_count)
        .and_then(|count| count.checked_add(candidate.materialized()))
        .ok_or_else(|| anyhow!("current retained generation count overflowed"))?;
    let next_bytes = (*materialized_bytes)
        .checked_sub(removed_bytes)
        .and_then(|bytes| bytes.checked_add(candidate_bytes))
        .ok_or_else(|| anyhow!("current retained generation byte count overflowed"))?;

    candidates.insert(position, candidate);
    if candidates.len() > retained_generations {
        candidates
            .pop()
            .expect("oversized current retained candidate set has a worst row");
    }
    *materialized_count = next_count;
    *materialized_bytes = next_bytes;
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
    enumerate_legacy_migration_inventory_for_scopes_locked(paths, limits, &BTreeSet::new())
}

pub fn enumerate_legacy_migration_inventory_for_scopes_locked(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    catalog_scopes: &BTreeSet<PublishedScope>,
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
            generation_set_sha256: CanonicalGenerationSetCommitment::new(
                b"bbox-code-source-legacy-generation-set-v1",
            )
            .finish(),
            unprotected_generation_count: 0,
            unprotected_generation_set_sha256: CanonicalGenerationSetCommitment::new(
                b"bbox-code-source-legacy-unprotected-generation-set-v1",
            )
            .finish(),
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
            if total_encoded_bytes > limits.max_migration_survivor_bytes {
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
            if total_encoded_bytes > limits.max_migration_survivor_bytes {
                bail!("legacy inventory exceeds its aggregate byte limit");
            }
            if record.project_id != project_id
                || record.state != CollisionRetirementLifecycleStateV1::Pending
            {
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
    let mut authority_scopes = catalog_scopes.clone();
    authority_scopes.extend(
        collision_pending
            .iter()
            .map(|row| row.record.former_scope.clone()),
    );
    if let MigrationLegacyAnchorEvidenceV1::Present { bytes, .. } = &anchor {
        authority_scopes.extend(
            decode_migration_effective_source_manifest_v1(bytes)?
                .selections
                .into_iter()
                .map(|selection| selection.published_scope),
        );
    }

    let mut found_root_generation_ids = BTreeSet::new();
    let mut full_generation_set =
        CanonicalGenerationSetCommitment::new(b"bbox-code-source-legacy-generation-set-v1");
    walk_legacy_generation_rows(paths, limits, |row| {
        full_generation_set.add_summary(&row)?;
        if root_generation_ids.contains(&row.generation_id) {
            found_root_generation_ids.insert(row.generation_id.clone());
            authority_scopes.insert(row.published_scope);
        }
        Ok(())
    })?;
    if found_root_generation_ids != root_generation_ids {
        bail!("legacy activation or collision references missing generation metadata");
    }

    let mut repeated_full_generation_set =
        CanonicalGenerationSetCommitment::new(b"bbox-code-source-legacy-generation-set-v1");
    let mut generations = Vec::new();
    let mut retained_by_scope = BTreeMap::<PublishedScope, Vec<LegacyRetentionCandidate>>::new();
    let mut retained_candidate_count = 0_usize;
    let mut retained_candidate_bytes = 0_usize;
    let mut intrinsic_bytes = 0_usize;
    walk_legacy_generation_rows(paths, limits, |row| {
        repeated_full_generation_set.add_summary(&row)?;
        if !authority_scopes.contains(&row.published_scope) {
            return Ok(());
        }
        let rooted = root_generation_ids.contains(&row.generation_id);
        if row.record.state == GenerationState::Superseded {
            if rooted {
                insert_legacy_retention_candidate(
                    &mut retained_by_scope,
                    row.published_scope.clone(),
                    LegacyRetentionCandidate {
                        ordinal: row.record.ordinal,
                        generation_id: row.generation_id.clone(),
                        summary: None,
                    },
                    limits.retained_generations,
                    &mut retained_candidate_count,
                    &mut retained_candidate_bytes,
                    generations.len(),
                    intrinsic_bytes,
                    total_encoded_bytes,
                    limits,
                )?;
            } else {
                insert_legacy_retention_candidate(
                    &mut retained_by_scope,
                    row.published_scope.clone(),
                    LegacyRetentionCandidate {
                        ordinal: row.record.ordinal,
                        generation_id: row.generation_id.clone(),
                        summary: Some(row),
                    },
                    limits.retained_generations,
                    &mut retained_candidate_count,
                    &mut retained_candidate_bytes,
                    generations.len(),
                    intrinsic_bytes,
                    total_encoded_bytes,
                    limits,
                )?;
                return Ok(());
            }
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
            let row_bytes = row.encoded_bytes()?;
            if row_bytes > limits.max_migration_survivor_bytes {
                bail!("protected legacy generation row exceeds its configured survivor byte limit");
            }
            let next_intrinsic_bytes = intrinsic_bytes
                .checked_add(row_bytes)
                .ok_or_else(|| anyhow!("protected legacy inventory byte count overflowed"))?;
            if generations
                .len()
                .checked_add(retained_candidate_count)
                .is_none_or(|rows| rows >= limits.max_migration_survivor_rows)
            {
                bail!("protected legacy generation inventory exceeds its row limit");
            }
            if total_encoded_bytes
                .checked_add(next_intrinsic_bytes)
                .and_then(|bytes| bytes.checked_add(retained_candidate_bytes))
                .is_none_or(|bytes| bytes > limits.max_migration_survivor_bytes)
            {
                bail!("protected legacy inventory exceeds its aggregate byte limit");
            }
            let evidence = row.materialize(limits.max_migration_survivor_bytes)?;
            intrinsic_bytes = next_intrinsic_bytes;
            generations.push(evidence);
        }
        Ok(())
    })?;
    for candidates in retained_by_scope.into_values() {
        for candidate in candidates {
            if let Some(summary) = candidate.summary {
                generations.push(summary.materialize(limits.max_migration_survivor_bytes)?);
            }
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
    let materialized_generation_ids = generations
        .iter()
        .map(|row| row.generation_id.clone())
        .collect::<BTreeSet<_>>();
    if protected_generation_ids != materialized_generation_ids {
        bail!("bounded legacy survivor materialization includes a non-protected generation");
    }
    let protected_identities: BTreeSet<(String, String)> = generations
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
    if total_encoded_bytes > limits.max_migration_survivor_bytes {
        bail!("protected legacy inventory exceeds its aggregate byte limit");
    }

    let mut unprotected_generation_set = CanonicalGenerationSetCommitment::new(
        b"bbox-code-source-legacy-unprotected-generation-set-v1",
    );
    let mut final_full_generation_set =
        CanonicalGenerationSetCommitment::new(b"bbox-code-source-legacy-generation-set-v1");
    walk_legacy_generation_rows(paths, limits, |row| {
        final_full_generation_set.add_summary(&row)?;
        let identity = (scope_hash(&row.published_scope), row.generation_id.clone());
        if !protected_identities.contains(&identity) {
            unprotected_generation_set.add_summary(&row)?;
        }
        Ok(())
    })?;
    let generation_count = full_generation_set.count;
    let repeated_generation_count = repeated_full_generation_set.count;
    let final_generation_count = final_full_generation_set.count;
    let unprotected_generation_count = unprotected_generation_set.count;
    let generation_set_sha256 = full_generation_set.finish();
    if repeated_generation_count != generation_count
        || repeated_full_generation_set.finish() != generation_set_sha256
        || final_generation_count != generation_count
        || final_full_generation_set.finish() != generation_set_sha256
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
        generation_count,
        generation_set_sha256,
        unprotected_generation_count,
        unprotected_generation_set_sha256: unprotected_generation_set.finish(),
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
    enumerate_current_migration_inventory_for_scopes_locked(
        paths,
        limits,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
}

pub fn enumerate_current_migration_inventory_for_scopes_locked(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    catalog_scopes: &BTreeSet<PublishedScope>,
    expected_retirement_selectors: &BTreeSet<String>,
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
    if total_encoded_bytes > limits.max_migration_survivor_bytes {
        bail!("current migration inventory exceeds its aggregate byte limit");
    }

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
            total_encoded_bytes = checked_inventory_bytes(
                total_encoded_bytes,
                bytes.len(),
                limits.max_migration_survivor_bytes,
            )?;
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

    let mut collision_pending = Vec::new();
    let mut collision_lifecycle_commitment = CanonicalCollisionLifecycleCommitment::new();
    walk_collision_lifecycle_records(
        paths,
        "current collision retirement",
        |project_id, bytes, record| {
            collision_lifecycle_commitment.add(&project_id, &bytes)?;
            let relevant = record.state != CollisionRetirementLifecycleStateV1::Completed
                || expected_retirement_selectors.contains(&record.selector);
            if relevant {
                if collision_pending.len() >= limits.max_migration_survivor_rows {
                    bail!("relevant collision lifecycle inventory exceeds its row limit");
                }
                total_encoded_bytes = checked_inventory_bytes(
                    total_encoded_bytes,
                    bytes.len(),
                    limits.max_migration_survivor_bytes,
                )?;
                collision_pending.push(MigrationCurrentCollisionEvidenceV1 {
                    project_id,
                    sha256: sha256_hex(&bytes),
                    bytes,
                    record,
                });
            }
            Ok(())
        },
    )?;
    let (collision_lifecycle_count, collision_lifecycle_set_sha256) =
        collision_lifecycle_commitment.finish();
    let mut authority_scopes = catalog_scopes.clone();
    authority_scopes.extend(
        effective_manifest
            .selections
            .iter()
            .map(|selection| selection.published_scope.clone()),
    );
    authority_scopes.extend(
        activations
            .iter()
            .map(|activation| activation.record.published_scope.clone()),
    );
    authority_scopes.extend(
        collision_pending
            .iter()
            .filter(|row| row.record.state != CollisionRetirementLifecycleStateV1::Completed)
            .map(|row| row.record.former_scope.clone()),
    );
    let current_root_generation_ids = activations
        .iter()
        .map(|row| row.record.generation_id.as_str())
        .chain(
            collision_pending
                .iter()
                .filter(|row| row.record.state != CollisionRetirementLifecycleStateV1::Completed)
                .map(|row| row.record.generation_id.as_str()),
        )
        .collect::<BTreeSet<_>>();

    let scopes_path = paths.root().join("scopes");
    let mut generations = Vec::new();
    walk_sha256_names_lexically(&scopes_path, "current scope", true, &mut |scope_name| {
        validate_sha256(&scope_name)?;
        let scope_path = scopes_path.join(&scope_name);
        let scope_entries = sorted_directory_entry_names(&scope_path, 1, "current scope")?;
        if scope_entries.len() != 1 || scope_entries[0] != "generations" {
            bail!("current scope directory has an incomplete or unexpected row set");
        }
        let generations_path = scope_path.join("generations");
        if NofollowDirectory::open_existing(&generations_path)?.is_none() {
            return Ok(());
        }
        let mut retained_candidates = Vec::<CurrentRetentionCandidate>::new();
        let mut retained_candidate_count = 0_usize;
        let mut retained_candidate_bytes = 0_usize;
        walk_sha256_names_lexically(
            &generations_path,
            "current generation",
            true,
            &mut |generation_id| {
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
                let manifest_path = generation_path.join("manifest.jsonl");
                match decode_stored_generation_v2_for_migration(&metadata_bytes) {
                    Ok(record) => {
                        if record.generation_id != generation_id
                            || scope_hash(&record.published_scope) != scope_name
                        {
                            bail!("current generation path and metadata disagree");
                        }
                        let (manifest_len, manifest_sha256) =
                            stream_verify_generation_manifest_for_migration(
                                &manifest_path,
                                &record.descriptor,
                                &record.producer_id,
                                &record.generation_id,
                                limits,
                            )?;
                        if !authority_scopes.contains(&record.published_scope) {
                            directory.ensure_still_current()?;
                            return Ok(());
                        }
                        let rooted = current_root_generation_ids.contains(generation_id);
                        let summary = CurrentGenerationRowSummary {
                            published_scope: record.published_scope.clone(),
                            generation_id: generation_id.to_string(),
                            generation_path: generation_path.clone(),
                            metadata_sha256: sha256_hex(&metadata_bytes),
                            metadata_bytes,
                            manifest_len,
                            manifest_sha256,
                            record,
                        };
                        if summary.record.state == GenerationState::Superseded {
                            if rooted {
                                insert_current_retention_candidate(
                                    &mut retained_candidates,
                                    CurrentRetentionCandidate {
                                        ordinal: summary.record.ordinal,
                                        generation_id: summary.generation_id.clone(),
                                        evidence: CurrentRetentionEvidence::RootMarker,
                                    },
                                    limits.retained_generations,
                                    &mut retained_candidate_count,
                                    &mut retained_candidate_bytes,
                                )?;
                            } else {
                                insert_current_retention_candidate(
                                    &mut retained_candidates,
                                    CurrentRetentionCandidate {
                                        ordinal: summary.record.ordinal,
                                        generation_id: summary.generation_id.clone(),
                                        evidence: CurrentRetentionEvidence::CurrentV2(summary),
                                    },
                                    limits.retained_generations,
                                    &mut retained_candidate_count,
                                    &mut retained_candidate_bytes,
                                )?;
                                let materialized_rows = generations
                                    .len()
                                    .checked_add(retained_candidate_count)
                                    .ok_or_else(|| {
                                        anyhow!("current protected generation count overflowed")
                                    })?;
                                if materialized_rows > limits.max_migration_survivor_rows
                                    || total_encoded_bytes
                                        .checked_add(retained_candidate_bytes)
                                        .is_none_or(|bytes| {
                                            bytes > limits.max_migration_survivor_bytes
                                        })
                                {
                                    bail!(
                                        "current retained generation inventory exceeds its configured limits"
                                    );
                                }
                                directory.ensure_still_current()?;
                                return Ok(());
                            }
                        }
                        if rooted
                            || matches!(
                                summary.record.state,
                                GenerationState::MissingBlobs
                                    | GenerationState::Ready
                                    | GenerationState::StagingIndex
                                    | GenerationState::Active
                                    | GenerationState::MissingBlobData
                            )
                        {
                            if generations
                                .len()
                                .checked_add(retained_candidate_count)
                                .is_none_or(|rows| rows >= limits.max_migration_survivor_rows)
                            {
                                bail!(
                                    "current protected generation inventory exceeds its row limit"
                                );
                            }
                            let row_bytes = summary.encoded_bytes()?;
                            if row_bytes > limits.max_migration_survivor_bytes
                                || total_encoded_bytes
                                    .checked_add(retained_candidate_bytes)
                                    .and_then(|bytes| bytes.checked_add(row_bytes))
                                    .is_none_or(|bytes| bytes > limits.max_migration_survivor_bytes)
                            {
                                bail!(
                                    "current protected generation inventory exceeds its byte limit"
                                );
                            }
                            let evidence =
                                summary.materialize(limits.max_migration_survivor_bytes)?;
                            total_encoded_bytes = checked_inventory_bytes(
                                total_encoded_bytes,
                                row_bytes,
                                limits.max_migration_survivor_bytes,
                            )?;
                            generations.push(evidence);
                        }
                    }
                    Err(v2_error) => {
                        let record = decode_stored_generation_v1_for_migration(&metadata_bytes)
                            .map_err(|_| v2_error)?;
                        if record.generation_id != generation_id
                            || scope_hash(&record.descriptor.scope) != scope_name
                        {
                            bail!("legacy leftover generation path and metadata disagree");
                        }
                        stream_verify_generation_manifest_for_migration(
                            &manifest_path,
                            &record.descriptor,
                            &record.producer_id,
                            &record.generation_id,
                            limits,
                        )?;
                        if !authority_scopes.contains(&record.descriptor.scope) {
                            directory.ensure_still_current()?;
                            return Ok(());
                        }
                        let rooted = current_root_generation_ids.contains(generation_id);
                        if rooted
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
                            insert_current_retention_candidate(
                                &mut retained_candidates,
                                CurrentRetentionCandidate {
                                    ordinal: record.ordinal,
                                    generation_id: generation_id.to_string(),
                                    evidence: CurrentRetentionEvidence::LegacyV1,
                                },
                                limits.retained_generations,
                                &mut retained_candidate_count,
                                &mut retained_candidate_bytes,
                            )?;
                        }
                    }
                }
                directory.ensure_still_current()?;
                Ok(())
            },
        )?;
        for candidate in retained_candidates {
            match candidate.evidence {
                CurrentRetentionEvidence::RootMarker => {}
                CurrentRetentionEvidence::LegacyV1 => {
                    bail!("protected retained generation keeps scopeless legacy metadata");
                }
                CurrentRetentionEvidence::CurrentV2(evidence) => {
                    if generations.len() >= limits.max_migration_survivor_rows {
                        bail!("current protected generation inventory exceeds its row limit");
                    }
                    let row_bytes = evidence.encoded_bytes()?;
                    if row_bytes > limits.max_migration_survivor_bytes {
                        bail!("current retained generation row exceeds its byte limit");
                    }
                    total_encoded_bytes = checked_inventory_bytes(
                        total_encoded_bytes,
                        row_bytes,
                        limits.max_migration_survivor_bytes,
                    )?;
                    let evidence = evidence.materialize(limits.max_migration_survivor_bytes)?;
                    generations.push(evidence);
                }
            }
        }
        Ok(())
    })?;
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
        let generation = generations_by_id.get(pending.record.generation_id.as_str());
        if pending.record.state != CollisionRetirementLifecycleStateV1::Completed
            && generation.is_none()
        {
            bail!("current collision retirement lacks generation metadata");
        }
        if let Some(generation) = generation
            && (pending.record.former_scope != generation.published_scope
                || pending.record.manifest_sha256 != generation.record.descriptor.manifest_sha256)
        {
            bail!("current collision retirement rewrites generation evidence");
        }
    }

    let retirement_path = paths.root().join("retirements");
    let mut retirements = Vec::new();
    let relevant_retirement_selectors = expected_retirement_selectors
        .iter()
        .cloned()
        .chain(
            collision_pending
                .iter()
                .filter_map(|lifecycle| lifecycle.record.exact_selector().map(str::to_string)),
        )
        .collect::<BTreeSet<_>>();
    walk_sha256_json_files_lexically(
        &retirement_path,
        "current retirement",
        &mut |selector_sha256, name| {
            let directory = NofollowDirectory::open_existing(&retirement_path)?
                .ok_or_else(|| anyhow!("current retirement directory disappeared"))?;
            let bytes = directory
                .read_regular(&name, MAX_MIGRATION_RECORD_BYTES, "current retirement")?
                .ok_or_else(|| anyhow!("current retirement disappeared"))?;
            let record: RetirementRecord =
                decode_bounded_json(&bytes, MAX_MIGRATION_RECORD_BYTES, "current retirement")?;
            validate_retirement_record(&record)?;
            if sha256_hex(record.selector.as_bytes()) != selector_sha256 {
                bail!("current retirement path and selector disagree");
            }
            if relevant_retirement_selectors.contains(&record.selector) {
                if retirements.len() >= limits.max_migration_survivor_rows {
                    bail!("relevant retirement inventory exceeds its row limit");
                }
                total_encoded_bytes = checked_inventory_bytes(
                    total_encoded_bytes,
                    bytes.len(),
                    limits.max_migration_survivor_bytes,
                )?;
                retirements.push(MigrationCurrentRetirementEvidenceV1 {
                    selector_sha256: selector_sha256.to_string(),
                    sha256: sha256_hex(&bytes),
                    bytes,
                    record,
                });
            }
            directory.ensure_still_current()?;
            Ok(())
        },
    )?;

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
        collision_lifecycle_count,
        collision_lifecycle_set_sha256,
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
    record: &CollisionRetirementLifecycleV1,
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
) -> Result<CollisionRetirementLifecycleV1> {
    let record: CollisionRetirementLifecycleV1 = decode_bounded_json(
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

    pub fn snapshot_legacy_migration_for_scopes(
        &self,
        catalog_scopes: &BTreeSet<PublishedScope>,
    ) -> Result<MigrationOwnedLegacyInventoryV1<'_>> {
        let mutation = self.lock_mutation()?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        let inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &self.paths,
            &limits,
            catalog_scopes,
        )?;
        Ok(MigrationOwnedLegacyInventoryV1 {
            inventory,
            limits,
            _mutation: mutation,
        })
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

    fn save_mixed_generation_locked(&self, generation: &MixedStoredGeneration) -> Result<()> {
        generation.validate()?;
        let path = self
            .paths
            .generation_metadata(&generation.descriptor().scope, generation.generation_id())?;
        match generation {
            MixedStoredGeneration::LegacyV1(record) => atomic_write_json(&path, record),
            MixedStoredGeneration::CurrentV2(record) => atomic_write_json(&path, record),
        }
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
        let queue_path = self.paths.retirement_for_selector(&record.selector)?;
        let project_id =
            ProjectId::parse(record.project_id.clone()).map_err(|error| anyhow!(error))?;
        let lifecycle = self.collision_lifecycle_for_project_locked(&project_id)?;
        if let Some(mut lifecycle) = lifecycle {
            if lifecycle.exact_selector() != Some(record.selector.as_str()) {
                bail!("collision retirement project lifecycle has different selector authority");
            }
            if !lifecycle.matches_queue(record) {
                bail!("collision retirement queue row rewrites lifecycle evidence");
            }
            if lifecycle.state == CollisionRetirementLifecycleStateV1::Completed {
                self.validate_lagging_collision_queue_locked(&lifecycle, &queue_path)?;
                remove_file_if_exists(&queue_path)?;
                return Ok(());
            }
            atomic_write_json(&queue_path, record)?;
            if lifecycle.state == CollisionRetirementLifecycleStateV1::Pending {
                lifecycle.state = CollisionRetirementLifecycleStateV1::Queued;
                atomic_write_json(
                    &self
                        .paths
                        .collision_retirement_pending(&lifecycle.project_id),
                    &lifecycle,
                )?;
            }
            return Ok(());
        }
        atomic_write_json(&queue_path, record)
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
        let queued = read_retirement_record_nofollow(&path)?;
        let lifecycle = if let Some(queued) = &queued {
            let project_id =
                ProjectId::parse(queued.project_id.clone()).map_err(|error| anyhow!(error))?;
            let lifecycle = self.collision_lifecycle_for_project_locked(&project_id)?;
            if lifecycle
                .as_ref()
                .is_some_and(|lifecycle| lifecycle.exact_selector() != Some(selector))
            {
                bail!("retirement queue selector and project lifecycle disagree");
            }
            lifecycle
        } else {
            self.collision_lifecycle_for_selector_locked(selector)?
        };
        if let Some(mut lifecycle) = lifecycle {
            match lifecycle.state {
                CollisionRetirementLifecycleStateV1::Pending => {
                    bail!("collision retirement cannot complete before queue publication");
                }
                CollisionRetirementLifecycleStateV1::Queued => {
                    if let Some(queued) = read_retirement_record_nofollow(&path)? {
                        if !lifecycle.matches_queue(&queued) {
                            bail!("collision retirement queue row rewrites lifecycle evidence");
                        }
                    }
                    lifecycle.state = CollisionRetirementLifecycleStateV1::Completed;
                    atomic_write_json(
                        &self
                            .paths
                            .collision_retirement_pending(&lifecycle.project_id),
                        &lifecycle,
                    )?;
                    remove_file_if_exists(&path)
                }
                CollisionRetirementLifecycleStateV1::Completed => {
                    self.validate_lagging_collision_queue_locked(&lifecycle, &path)?;
                    remove_file_if_exists(&path)
                }
            }
        } else {
            remove_file_if_exists(&path)
        }
    }

    pub fn reconcile_collision_retirements(&self) -> Result<()> {
        let _guard = self.lock_mutation()?;
        walk_collision_lifecycle_records(
            &self.paths,
            "collision retirement lifecycle",
            |_, _, mut lifecycle| {
                let Some(selector) = lifecycle.exact_selector().map(str::to_string) else {
                    if lifecycle.state == CollisionRetirementLifecycleStateV1::Pending {
                        lifecycle.state = CollisionRetirementLifecycleStateV1::Queued;
                        atomic_write_json(
                            &self
                                .paths
                                .collision_retirement_pending(&lifecycle.project_id),
                            &lifecycle,
                        )?;
                    }
                    return Ok(());
                };
                let queue = RetirementRecord {
                    version: STORE_VERSION,
                    project_id: lifecycle.project_id.to_string(),
                    selector: selector.clone(),
                    snapshot_id: lifecycle.snapshot_id.clone(),
                    generation_id: Some(lifecycle.generation_id.clone()),
                };
                validate_retirement_record(&queue)?;
                let queue_path = self.paths.retirement_for_selector(&selector)?;
                match lifecycle.state {
                    CollisionRetirementLifecycleStateV1::Pending => {
                        if let Some(existing) = read_retirement_record_nofollow(&queue_path)?
                            && !lifecycle.matches_queue(&existing)
                        {
                            bail!("collision retirement queue row rewrites lifecycle evidence");
                        }
                        atomic_write_json(&queue_path, &queue)?;
                        lifecycle.state = CollisionRetirementLifecycleStateV1::Queued;
                        atomic_write_json(
                            &self
                                .paths
                                .collision_retirement_pending(&lifecycle.project_id),
                            &lifecycle,
                        )?;
                    }
                    CollisionRetirementLifecycleStateV1::Queued => {
                        if let Some(existing) = read_retirement_record_nofollow(&queue_path)? {
                            if !lifecycle.matches_queue(&existing) {
                                bail!("collision retirement queue row rewrites lifecycle evidence");
                            }
                        } else {
                            atomic_write_json(&queue_path, &queue)?;
                        }
                    }
                    CollisionRetirementLifecycleStateV1::Completed => {
                        self.validate_lagging_collision_queue_locked(&lifecycle, &queue_path)?;
                        remove_file_if_exists(&queue_path)?;
                    }
                }
                Ok(())
            },
        )
    }

    pub fn complete_retained_collision_retirement(
        &self,
        project_id: &ProjectId,
        generation_id: &str,
    ) -> Result<()> {
        validate_sha256(generation_id)?;
        let _guard = self.lock_mutation()?;
        let Some(mut lifecycle) = self.collision_lifecycle_for_project_locked(project_id)? else {
            bail!("retained collision retirement lifecycle is missing");
        };
        if lifecycle.generation_id != generation_id
            || lifecycle.selector_evidence
                != CollisionRetirementSelectorEvidenceV1::NoDurableSelector
        {
            bail!("retained collision retirement identity or selector authority disagrees");
        }
        match lifecycle.state {
            CollisionRetirementLifecycleStateV1::Pending => {
                bail!("retained collision retirement cannot complete before queue transition");
            }
            CollisionRetirementLifecycleStateV1::Queued => {
                lifecycle.state = CollisionRetirementLifecycleStateV1::Completed;
                atomic_write_json(
                    &self
                        .paths
                        .collision_retirement_pending(&lifecycle.project_id),
                    &lifecycle,
                )
            }
            CollisionRetirementLifecycleStateV1::Completed => Ok(()),
        }
    }

    fn validate_lagging_collision_queue_locked(
        &self,
        lifecycle: &CollisionRetirementLifecycleV1,
        path: &Path,
    ) -> Result<()> {
        let Some(queued) = read_retirement_record_nofollow(path)? else {
            return Ok(());
        };
        if !lifecycle.matches_queue(&queued) {
            bail!("collision retirement queue row rewrites lifecycle evidence");
        }
        Ok(())
    }

    fn collision_lifecycle_for_selector_locked(
        &self,
        selector: &str,
    ) -> Result<Option<CollisionRetirementLifecycleV1>> {
        validate_retirement_selector(selector)?;
        let mut matched = None;
        walk_collision_lifecycle_records(
            &self.paths,
            "collision retirement lifecycle",
            |_, _, lifecycle| {
                if lifecycle.exact_selector() == Some(selector) {
                    if matched.replace(lifecycle).is_some() {
                        bail!("collision retirement selector has multiple lifecycle owners");
                    }
                }
                Ok(())
            },
        )?;
        Ok(matched)
    }

    fn collision_lifecycle_for_project_locked(
        &self,
        project_id: &ProjectId,
    ) -> Result<Option<CollisionRetirementLifecycleV1>> {
        let path = self.paths.collision_retirement_pending(project_id);
        let Some(bytes) = read_optional_regular_nofollow(
            &path,
            MAX_COLLISION_RETIREMENT_RECORD_BYTES,
            "collision retirement lifecycle",
        )?
        else {
            return Ok(None);
        };
        let lifecycle = decode_collision_retirement_pending_for_migration(&bytes)?;
        if lifecycle.project_id != *project_id {
            bail!("collision retirement lifecycle path and project disagree");
        }
        Ok(Some(lifecycle))
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
        let protected = self.protected_generation_ids(
            &generations,
            limits.retained_generations,
            &BTreeSet::new(),
        )?;
        let mut stats = MaintenanceStats::default();
        for mut generation in generations {
            if !protected.contains(generation.generation_id()) {
                continue;
            }
            let entries = self.load_generation_entries(
                &generation.descriptor().scope,
                generation.generation_id(),
            )?;
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
                generation.mark_missing_blob_data();
                self.save_mixed_generation_locked(&generation)?;
                self.update_desired_if_same_mixed(&generation)?;
                self.record_health_failure_locked(
                    &self
                        .activation_project_for_generation(generation.generation_id())?
                        .unwrap_or_else(|| scope_hash(&generation.descriptor().scope)),
                    "missing_blob_data",
                    "one or more retained source blobs failed verification",
                )?;
                stats.degraded_generations += 1;
            }
        }
        Ok(stats)
    }

    pub fn gc_blobs(&self) -> Result<MaintenanceStats> {
        self.gc_blobs_for_scopes(&BTreeSet::new())
    }

    pub fn gc_blobs_for_scopes(
        &self,
        catalog_scopes: &BTreeSet<PublishedScope>,
    ) -> Result<MaintenanceStats> {
        let _guard = self.lock_mutation()?;
        let limits = self
            .shared
            .limits
            .read()
            .map_err(|_| anyhow!("code-source limits lock poisoned"))?
            .clone();
        let generations = self.list_generations()?;
        let protected = self.protected_generation_ids(
            &generations,
            limits.retained_generations,
            catalog_scopes,
        )?;
        let mut marked = BTreeSet::new();
        for generation in &generations {
            if protected.contains(generation.generation_id()) {
                marked.extend(
                    self.load_generation_entries(
                        &generation.descriptor().scope,
                        generation.generation_id(),
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

    fn list_generations(&self) -> Result<Vec<MixedStoredGeneration>> {
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
                generations.push(read_mixed_stored_generation(
                    &generation.path().join("metadata.json"),
                )?);
            }
        }
        Ok(generations)
    }

    fn protected_generation_ids(
        &self,
        generations: &[MixedStoredGeneration],
        retained_generations: usize,
        catalog_scopes: &BTreeSet<PublishedScope>,
    ) -> Result<BTreeSet<String>> {
        let mut authority_scopes = catalog_scopes.clone();
        let mut effective_roots = BTreeMap::new();
        let mut has_current_anchor = false;
        if self.paths.anchor().is_file() {
            let bytes = fs::read(self.paths.anchor())?;
            let effective = decode_migration_effective_source_manifest_v1(&bytes)?;
            has_current_anchor = true;
            for selection in effective.selections {
                authority_scopes.insert(selection.published_scope.clone());
                if effective_roots
                    .insert(
                        selection.generation_id,
                        (selection.project_id.to_string(), selection.published_scope),
                    )
                    .is_some()
                {
                    bail!("effective source manifest contains a duplicate generation");
                }
            }
        }

        let mut activations = Vec::new();
        for activation in fs::read_dir(self.root().join("activations"))? {
            let activation = activation?;
            if activation.file_type()?.is_file() {
                let activation = read_mixed_activation(&activation.path())?;
                if let Some(scope) = activation.published_scope() {
                    authority_scopes.insert(scope.clone());
                }
                activations.push(activation);
            }
        }
        let collision_lifecycle = self.collision_retirement_pending_records_for_gc()?;
        for record in &collision_lifecycle {
            if record.state != CollisionRetirementLifecycleStateV1::Completed {
                authority_scopes.insert(record.former_scope.clone());
            }
        }

        let has_current_rows = generations
            .iter()
            .any(|record| matches!(record, MixedStoredGeneration::CurrentV2(_)))
            || activations
                .iter()
                .any(|record| matches!(record, MixedActivationRecord::CurrentV2(_)));
        if !has_current_anchor && !has_current_rows && catalog_scopes.is_empty() {
            let legacy_generations = generations
                .iter()
                .map(|record| match record {
                    MixedStoredGeneration::LegacyV1(record) => Ok(record.clone()),
                    MixedStoredGeneration::CurrentV2(_) => {
                        bail!("mixed store classification lost a current generation")
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            let legacy_activations = activations
                .iter()
                .map(|record| match record {
                    MixedActivationRecord::LegacyV1(record) => Ok(record.clone()),
                    MixedActivationRecord::CurrentV2(_) => {
                        bail!("mixed store classification lost a current activation")
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            return protected_generation_ids_from_records(
                &legacy_generations,
                &legacy_activations,
                &collision_lifecycle,
                retained_generations,
            );
        }

        mixed_protected_generation_ids_from_records(
            generations,
            &activations,
            &collision_lifecycle,
            retained_generations,
            &authority_scopes,
            &effective_roots,
        )
    }

    fn collision_retirement_pending_records_for_gc(
        &self,
    ) -> Result<Vec<CollisionRetirementLifecycleV1>> {
        let mut records = Vec::new();
        let mut commitment = CanonicalCollisionLifecycleCommitment::new();
        walk_collision_lifecycle_records(
            &self.paths,
            "collision retirement lifecycle",
            |project_id, bytes, record| {
                commitment.add(&project_id, &bytes)?;
                if record.state != CollisionRetirementLifecycleStateV1::Completed {
                    records.push(record);
                }
                Ok(())
            },
        )?;
        let (count, sha256) = commitment.finish();
        if count > 0 {
            validate_sha256(&sha256)?;
        }
        Ok(records)
    }

    fn update_desired_if_same_mixed(&self, generation: &MixedStoredGeneration) -> Result<()> {
        let desired_path = self.root().join("desired").join(format!(
            "{}.json",
            scope_hash(&generation.descriptor().scope)
        ));
        if desired_path.is_file()
            && read_mixed_stored_generation(&desired_path)?.generation_id()
                == generation.generation_id()
        {
            match generation {
                MixedStoredGeneration::LegacyV1(record) => {
                    atomic_write_json(&desired_path, record)?;
                }
                MixedStoredGeneration::CurrentV2(record) => {
                    atomic_write_json(&desired_path, record)?;
                }
            }
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
            let activation = read_mixed_activation(&entry.path())?;
            if activation.generation_id() == generation_id {
                return Ok(Some(activation.project_id().to_string()));
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
    open_regular_nofollow(path, "stored blob")
}

fn open_regular_nofollow(path: &Path, label: &str) -> Result<File> {
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
            bail!("{label} path is a symlink");
        }
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    if !file.metadata()?.file_type().is_file() {
        bail!("{label} is not a regular file");
    }
    Ok(file)
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

fn read_retirement_record_nofollow(path: &Path) -> Result<Option<RetirementRecord>> {
    let Some(bytes) =
        read_optional_regular_nofollow(path, MAX_RETIREMENT_RECORD_BYTES, "retirement queue row")?
    else {
        return Ok(None);
    };
    let record: RetirementRecord =
        decode_bounded_json(&bytes, MAX_RETIREMENT_RECORD_BYTES, "retirement queue row")?;
    validate_retirement_record(&record)?;
    Ok(Some(record))
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

fn checked_inventory_bytes(current: usize, added: usize, max_bytes: usize) -> Result<usize> {
    let total = current
        .checked_add(added)
        .ok_or_else(|| anyhow!("migration inventory byte count overflowed"))?;
    if total > max_bytes {
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

fn mixed_protected_generation_ids_from_records(
    generations: &[MixedStoredGeneration],
    activations: &[MixedActivationRecord],
    collision_lifecycle: &[CollisionRetirementLifecycleV1],
    retained_generations: usize,
    authority_scopes: &BTreeSet<PublishedScope>,
    effective_roots: &BTreeMap<String, (String, PublishedScope)>,
) -> Result<BTreeSet<String>> {
    let mut generations_by_id = BTreeMap::new();
    let mut ordinals_by_scope = BTreeSet::new();
    let mut by_scope = BTreeMap::<String, Vec<&MixedStoredGeneration>>::new();
    for generation in generations {
        generation.validate()?;
        let scope = scope_hash(&generation.descriptor().scope);
        if generations_by_id
            .insert(generation.generation_id(), generation)
            .is_some()
        {
            bail!("generation inventory contains a duplicate generation id");
        }
        if !ordinals_by_scope.insert((scope.clone(), generation.ordinal())) {
            bail!("generation inventory contains a duplicate scope ordinal");
        }
        if authority_scopes.contains(&generation.descriptor().scope) {
            by_scope.entry(scope).or_default().push(generation);
        }
    }

    let mut protected = BTreeSet::new();
    for (generation_id, (_, published_scope)) in effective_roots {
        let generation = generations_by_id
            .get(generation_id.as_str())
            .ok_or_else(|| {
                anyhow!("effective source manifest references missing generation metadata")
            })?;
        if generation.is_legacy_v1() {
            bail!("protected legacy generation lacks strict v2 ownership");
        }
        if generation.descriptor().scope != *published_scope {
            bail!("effective source manifest scope does not match generation metadata");
        }
        protected.insert(generation_id.clone());
    }

    let mut activation_projects = BTreeSet::new();
    for activation in activations {
        if !activation_projects.insert(activation.project_id()) {
            bail!("activation inventory contains a duplicate project");
        }
        let generation = generations_by_id
            .get(activation.generation_id())
            .ok_or_else(|| anyhow!("activation references missing generation metadata"))?;
        if generation.is_legacy_v1() {
            bail!("protected legacy generation lacks strict v2 ownership");
        }
        if let Some(published_scope) = activation.published_scope()
            && generation.descriptor().scope != *published_scope
        {
            bail!("activation scope does not match generation metadata");
        }
        if Some(activation.document_count()) != generation.materialized_doc_count()
            || Some(activation.entity_inventory_sha256()) != generation.entity_inventory_sha256()
        {
            bail!("activation materialization evidence does not match generation metadata");
        }
        protected.insert(activation.generation_id().to_string());
    }

    let mut lifecycle_projects = BTreeSet::new();
    for lifecycle in collision_lifecycle {
        lifecycle.validate()?;
        if !lifecycle_projects.insert(&lifecycle.project_id) {
            bail!("collision retirement inventory contains a duplicate project");
        }
        if lifecycle.state == CollisionRetirementLifecycleStateV1::Completed {
            continue;
        }
        let generation = generations_by_id
            .get(lifecycle.generation_id.as_str())
            .ok_or_else(|| {
                anyhow!("collision retirement lifecycle references missing generation metadata")
            })?;
        if generation.is_legacy_v1() {
            bail!("protected legacy generation lacks strict v2 ownership");
        }
        if lifecycle.former_scope != generation.descriptor().scope
            || lifecycle.manifest_sha256 != generation.descriptor().manifest_sha256
        {
            bail!("collision retirement lifecycle does not match generation metadata");
        }
        if lifecycle.selector_evidence == CollisionRetirementSelectorEvidenceV1::NoDurableSelector
            && (generation.state() == GenerationState::Active
                || activations.iter().any(|activation| {
                    activation.project_id() == lifecycle.project_id.as_str()
                        && activation.generation_id() == lifecycle.generation_id
                }))
        {
            bail!("retained collision lifecycle suppresses active selector authority");
        }
        protected.insert(lifecycle.generation_id.clone());
    }

    for generation in generations {
        if !authority_scopes.contains(&generation.descriptor().scope) {
            continue;
        }
        if matches!(
            generation.state(),
            GenerationState::MissingBlobs
                | GenerationState::Ready
                | GenerationState::StagingIndex
                | GenerationState::Active
                | GenerationState::MissingBlobData
        ) {
            if generation.is_legacy_v1() {
                bail!("protected legacy generation lacks strict v2 ownership");
            }
            protected.insert(generation.generation_id().to_string());
        }
    }

    for scope_generations in by_scope.values_mut() {
        scope_generations.sort_by(|left, right| {
            right
                .ordinal()
                .cmp(&left.ordinal())
                .then_with(|| left.generation_id().cmp(right.generation_id()))
        });
        for generation in scope_generations
            .iter()
            .filter(|generation| generation.state() == GenerationState::Superseded)
            .take(retained_generations)
        {
            if generation.is_legacy_v1() {
                bail!("retained legacy generation lacks strict v2 ownership");
            }
            protected.insert(generation.generation_id().to_string());
        }
    }
    Ok(protected)
}

fn protected_generation_ids_from_records(
    generations: &[StoredGeneration],
    activations: &[ActivationRecord],
    collision_pending: &[CollisionRetirementLifecycleV1],
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
        if pending.state == CollisionRetirementLifecycleStateV1::Completed {
            continue;
        }
        let generation = generations_by_id
            .get(pending.generation_id.as_str())
            .ok_or_else(|| {
                anyhow!("collision retirement lifecycle references missing generation metadata")
            })?;
        if pending.former_scope != generation.descriptor.scope
            || pending.manifest_sha256 != generation.descriptor.manifest_sha256
        {
            bail!("collision retirement lifecycle does not match generation metadata");
        }
        if pending.selector_evidence == CollisionRetirementSelectorEvidenceV1::NoDurableSelector
            && (generation.state == GenerationState::Active
                || activations.iter().any(|activation| {
                    activation.project_id.as_str() == pending.project_id.as_str()
                        && activation.generation_id == pending.generation_id
                }))
        {
            bail!("retained collision lifecycle suppresses active selector authority");
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
    field(
        &mut hasher,
        &inventory.collision_lifecycle_count.to_be_bytes(),
    );
    text(&mut hasher, &inventory.collision_lifecycle_set_sha256);
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

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
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

fn read_mixed_stored_generation(path: &Path) -> Result<MixedStoredGeneration> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > MAX_STORED_GENERATION_RECORD_BYTES {
        bail!("stored generation record exceeds its byte limit");
    }
    if let Ok(record) = decode_stored_generation_v2_for_migration(&bytes) {
        return Ok(MixedStoredGeneration::CurrentV2(record));
    }
    let record: StoredGeneration =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    validate_stored_generation_v1(&record)?;
    Ok(MixedStoredGeneration::LegacyV1(record))
}

fn read_activation_v1(path: &Path) -> Result<ActivationRecord> {
    let record = read_json(path)?;
    validate_activation_v1(&record)?;
    Ok(record)
}

fn read_mixed_activation(path: &Path) -> Result<MixedActivationRecord> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() > MAX_MIGRATION_RECORD_BYTES {
        bail!("activation record exceeds its byte limit");
    }
    if let Ok(record) = decode_activation_v2_for_migration(&bytes) {
        return Ok(MixedActivationRecord::CurrentV2(record));
    }
    let record: ActivationRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    validate_activation_v1(&record)?;
    Ok(MixedActivationRecord::LegacyV1(record))
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
    fn generation_set_evidence_requires_canonical_order_and_detects_set_changes() {
        let row = |generation_id: String| MigrationLegacyGenerationEvidenceV1 {
            published_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id,
            metadata_bytes: Vec::new(),
            metadata_sha256: "a".repeat(64),
            record: stored_generation_v1("host-a", descriptor(&[])),
            manifest_bytes: Vec::new(),
            manifest_sha256: "b".repeat(64),
        };
        let rows = [
            row("1".repeat(64)),
            row("2".repeat(64)),
            row("3".repeat(64)),
        ];
        let digest = |rows: &[MigrationLegacyGenerationEvidenceV1]| {
            let mut accumulator = CanonicalGenerationSetCommitment::new(b"test-generation-set");
            for row in rows {
                accumulator.add(row)?;
            }
            Ok::<_, anyhow::Error>(accumulator.finish())
        };
        let expected = digest(&rows).unwrap();
        let mut reordered = rows.clone();
        reordered.reverse();
        assert!(digest(&reordered).is_err());
        assert_ne!(digest(&rows[..2]).unwrap(), expected);
        let mut swapped = rows.clone();
        swapped[2] = row("4".repeat(64));
        assert_ne!(digest(&swapped).unwrap(), expected);
    }

    #[test]
    fn unprotected_history_can_exceed_the_survivor_row_cap() {
        let mut accumulator =
            CanonicalGenerationSetCommitment::new(b"test-unprotected-generation-set");
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
        validate_sha256(&accumulator.finish()).unwrap();
    }

    #[test]
    fn inventory_refuses_an_omitted_protected_survivor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let generation =
            write_legacy_generation_fixture(&paths, "host-protected", 1, GenerationState::Active);
        let guard = paths.lock_migration_inventory().unwrap();
        let mut inventory = guard
            .snapshot_legacy_v1_for_scopes(
                &StoreLimits::default(),
                &BTreeSet::from([generation.descriptor.scope.clone()]),
            )
            .unwrap();
        assert_eq!(
            inventory.protected_generation_ids,
            BTreeSet::from([generation.generation_id])
        );

        inventory.generations.clear();
        assert!(inventory.validate_evidence().is_err());
    }

    #[test]
    fn legacy_inventory_applies_owner_limits_only_to_survivors() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let scope = descriptor(&[]).scope;
        for index in 0..4 {
            write_legacy_generation_fixture(
                &paths,
                &format!("host-history-{index}"),
                index + 1,
                GenerationState::Failed,
            );
        }
        let limits = StoreLimits {
            max_migration_survivor_rows: 1,
            max_migration_survivor_bytes: 1,
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();
        let inventory = guard
            .snapshot_legacy_v1_for_scopes(&limits, &BTreeSet::from([scope]))
            .unwrap();

        assert_eq!(inventory.generation_count, 4);
        assert_eq!(inventory.unprotected_generation_count, 4);
        assert!(inventory.generations.is_empty());
    }

    #[test]
    fn legacy_inventory_refuses_low_owner_limits_for_a_protected_survivor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let generation =
            write_legacy_generation_fixture(&paths, "host-protected", 1, GenerationState::Active);
        let scopes = BTreeSet::from([generation.descriptor.scope.clone()]);
        let guard = paths.lock_migration_inventory().unwrap();

        let row_error = guard
            .snapshot_legacy_v1_for_scopes(
                &StoreLimits {
                    max_migration_survivor_rows: 0,
                    ..StoreLimits::default()
                },
                &scopes,
            )
            .unwrap_err();
        assert!(row_error.to_string().contains("row limit"));
        let byte_error = guard
            .snapshot_legacy_v1_for_scopes(
                &StoreLimits {
                    max_migration_survivor_bytes: 1,
                    ..StoreLimits::default()
                },
                &scopes,
            )
            .unwrap_err();
        assert!(byte_error.to_string().contains("byte limit"));
    }

    #[test]
    fn active_generation_outside_authority_is_an_inert_gc_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        write_legacy_generation_fixture(&paths, "host-orphan", 1, GenerationState::Active);
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard.snapshot_legacy_v1(&StoreLimits::default()).unwrap();

        assert_eq!(inventory.generation_count, 1);
        assert_eq!(inventory.unprotected_generation_count, 1);
        assert!(inventory.generations.is_empty());
        assert!(inventory.protected_generation_ids.is_empty());
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
        let protected = write_legacy_generation_fixture(
            &protected_paths,
            "host-protected-current",
            1,
            GenerationState::Active,
        );
        let guard = protected_paths.lock_migration_inventory().unwrap();
        let error = guard
            .snapshot_current_v2_for_scopes(
                &StoreLimits::default(),
                &BTreeSet::from([protected.descriptor.scope]),
                &BTreeSet::new(),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("protected current generation retains scopeless legacy metadata")
        );
    }

    #[test]
    fn current_inventory_streams_v2_history_beyond_the_survivor_cap() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        fs::create_dir_all(paths.root()).unwrap();
        let effective =
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap();
        fs::write(paths.anchor(), &effective).unwrap();
        let mut scope = None;
        for index in 0..4 {
            let legacy = write_legacy_generation_fixture(
                &paths,
                &format!("host-current-history-{index}"),
                index + 1,
                GenerationState::Failed,
            );
            scope = Some(legacy.descriptor.scope.clone());
            let current = StoredGenerationV2::from_v1_for_migration(
                legacy.clone(),
                legacy.descriptor.scope.clone(),
            )
            .unwrap();
            fs::write(
                paths
                    .generation_metadata(&legacy.descriptor.scope, &legacy.generation_id)
                    .unwrap(),
                encode_stored_generation_v2_for_migration(&current).unwrap(),
            )
            .unwrap();
        }
        let limits = StoreLimits {
            max_migration_survivor_rows: 1,
            max_migration_survivor_bytes: effective.len(),
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard
            .snapshot_current_v2_for_scopes(
                &limits,
                &BTreeSet::from([scope.unwrap()]),
                &BTreeSet::new(),
            )
            .unwrap();

        assert!(inventory.generations.is_empty());
    }

    #[test]
    fn current_inventory_does_not_charge_a_discarded_superseded_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        fs::create_dir_all(paths.root()).unwrap();
        let effective =
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap();
        fs::write(paths.anchor(), &effective).unwrap();

        let small_descriptor = descriptor(&[]);
        let mut small = stored_generation_v1("host-retained-small", small_descriptor.clone());
        small.ordinal = 2;
        small.state = GenerationState::Superseded;
        let small =
            StoredGenerationV2::from_v1_for_migration(small, small_descriptor.scope.clone())
                .unwrap();
        let large_entries = (0..128)
            .map(|index| ManifestEntry {
                relative_path: format!("src/file-{index:04}.rs"),
                content_sha256: "a".repeat(64),
                size: 1,
            })
            .collect::<Vec<_>>();
        let large_descriptor = descriptor(&large_entries);
        let mut large = (0..1_024)
            .find_map(|index| {
                let candidate = stored_generation_v1(
                    &format!("host-discarded-large-{index}"),
                    large_descriptor.clone(),
                );
                (candidate.generation_id > small.generation_id).then_some(candidate)
            })
            .expect("a lexically later discarded generation id");
        large.ordinal = 1;
        large.state = GenerationState::Superseded;
        let large =
            StoredGenerationV2::from_v1_for_migration(large, large_descriptor.scope.clone())
                .unwrap();
        let small_metadata = encode_stored_generation_v2_for_migration(&small).unwrap();
        let small_manifest = manifest_bytes(&[]);
        let large_metadata = encode_stored_generation_v2_for_migration(&large).unwrap();
        let large_manifest = manifest_bytes(&large_entries);
        for (record, metadata, manifest) in [
            (&small, &small_metadata, &small_manifest),
            (&large, &large_metadata, &large_manifest),
        ] {
            let metadata_path = paths
                .generation_metadata(&record.published_scope, &record.generation_id)
                .unwrap();
            fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
            fs::write(metadata_path, metadata).unwrap();
            fs::write(
                paths
                    .generation_manifest(&record.published_scope, &record.generation_id)
                    .unwrap(),
                manifest,
            )
            .unwrap();
        }
        let survivor_bytes = effective
            .len()
            .checked_add(small_metadata.len())
            .and_then(|bytes| bytes.checked_add(small_manifest.len()))
            .unwrap();
        assert!(large_manifest.len() > survivor_bytes);
        let limits = StoreLimits {
            retained_generations: 1,
            max_migration_survivor_rows: 1,
            max_migration_survivor_bytes: survivor_bytes,
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard
            .snapshot_current_v2_for_scopes(
                &limits,
                &BTreeSet::from([small.published_scope.clone()]),
                &BTreeSet::new(),
            )
            .unwrap();

        assert_eq!(inventory.generations.len(), 1);
        assert_eq!(inventory.generations[0].generation_id, small.generation_id);
    }

    #[test]
    fn current_retention_discards_a_lower_rank_before_inspecting_its_evidence_bytes() {
        let make_record = |producer_id: &str, ordinal| {
            let descriptor = descriptor(&[]);
            let mut legacy = stored_generation_v1(producer_id, descriptor.clone());
            legacy.ordinal = ordinal;
            legacy.state = GenerationState::Superseded;
            StoredGenerationV2::from_v1_for_migration(legacy, descriptor.scope).unwrap()
        };
        let winner_record = make_record("host-retained-winner", 2);
        let winner_generation_id = winner_record.generation_id.clone();
        let winner = CurrentRetentionCandidate {
            ordinal: 2,
            generation_id: winner_generation_id.clone(),
            evidence: CurrentRetentionEvidence::CurrentV2(CurrentGenerationRowSummary {
                published_scope: winner_record.published_scope.clone(),
                generation_id: winner_generation_id.clone(),
                generation_path: PathBuf::new(),
                metadata_bytes: vec![0; 7],
                metadata_sha256: "a".repeat(64),
                record: winner_record,
                manifest_len: 0,
                manifest_sha256: "b".repeat(64),
            }),
        };
        let mut candidates = vec![winner];
        let mut materialized_count = 1;
        let mut materialized_bytes = 7;
        let discarded_record = make_record("host-discarded-overflow", 1);
        let discarded = CurrentRetentionCandidate {
            ordinal: 1,
            generation_id: discarded_record.generation_id.clone(),
            evidence: CurrentRetentionEvidence::CurrentV2(CurrentGenerationRowSummary {
                published_scope: discarded_record.published_scope.clone(),
                generation_id: discarded_record.generation_id.clone(),
                generation_path: PathBuf::new(),
                metadata_bytes: vec![0],
                metadata_sha256: "c".repeat(64),
                record: discarded_record,
                manifest_len: usize::MAX,
                manifest_sha256: "d".repeat(64),
            }),
        };

        insert_current_retention_candidate(
            &mut candidates,
            discarded,
            1,
            &mut materialized_count,
            &mut materialized_bytes,
        )
        .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].generation_id, winner_generation_id);
        assert_eq!(materialized_count, 1);
        assert_eq!(materialized_bytes, 7);
    }

    #[test]
    fn current_inventory_refuses_a_protected_row_before_materializing_over_limit_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        fs::create_dir_all(paths.root()).unwrap();
        fs::write(
            paths.anchor(),
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let legacy =
            write_legacy_generation_fixture(&paths, "host-current", 1, GenerationState::Active);
        let current = StoredGenerationV2::from_v1_for_migration(
            legacy.clone(),
            legacy.descriptor.scope.clone(),
        )
        .unwrap();
        let metadata = encode_stored_generation_v2_for_migration(&current).unwrap();
        fs::write(
            paths
                .generation_metadata(&legacy.descriptor.scope, &legacy.generation_id)
                .unwrap(),
            &metadata,
        )
        .unwrap();
        let limits = StoreLimits {
            max_migration_survivor_bytes: metadata.len() - 1,
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();

        let error = guard
            .snapshot_current_v2_for_scopes(
                &limits,
                &BTreeSet::from([legacy.descriptor.scope]),
                &BTreeSet::new(),
            )
            .unwrap_err();

        assert!(error.to_string().contains("byte limit"));
    }

    #[test]
    fn current_inventory_streams_unrelated_completed_lifecycle_history() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("source");
        let paths = CodeSourceStorePaths::new(root).unwrap();
        fs::create_dir_all(paths.root()).unwrap();
        let effective =
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap();
        fs::write(paths.anchor(), &effective).unwrap();
        let lifecycle_directory = paths.root().join("collision-retirements");
        fs::create_dir_all(&lifecycle_directory).unwrap();
        for index in 0..8 {
            let project_id = ProjectId::parse(format!("completed-{index}")).unwrap();
            let generation_id = format!("{:064x}", index + 1);
            let lifecycle = CollisionRetirementLifecycleV1 {
                version: STORE_VERSION,
                state: CollisionRetirementLifecycleStateV1::Completed,
                project_id: project_id.clone(),
                former_scope: PublishedScope::try_new(format!("repo-{index}"), ".").unwrap(),
                generation_id: generation_id.clone(),
                selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                    materialized_selector(project_id.as_str(), &generation_id),
                ),
                snapshot_id: format!("collected-{:032x}", index + 1),
                manifest_sha256: "b".repeat(64),
                inventory_hash: "c".repeat(64),
                plan_hash: "d".repeat(64),
            };
            fs::write(
                paths.collision_retirement_pending(&project_id),
                encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
            )
            .unwrap();
        }
        let limits = StoreLimits {
            max_migration_survivor_rows: 1,
            max_migration_survivor_bytes: effective.len(),
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard.snapshot_current_v2(&limits).unwrap();

        assert!(inventory.collision_pending.is_empty());
        assert_eq!(inventory.collision_lifecycle_count, 8);
        validate_sha256(&inventory.collision_lifecycle_set_sha256).unwrap();
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
        let record = CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            state: CollisionRetirementLifecycleStateV1::Pending,
            project_id: ProjectId::parse("project-a").unwrap(),
            former_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id: "a".repeat(64),
            selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                materialized_selector("project-a", &"a".repeat(64)),
            ),
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
        invalid_selector.selector_evidence =
            CollisionRetirementSelectorEvidenceV1::ExactMaterialized("selector-a".into());
        assert!(encode_collision_retirement_pending_for_migration(&invalid_selector).is_err());
        let mut invalid_hash = record;
        invalid_hash.plan_hash = "not-a-hash".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
        invalid_hash.plan_hash = "d".repeat(64);
        invalid_hash.snapshot_id = "snapshot-a".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
    }

    fn collision_lifecycle_fixture() -> CollisionRetirementLifecycleV1 {
        CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            state: CollisionRetirementLifecycleStateV1::Pending,
            project_id: ProjectId::parse("project-a").unwrap(),
            former_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
            generation_id: "a".repeat(64),
            selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                materialized_selector("project-a", &"a".repeat(64)),
            ),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            manifest_sha256: "b".repeat(64),
            inventory_hash: "c".repeat(64),
            plan_hash: "d".repeat(64),
        }
    }

    fn retirement_for_lifecycle(lifecycle: &CollisionRetirementLifecycleV1) -> RetirementRecord {
        RetirementRecord {
            version: STORE_VERSION,
            project_id: lifecycle.project_id.to_string(),
            selector: lifecycle.exact_selector().unwrap().to_string(),
            snapshot_id: lifecycle.snapshot_id.clone(),
            generation_id: Some(lifecycle.generation_id.clone()),
        }
    }

    fn write_collision_lifecycle(
        store: &CodeSourceStore,
        lifecycle: &CollisionRetirementLifecycleV1,
    ) {
        let path = store
            .paths
            .collision_retirement_pending(&lifecycle.project_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            encode_collision_retirement_pending_for_migration(lifecycle).unwrap(),
        )
        .unwrap();
    }

    fn read_collision_lifecycle(
        store: &CodeSourceStore,
        project_id: &ProjectId,
    ) -> CollisionRetirementLifecycleV1 {
        decode_collision_retirement_pending_for_migration(
            &fs::read(store.paths.collision_retirement_pending(project_id)).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn collision_lifecycle_recovers_pending_with_published_queue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        let queue = retirement_for_lifecycle(&lifecycle);
        write_collision_lifecycle(&store, &lifecycle);
        let queue_path = store
            .paths
            .retirement_for_selector(lifecycle.exact_selector().unwrap())
            .unwrap();
        fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();

        store.enqueue_retirement(&queue).unwrap();

        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id).state,
            CollisionRetirementLifecycleStateV1::Queued
        );
        assert!(queue_path.is_file());
    }

    #[test]
    fn collision_lifecycle_completes_when_queued_row_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        lifecycle.state = CollisionRetirementLifecycleStateV1::Queued;
        write_collision_lifecycle(&store, &lifecycle);

        store
            .complete_retirement(lifecycle.exact_selector().unwrap())
            .unwrap();

        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id).state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_cleans_a_matching_lagging_queue_after_completion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        lifecycle.state = CollisionRetirementLifecycleStateV1::Completed;
        let queue = retirement_for_lifecycle(&lifecycle);
        write_collision_lifecycle(&store, &lifecycle);
        let queue_path = store
            .paths
            .retirement_for_selector(lifecycle.exact_selector().unwrap())
            .unwrap();
        fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();

        store
            .complete_retirement(lifecycle.exact_selector().unwrap())
            .unwrap();

        assert!(!queue_path.exists());
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id).state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_refuses_a_contradictory_lagging_queue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        lifecycle.state = CollisionRetirementLifecycleStateV1::Completed;
        let mut queue = retirement_for_lifecycle(&lifecycle);
        queue.snapshot_id = format!("collected-{}", "f".repeat(32));
        write_collision_lifecycle(&store, &lifecycle);
        let queue_path = store
            .paths
            .retirement_for_selector(lifecycle.exact_selector().unwrap())
            .unwrap();
        fs::write(&queue_path, serde_json::to_vec_pretty(&queue).unwrap()).unwrap();

        assert!(
            store
                .complete_retirement(lifecycle.exact_selector().unwrap())
                .is_err()
        );
        assert!(queue_path.is_file());
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id).state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_reconciliation_repairs_queued_absence_and_completed_lag() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        lifecycle.state = CollisionRetirementLifecycleStateV1::Queued;
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();

        let queue_path = store
            .paths
            .retirement_for_selector(lifecycle.exact_selector().unwrap())
            .unwrap();
        assert!(queue_path.is_file());
        lifecycle.state = CollisionRetirementLifecycleStateV1::Completed;
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();

        assert!(!queue_path.exists());
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id).state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn retained_collision_lifecycle_preserves_typed_selector_absence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        lifecycle.selector_evidence = CollisionRetirementSelectorEvidenceV1::NoDurableSelector;
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();
        let queued = read_collision_lifecycle(&store, &lifecycle.project_id);
        assert_eq!(queued.state, CollisionRetirementLifecycleStateV1::Queued);
        assert_eq!(
            queued.selector_evidence,
            CollisionRetirementSelectorEvidenceV1::NoDurableSelector
        );
        assert!(store.retirement_records().unwrap().is_empty());

        store
            .complete_retained_collision_retirement(&lifecycle.project_id, &lifecycle.generation_id)
            .unwrap();
        let completed = read_collision_lifecycle(&store, &lifecycle.project_id);
        assert_eq!(
            completed.state,
            CollisionRetirementLifecycleStateV1::Completed
        );
        assert_eq!(
            completed.selector_evidence,
            CollisionRetirementSelectorEvidenceV1::NoDurableSelector
        );
    }

    #[test]
    fn collision_lifecycle_lookup_rejects_noncanonical_and_oversized_rows() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        let queue = retirement_for_lifecycle(&lifecycle);
        let lifecycle_directory = store.root().join("collision-retirements");
        fs::create_dir_all(&lifecycle_directory).unwrap();
        fs::write(
            lifecycle_directory.join("project-a.txt"),
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();
        assert!(store.reconcile_collision_retirements().is_err());

        fs::remove_file(lifecycle_directory.join("project-a.txt")).unwrap();
        fs::write(
            lifecycle_directory.join("wrong-owner.json"),
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();
        assert!(store.reconcile_collision_retirements().is_err());

        fs::remove_file(lifecycle_directory.join("wrong-owner.json")).unwrap();
        fs::write(
            lifecycle_directory.join("project-a.json"),
            vec![b'x'; MAX_COLLISION_RETIREMENT_RECORD_BYTES + 1],
        )
        .unwrap();
        assert!(store.enqueue_retirement(&queue).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collision_lifecycle_lookup_refuses_a_symlinked_row() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        let queue = retirement_for_lifecycle(&lifecycle);
        let lifecycle_directory = store.root().join("collision-retirements");
        fs::create_dir_all(&lifecycle_directory).unwrap();
        let target = store.root().join("outside-lifecycle.json");
        fs::write(
            &target,
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();
        symlink(&target, lifecycle_directory.join("project-a.json")).unwrap();

        assert!(store.enqueue_retirement(&queue).is_err());
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
                selector: materialized_selector("project-a", &"a".repeat(64)),
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
        let pending = CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            state: CollisionRetirementLifecycleStateV1::Pending,
            project_id: project_id.clone(),
            former_scope: descriptor.scope,
            generation_id: stored.generation_id,
            selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector),
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
    fn gc_streams_completed_collision_history_without_protecting_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let lifecycle_directory = store.root().join("collision-retirements");
        fs::create_dir_all(&lifecycle_directory).unwrap();
        for index in 0..(RADIX_BUCKET_MAX_NAMES + 8) {
            let project_id = ProjectId::parse(format!("completed-gc-{index:04}")).unwrap();
            let generation_id = format!("{:064x}", index + 1);
            let lifecycle = CollisionRetirementLifecycleV1 {
                version: STORE_VERSION,
                state: CollisionRetirementLifecycleStateV1::Completed,
                project_id: project_id.clone(),
                former_scope: PublishedScope::try_new(format!("gc-repo-{index:04}"), ".").unwrap(),
                generation_id: generation_id.clone(),
                selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                    materialized_selector(project_id.as_str(), &generation_id),
                ),
                snapshot_id: format!("collected-{:032x}", index + 1),
                manifest_sha256: "b".repeat(64),
                inventory_hash: "c".repeat(64),
                plan_hash: "d".repeat(64),
            };
            fs::write(
                store.paths.collision_retirement_pending(&project_id),
                encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
            )
            .unwrap();
        }
        let orphan_bytes = b"completed history orphan";
        let orphan_hash = sha256_hex(orphan_bytes);
        let orphan_path = store.blob_path(&orphan_hash);
        fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
        fs::write(&orphan_path, orphan_bytes).unwrap();

        let stats = store.gc_blobs().unwrap();

        assert!(!orphan_path.exists());
        assert_eq!(stats.reclaimed_blobs, 1);
    }

    #[test]
    fn post_migration_gc_classifies_mixed_v1_v2_generations_without_reviving_history() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.retained_generations = 0;
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let protected_bytes = b"protected current generation";
        let protected_hash = sha256_hex(protected_bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: protected_hash.clone(),
            size: protected_bytes.len() as u64,
        }];
        let current_descriptor = descriptor(&entries);
        let legacy = stored_generation_v1("host-current", current_descriptor.clone());
        let current =
            StoredGenerationV2::from_v1_for_migration(legacy, current_descriptor.scope.clone())
                .unwrap();
        let generation_directory = store
            .paths
            .generation_metadata(&current_descriptor.scope, &current.generation_id)
            .unwrap();
        fs::create_dir_all(generation_directory.parent().unwrap()).unwrap();
        fs::write(
            &generation_directory,
            encode_stored_generation_v2_for_migration(&current).unwrap(),
        )
        .unwrap();
        fs::write(
            store
                .paths
                .generation_manifest(&current_descriptor.scope, &current.generation_id)
                .unwrap(),
            manifest_bytes(&entries),
        )
        .unwrap();
        let activation = ActivationRecordV2::from_v1_for_migration(
            activation_v1(&current.generation_id),
            &current,
        )
        .unwrap();
        fs::write(
            store.paths.activation(&activation.project_id),
            encode_activation_v2_for_migration(&activation).unwrap(),
        )
        .unwrap();
        fs::write(
            store.paths.anchor(),
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: vec![MigrationEffectiveSourceSelectionV1 {
                    project_id: activation.project_id.clone(),
                    published_scope: current_descriptor.scope.clone(),
                    generation_id: current.generation_id.clone(),
                    selector: activation.selector.clone(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let inert_bytes = b"inert legacy history blob";
        let inert_hash = sha256_hex(inert_bytes);
        let inert_entries = vec![ManifestEntry {
            relative_path: "src/history.rs".into(),
            content_sha256: inert_hash.clone(),
            size: inert_bytes.len() as u64,
        }];
        let inert_descriptor = descriptor(&inert_entries);
        let mut inert_generation = stored_generation_v1("host-leftover", inert_descriptor.clone());
        inert_generation.ordinal = 2;
        inert_generation.state = GenerationState::Failed;
        inert_generation.materialized_doc_count = None;
        inert_generation.entity_inventory_sha256 = None;
        let inert_metadata = store
            .paths
            .generation_metadata(&inert_descriptor.scope, &inert_generation.generation_id)
            .unwrap();
        fs::create_dir_all(inert_metadata.parent().unwrap()).unwrap();
        fs::write(
            inert_metadata,
            serde_json::to_vec_pretty(&inert_generation).unwrap(),
        )
        .unwrap();
        fs::write(
            store
                .paths
                .generation_manifest(&inert_descriptor.scope, &inert_generation.generation_id)
                .unwrap(),
            manifest_bytes(&inert_entries),
        )
        .unwrap();
        let protected_path = store.blob_path(&protected_hash);
        fs::create_dir_all(protected_path.parent().unwrap()).unwrap();
        fs::write(&protected_path, protected_bytes).unwrap();
        let inert_path = store.blob_path(&inert_hash);
        fs::create_dir_all(inert_path.parent().unwrap()).unwrap();
        fs::write(&inert_path, inert_bytes).unwrap();

        let stats = store
            .gc_blobs_for_scopes(&BTreeSet::from([current_descriptor.scope]))
            .unwrap();

        assert!(protected_path.is_file());
        assert!(!inert_path.exists());
        assert_eq!(stats.reclaimed_blobs, 1);
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
            .record_materialization(&descriptor.scope, &ready.generation_id, 1, "c".repeat(64))
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
