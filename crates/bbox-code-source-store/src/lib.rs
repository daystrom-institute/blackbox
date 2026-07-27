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
    BeginUploadResponse, CutbackStateV2, DEFAULT_MAX_MANIFEST_FILES,
    DEFAULT_MAX_MANIFEST_LOGICAL_BYTES, GenerationDescriptor, GenerationState, GenerationStatus,
    MAX_MANIFEST_PAGE_ENTRIES, ManifestEntry, MissingBlobsPage, generation_id, scope_hash,
    validate_collected_materialization_selector, validate_manifest, validate_producer_id,
    validate_sha256,
};
#[cfg(test)]
use bbox_code_source::{CutbackErrorClass, CutbackReason};
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
pub const MIGRATION_STORE_VERSION: u32 = 2;
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
pub const MAX_COLLISION_RETIREMENT_ENTRIES: usize = MAX_PROJECT_CATALOG_ENTRIES;

/// The record codec the runtime expects on this store.
///
/// Set at open time from the server-crate authority selection (section 4.9):
/// the store crate never imports `ProjectAuthority`; the mode reaches it as
/// this enum. Catalog APIs accept only strict v2 protected records; bridge
/// wrappers retain v1 signatures and bytes. The default for the legacy
/// `open` entry point is [`RuntimeRecordMode::BridgeV1`] so bridge behavior
/// is byte-identical until the server crate selects catalog mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeRecordMode {
    /// Legacy version-1 records (bridge daemon).
    BridgeV1,
    /// Strict scope-bearing version-2 records (catalog mode).
    CatalogV2,
}

impl Default for RuntimeRecordMode {
    fn default() -> Self {
        Self::BridgeV1
    }
}

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

/// Producer-correctable upload failures surfaced across the store boundary.
///
/// Durable IO and stored-state failures remain ordinary `anyhow::Error`
/// values. Callers can therefore distinguish request semantics without
/// parsing messages or accidentally exposing store paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreRequestError {
    LimitExceeded,
    TooManyOpenUploads,
    InvalidState,
    InvalidInput,
}

impl std::fmt::Display for StoreRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::LimitExceeded => "code-source input exceeds an enforced limit",
            Self::TooManyOpenUploads => "producer has too many open uploads",
            Self::InvalidState => "upload is not in the required state",
            Self::InvalidInput => "code-source input is invalid",
        })
    }
}

impl std::error::Error for StoreRequestError {}

struct SharedStoreState {
    limits: RwLock<StoreLimits>,
    mutation: Mutex<()>,
    record_mode: RuntimeRecordMode,
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

    pub fn collision_retirement_work(
        &self,
        project_id: &ProjectId,
        generation_id: &str,
    ) -> Result<PathBuf> {
        validate_sha256(generation_id)?;
        Ok(self.root.join("collision-retirement-work").join(format!(
            "{}.json",
            collision_retirement_work_id(project_id, generation_id)
        )))
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
        expected_collision_generations: &BTreeSet<(ProjectId, String)>,
    ) -> Result<MigrationCurrentInventoryV1> {
        enumerate_current_migration_inventory_for_scopes_locked(
            self.paths,
            limits,
            catalog_scopes,
            expected_retirement_selectors,
            expected_collision_generations,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementGenerationInventory {
    pub published_scope: PublishedScope,
    pub generation_id: String,
    pub blob_hashes: BTreeSet<String>,
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
pub enum MixedStoredGeneration {
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

    pub fn generation_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.generation_id,
            Self::CurrentV2(record) => &record.generation_id,
        }
    }

    pub fn producer_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.producer_id,
            Self::CurrentV2(record) => &record.producer_id,
        }
    }

    fn ordinal(&self) -> u64 {
        match self {
            Self::LegacyV1(record) => record.ordinal,
            Self::CurrentV2(record) => record.ordinal,
        }
    }

    pub fn descriptor(&self) -> &GenerationDescriptor {
        match self {
            Self::LegacyV1(record) => &record.descriptor,
            Self::CurrentV2(record) => &record.descriptor,
        }
    }

    pub fn state(&self) -> GenerationState {
        match self {
            Self::LegacyV1(record) => record.state,
            Self::CurrentV2(record) => record.state,
        }
    }

    pub fn published_scope(&self) -> &PublishedScope {
        match self {
            Self::LegacyV1(record) => &record.descriptor.scope,
            Self::CurrentV2(record) => &record.published_scope,
        }
    }

    pub fn materialized_doc_count(&self) -> Option<u64> {
        match self {
            Self::LegacyV1(record) => record.materialized_doc_count,
            Self::CurrentV2(record) => record.materialized_doc_count,
        }
    }

    pub fn entity_inventory_sha256(&self) -> Option<&str> {
        match self {
            Self::LegacyV1(record) => record.entity_inventory_sha256.as_deref(),
            Self::CurrentV2(record) => record.entity_inventory_sha256.as_deref(),
        }
    }

    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::LegacyV1(record) => record.diagnostic.as_deref(),
            Self::CurrentV2(record) => record.diagnostic.as_deref(),
        }
    }

    fn is_legacy_v1(&self) -> bool {
        matches!(self, Self::LegacyV1(_))
    }

    pub fn is_current_v2(&self) -> bool {
        matches!(self, Self::CurrentV2(_))
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
pub enum MixedActivationRecord {
    LegacyV1(ActivationRecord),
    CurrentV2(ActivationRecordV2),
}

impl MixedActivationRecord {
    pub fn project_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.project_id,
            Self::CurrentV2(record) => record.project_id.as_str(),
        }
    }

    pub fn generation_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.generation_id,
            Self::CurrentV2(record) => &record.generation_id,
        }
    }

    pub fn published_scope(&self) -> Option<&PublishedScope> {
        match self {
            Self::LegacyV1(_) => None,
            Self::CurrentV2(record) => Some(&record.published_scope),
        }
    }

    pub fn document_count(&self) -> u64 {
        match self {
            Self::LegacyV1(record) => record.document_count,
            Self::CurrentV2(record) => record.document_count,
        }
    }

    pub fn entity_inventory_sha256(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.entity_inventory_sha256,
            Self::CurrentV2(record) => &record.entity_inventory_sha256,
        }
    }

    pub fn selector(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.selector,
            Self::CurrentV2(record) => &record.selector,
        }
    }

    pub fn snapshot_id(&self) -> &str {
        match self {
            Self::LegacyV1(record) => &record.snapshot_id,
            Self::CurrentV2(record) => &record.snapshot_id,
        }
    }

    pub fn activated_unix_secs(&self) -> u64 {
        match self {
            Self::LegacyV1(record) => record.activated_unix_secs,
            Self::CurrentV2(record) => record.activated_unix_secs,
        }
    }

    /// The typed cutback state on a catalog-mode (v2) record, or `None` for
    /// bridge (v1) records which carry only the derived boolean mirror.
    pub fn cutback(&self) -> Option<&CutbackStateV2> {
        match self {
            Self::LegacyV1(_) => None,
            Self::CurrentV2(record) => record.cutback.as_ref(),
        }
    }

    /// True when the record is a v2 (catalog-mode) activation record.
    pub fn is_current_v2(&self) -> bool {
        matches!(self, Self::CurrentV2(_))
    }

    /// The derived `cutback_pending` mirror (section 4.10). True when
    /// `cutback` is `Some` and not `Terminal`.
    pub fn is_cutback_pending(&self) -> bool {
        match self {
            Self::CurrentV2(record) => record.cutback_pending,
            Self::LegacyV1(_) => false,
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
    /// The typed cutback state authority (section 4.10). `cutback_pending`
    /// is the derived mirror; this field is the sole authority for live
    /// writers. Defaulted to `None` so migration bytes written without the
    /// field decode cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutback: Option<CutbackStateV2>,
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
            cutback: None,
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
        // Typed cutback state validates per-variant (section 5.2 item 3).
        if let Some(cutback) = self.cutback.as_ref() {
            cutback
                .validate()
                .map_err(|error| anyhow!("error.code_source_cutback_state: {error}"))?;
        }
        // Layered coherence clause (section 4.10): cutback_pending is the
        // derived mirror of the typed cutback field. Store-level validate
        // is pure and load-time, so it ADMITS the legacy-migration shape
        // (cutback: None, cutback_pending: true) left by the migration
        // writer before once-only startup classification. The sole refuser
        // of that shape is the startup relationship chain (section 10.2
        // step 6). Live writers never emit it. A typed cutback field whose
        // derived mirror disagrees fails closed here with a coherence
        // error.
        let derived_pending = match self.cutback.as_ref() {
            Some(CutbackStateV2::Terminal { .. }) | None => false,
            Some(_) => true,
        };
        if self.cutback.is_some() && derived_pending != self.cutback_pending {
            bail!(
                "error.code_source_cutback_coherence: cutback_pending mirror does not match typed cutback"
            );
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
    pub project_id: ProjectId,
    pub entries: BTreeMap<String, CollisionRetirementEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollisionRetirementEntryV1 {
    pub state: CollisionRetirementLifecycleStateV1,
    pub former_scope: PublishedScope,
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
        if self.entries.is_empty() || self.entries.len() > MAX_COLLISION_RETIREMENT_ENTRIES {
            bail!("collision retirement lifecycle entry count is invalid");
        }
        for (generation_id, entry) in &self.entries {
            entry.validate(&self.project_id, generation_id)?;
        }
        Ok(())
    }

    pub fn entry(&self, generation_id: &str) -> Result<&CollisionRetirementEntryV1> {
        validate_sha256(generation_id)?;
        self.entries
            .get(generation_id)
            .ok_or_else(|| anyhow!("collision retirement generation is absent"))
    }

    pub fn validate_transition_from(&self, previous: &Self) -> Result<()> {
        self.validate()?;
        previous.validate()?;
        if self.project_id != previous.project_id
            || self.entries.keys().collect::<Vec<_>>()
                != previous.entries.keys().collect::<Vec<_>>()
        {
            bail!("collision retirement lifecycle membership changed");
        }
        for (generation_id, entry) in &self.entries {
            let previous_entry = &previous.entries[generation_id];
            entry.validate_transition_from(previous_entry)?;
        }
        Ok(())
    }

    pub fn validate_descendant_from(&self, previous: &Self) -> Result<()> {
        self.validate()?;
        previous.validate()?;
        if self.project_id != previous.project_id
            || self.entries.keys().collect::<Vec<_>>()
                != previous.entries.keys().collect::<Vec<_>>()
        {
            bail!("collision retirement lifecycle membership changed");
        }
        for (generation_id, entry) in &self.entries {
            let previous_entry = &previous.entries[generation_id];
            if !entry.immutable_evidence_eq(previous_entry)
                || collision_retirement_state_rank(entry.state)
                    < collision_retirement_state_rank(previous_entry.state)
            {
                bail!("collision retirement lifecycle evidence changed or state regressed");
            }
        }
        Ok(())
    }
}

fn collision_retirement_state_rank(state: CollisionRetirementLifecycleStateV1) -> u8 {
    match state {
        CollisionRetirementLifecycleStateV1::Pending => 0,
        CollisionRetirementLifecycleStateV1::Queued => 1,
        CollisionRetirementLifecycleStateV1::Completed => 2,
    }
}

impl CollisionRetirementEntryV1 {
    fn validate(&self, project_id: &ProjectId, generation_id: &str) -> Result<()> {
        self.former_scope.validate()?;
        validate_sha256(generation_id)?;
        if let CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) =
            &self.selector_evidence
        {
            validate_retirement_selector(selector)?;
            validate_collected_materialization_selector(
                project_id.as_str(),
                generation_id,
                selector,
            )?;
        }
        validate_migration_snapshot_id(&self.snapshot_id)?;
        validate_sha256(&self.manifest_sha256)?;
        validate_sha256(&self.inventory_hash)?;
        validate_sha256(&self.plan_hash)?;
        Ok(())
    }

    fn immutable_evidence_eq(&self, other: &Self) -> bool {
        self.former_scope == other.former_scope
            && self.selector_evidence == other.selector_evidence
            && self.snapshot_id == other.snapshot_id
            && self.manifest_sha256 == other.manifest_sha256
            && self.inventory_hash == other.inventory_hash
            && self.plan_hash == other.plan_hash
    }

    fn validate_transition_from(&self, previous: &Self) -> Result<()> {
        if !self.immutable_evidence_eq(previous) {
            bail!("collision retirement lifecycle evidence changed");
        }
        let monotonic = matches!(
            (previous.state, self.state),
            (
                CollisionRetirementLifecycleStateV1::Pending,
                CollisionRetirementLifecycleStateV1::Pending
                    | CollisionRetirementLifecycleStateV1::Queued
            ) | (
                CollisionRetirementLifecycleStateV1::Queued,
                CollisionRetirementLifecycleStateV1::Queued
                    | CollisionRetirementLifecycleStateV1::Completed
            ) | (
                CollisionRetirementLifecycleStateV1::Completed,
                CollisionRetirementLifecycleStateV1::Completed
            )
        );
        if !monotonic {
            bail!("collision retirement lifecycle state regressed");
        }
        Ok(())
    }

    pub fn exact_selector(&self) -> Option<&str> {
        match &self.selector_evidence {
            CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) => Some(selector),
            CollisionRetirementSelectorEvidenceV1::NoDurableSelector => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollisionRetirementWorkV1 {
    pub version: u32,
    pub project_id: ProjectId,
    pub generation_id: String,
    pub former_scope: PublishedScope,
    pub selector_evidence: CollisionRetirementSelectorEvidenceV1,
    pub snapshot_id: String,
    pub manifest_sha256: String,
    pub inventory_hash: String,
    pub plan_hash: String,
}

impl CollisionRetirementWorkV1 {
    pub fn validate(&self) -> Result<()> {
        if self.version != STORE_VERSION {
            bail!("invalid collision retirement work version");
        }
        ProjectId::parse(self.project_id.to_string()).map_err(|error| anyhow!(error))?;
        let entry = self.as_entry(CollisionRetirementLifecycleStateV1::Queued);
        entry.validate(&self.project_id, &self.generation_id)
    }

    fn from_entry(
        project_id: &ProjectId,
        generation_id: &str,
        entry: &CollisionRetirementEntryV1,
    ) -> Self {
        Self {
            version: STORE_VERSION,
            project_id: project_id.clone(),
            generation_id: generation_id.to_string(),
            former_scope: entry.former_scope.clone(),
            selector_evidence: entry.selector_evidence.clone(),
            snapshot_id: entry.snapshot_id.clone(),
            manifest_sha256: entry.manifest_sha256.clone(),
            inventory_hash: entry.inventory_hash.clone(),
            plan_hash: entry.plan_hash.clone(),
        }
    }

    fn as_entry(&self, state: CollisionRetirementLifecycleStateV1) -> CollisionRetirementEntryV1 {
        CollisionRetirementEntryV1 {
            state,
            former_scope: self.former_scope.clone(),
            selector_evidence: self.selector_evidence.clone(),
            snapshot_id: self.snapshot_id.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            inventory_hash: self.inventory_hash.clone(),
            plan_hash: self.plan_hash.clone(),
        }
    }

    fn matches_entry(&self, entry: &CollisionRetirementEntryV1) -> bool {
        self.as_entry(entry.state).immutable_evidence_eq(entry)
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
pub struct MigrationCurrentCollisionWorkEvidenceV1 {
    pub work_id: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub record: CollisionRetirementWorkV1,
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
    pub collision_work: Vec<MigrationCurrentCollisionWorkEvidenceV1>,
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
            // A crash between `atomic_write`'s staging write and its rename
            // leaves a `<key>.<uuid>.tmp` orphan beside the canonical rows;
            // it is debris, not corruption, and must not hard-fail every
            // subsequent walk (the M8 defect family). Any OTHER
            // non-canonical name still fails closed.
            if name.ends_with(".tmp") {
                continue;
            }
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
                || record
                    .entries
                    .values()
                    .any(|entry| entry.state != CollisionRetirementLifecycleStateV1::Pending)
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
                .flat_map(|row| row.record.entries.keys().cloned()),
        )
        .collect::<BTreeSet<_>>();
    let mut authority_scopes = catalog_scopes.clone();
    authority_scopes.extend(collision_pending.iter().flat_map(|row| {
        row.record
            .entries
            .values()
            .map(|entry| entry.former_scope.clone())
    }));
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
        &BTreeSet::new(),
    )
}

pub fn enumerate_current_migration_inventory_for_scopes_locked(
    paths: &CodeSourceStorePaths,
    limits: &StoreLimits,
    catalog_scopes: &BTreeSet<PublishedScope>,
    expected_retirement_selectors: &BTreeSet<String>,
    expected_collision_generations: &BTreeSet<(ProjectId, String)>,
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
            let relevant = record.entries.iter().any(|(generation_id, entry)| {
                entry.state != CollisionRetirementLifecycleStateV1::Completed
                    || expected_collision_generations
                        .contains(&(project_id.clone(), generation_id.clone()))
            });
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
    authority_scopes.extend(collision_pending.iter().flat_map(|row| {
        row.record
            .entries
            .values()
            .filter(|entry| entry.state != CollisionRetirementLifecycleStateV1::Completed)
            .map(|entry| entry.former_scope.clone())
    }));
    let current_root_generation_ids = activations
        .iter()
        .map(|row| row.record.generation_id.as_str())
        .chain(collision_pending.iter().flat_map(|row| {
            row.record
                .entries
                .iter()
                .filter_map(|(generation_id, entry)| {
                    (entry.state != CollisionRetirementLifecycleStateV1::Completed)
                        .then_some(generation_id.as_str())
                })
        }))
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
        for (generation_id, entry) in &pending.record.entries {
            let generation = generations_by_id.get(generation_id.as_str());
            if entry.state != CollisionRetirementLifecycleStateV1::Completed && generation.is_none()
            {
                bail!("current collision retirement lacks generation metadata");
            }
            if let Some(generation) = generation
                && (entry.former_scope != generation.published_scope
                    || entry.manifest_sha256 != generation.record.descriptor.manifest_sha256)
            {
                bail!("current collision retirement rewrites generation evidence");
            }
        }
    }

    let collision_entries = collision_pending
        .iter()
        .flat_map(|lifecycle| {
            lifecycle
                .record
                .entries
                .iter()
                .map(|(generation_id, entry)| {
                    (
                        (lifecycle.project_id.clone(), generation_id.as_str()),
                        entry,
                    )
                })
        })
        .collect::<BTreeMap<_, _>>();
    let collision_work_path = paths.root().join("collision-retirement-work");
    let mut collision_work = Vec::new();
    walk_sha256_json_files_lexically(
        &collision_work_path,
        "current collision retirement work",
        &mut |work_id, name| {
            let directory = NofollowDirectory::open_existing(&collision_work_path)?
                .ok_or_else(|| anyhow!("current collision work directory disappeared"))?;
            let bytes = directory
                .read_regular(
                    &name,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "current collision retirement work",
                )?
                .ok_or_else(|| anyhow!("current collision retirement work disappeared"))?;
            let record = decode_collision_retirement_work(&bytes)?;
            if collision_retirement_work_id(&record.project_id, &record.generation_id) != work_id {
                bail!("current collision work path and identity disagree");
            }
            let included_entry =
                collision_entries.get(&(record.project_id.clone(), record.generation_id.as_str()));
            let matches_entry = if let Some(entry) = included_entry {
                record.matches_entry(entry)
            } else {
                let lifecycle_path = paths.collision_retirement_pending(&record.project_id);
                let lifecycle_bytes = read_optional_regular_nofollow(
                    &lifecycle_path,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "current collision retirement lifecycle",
                )?
                .ok_or_else(|| anyhow!("current collision work lacks lifecycle document"))?;
                let lifecycle =
                    decode_collision_retirement_pending_for_migration(&lifecycle_bytes)?;
                lifecycle.project_id == record.project_id
                    && lifecycle
                        .entries
                        .get(&record.generation_id)
                        .is_some_and(|entry| {
                            entry.state == CollisionRetirementLifecycleStateV1::Completed
                                && record.matches_entry(entry)
                        })
            };
            if !matches_entry {
                bail!("current collision work rewrites lifecycle evidence");
            }
            if collision_work.len() >= limits.max_migration_survivor_rows {
                bail!("current collision work inventory exceeds its row limit");
            }
            total_encoded_bytes = checked_inventory_bytes(
                total_encoded_bytes,
                bytes.len(),
                limits.max_migration_survivor_bytes,
            )?;
            collision_work.push(MigrationCurrentCollisionWorkEvidenceV1 {
                work_id: work_id.to_string(),
                sha256: sha256_hex(&bytes),
                bytes,
                record,
            });
            directory.ensure_still_current()?;
            Ok(())
        },
    )?;

    let retirement_path = paths.root().join("retirements");
    let mut retirements = Vec::new();
    let relevant_retirement_selectors = expected_retirement_selectors.clone();
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
        collision_work,
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

fn decode_collision_retirement_work(bytes: &[u8]) -> Result<CollisionRetirementWorkV1> {
    let record: CollisionRetirementWorkV1 = decode_bounded_json(
        bytes,
        MAX_COLLISION_RETIREMENT_RECORD_BYTES,
        "collision retirement work",
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
    pub reclaimed_generations: u64,
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
        Self::open_with_mode(root, limits, RuntimeRecordMode::BridgeV1)
    }

    /// Open a store with an explicit record mode (section 4.9).
    ///
    /// The server crate derives the mode from the `ProjectAuthority`
    /// selection and passes it here; the store never imports that type.
    /// Bridge callers keep using [`Self::open`], which defaults to v1.
    pub fn open_with_mode(
        root: impl Into<PathBuf>,
        limits: StoreLimits,
        record_mode: RuntimeRecordMode,
    ) -> Result<Self> {
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
            "collision-retirements",
            "collision-retirement-work",
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
        Self::from_existing_paths(paths, limits, record_mode)
    }

    /// Open an existing migration owner without creating its root, lock, or
    /// conventional child directories.
    ///
    /// Normal daemon startup uses [`Self::open`] to initialize a new store.
    /// Migration preflight must instead prove that the owner already exists
    /// and was initialized by that path before it may take the existing owner
    /// lock and enumerate exact source bytes.
    pub fn open_existing_for_migration(
        root: impl Into<PathBuf>,
        limits: StoreLimits,
    ) -> Result<Option<Self>> {
        let paths = CodeSourceStorePaths::new(root)?;
        let metadata = match fs::symlink_metadata(paths.root()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspecting existing code-source store root"),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("existing code-source store root is not a safe directory");
        }
        let lock_path = bbox_corpus_core::json_store::canonical_store_lock_path(&paths.anchor());
        let lock_metadata = fs::symlink_metadata(&lock_path)
            .context("inspecting existing code-source store lock")?;
        if lock_metadata.file_type().is_symlink() || !lock_metadata.is_file() {
            bail!("existing code-source store lock is not a safe regular file");
        }
        let root = paths
            .root()
            .canonicalize()
            .context("canonicalizing existing code-source store root")?;
        Self::from_existing_paths(
            CodeSourceStorePaths::new(root)?,
            limits,
            RuntimeRecordMode::default(),
        )
        .map(Some)
    }

    fn from_existing_paths(
        paths: CodeSourceStorePaths,
        limits: StoreLimits,
        record_mode: RuntimeRecordMode,
    ) -> Result<Self> {
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
                    record_mode,
                    verified_blobs: Mutex::new(HashMap::new()),
                    #[cfg(test)]
                    blob_verifications: AtomicU64::new(0),
                });
                registry.insert(paths.root().to_path_buf(), Arc::downgrade(&shared));
                shared
            });
        Ok(Self { paths, shared })
    }

    /// The record codec this store was opened with (section 4.9).
    pub fn record_mode(&self) -> RuntimeRecordMode {
        self.shared.record_mode
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

    /// Read the current store limits (section 10.2 link 4: bounded
    /// manifest verification needs the limits for entry/byte caps).
    pub fn limits(&self) -> StoreLimits {
        self.shared
            .limits
            .read()
            .map(|l| l.clone())
            .unwrap_or_default()
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
            return Err(StoreRequestError::LimitExceeded.into());
        }
        if descriptor.logical_bytes > limits.max_manifest_logical_bytes {
            return Err(StoreRequestError::LimitExceeded.into());
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
            return Err(StoreRequestError::TooManyOpenUploads.into());
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
            return Err(StoreRequestError::InvalidInput.into());
        }
        if entries.len() > MAX_MANIFEST_PAGE_ENTRIES {
            return Err(StoreRequestError::LimitExceeded.into());
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
            return Err(StoreRequestError::InvalidState.into());
        }
        let raw = serde_json::to_vec(entries)?;
        if raw.len() > bbox_code_source::MAX_MANIFEST_PAGE_BYTES {
            return Err(StoreRequestError::LimitExceeded.into());
        }
        let digest = sha256_hex(&raw);
        if page < record.next_page {
            if record.page_digests.get(&page) == Some(&digest) {
                return Ok(());
            }
            return Err(StoreRequestError::InvalidInput.into());
        }
        if page != record.next_page {
            return Err(StoreRequestError::InvalidInput.into());
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
            return Err(StoreRequestError::InvalidInput.into());
        }
        if received_logical_bytes > record.descriptor.logical_bytes {
            return Err(StoreRequestError::InvalidInput.into());
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
            return Err(StoreRequestError::InvalidInput.into());
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
            return Err(StoreRequestError::InvalidState.into());
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
                return Err(StoreRequestError::InvalidInput.into());
            }
        } else {
            write_manifest_jsonl(&manifest_path, &entries)?;
        }
        let metadata_path = generation_dir.join("metadata.json");
        if metadata_path.is_file() {
            let mixed = read_mixed_stored_generation(&metadata_path)?;
            let (stored_producer, stored_descriptor) = match &mixed {
                MixedStoredGeneration::LegacyV1(record) => {
                    (&record.producer_id, &record.descriptor)
                }
                MixedStoredGeneration::CurrentV2(record) => {
                    (&record.producer_id, &record.descriptor)
                }
            };
            if stored_producer != producer_id || stored_descriptor != &record.descriptor {
                return Err(StoreRequestError::InvalidInput.into());
            }
        } else {
            let created_unix_secs = now_unix_secs();
            if self.shared.record_mode == RuntimeRecordMode::CatalogV2 {
                let stored_v2 = StoredGenerationV2 {
                    version: MIGRATION_STORE_VERSION,
                    generation_id: generation.clone(),
                    producer_id: producer_id.to_string(),
                    ordinal: record.ordinal,
                    descriptor: record.descriptor.clone(),
                    published_scope: record.descriptor.scope.clone(),
                    state: GenerationState::MissingBlobs,
                    diagnostic: None,
                    created_unix_secs,
                    materialized_doc_count: None,
                    entity_inventory_sha256: None,
                };
                atomic_write_json(&metadata_path, &stored_v2)?;
            } else {
                let stored = StoredGeneration {
                    version: STORE_VERSION,
                    generation_id: generation.clone(),
                    producer_id: producer_id.to_string(),
                    ordinal: record.ordinal,
                    descriptor: record.descriptor.clone(),
                    state: GenerationState::MissingBlobs,
                    diagnostic: None,
                    created_unix_secs,
                    materialized_doc_count: None,
                    entity_inventory_sha256: None,
                };
                atomic_write_json(&metadata_path, &stored)?;
            }
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
            return Err(StoreRequestError::InvalidState.into());
        }
        let generation = record
            .generation_id
            .as_deref()
            .ok_or(StoreRequestError::InvalidState)?;
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
            return Err(StoreRequestError::InvalidInput.into());
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
                return Err(StoreRequestError::LimitExceeded.into());
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
        }
        if written != expected_size || hex::encode(hasher.finalize()) != expected_hash {
            drop(file);
            let _ = fs::remove_file(&temporary);
            return Err(StoreRequestError::InvalidInput.into());
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
            .ok_or(StoreRequestError::InvalidState)?;
        let entries = self.load_generation_entries(&record.descriptor.scope, &generation)?;
        let missing = self.missing_hashes(&entries)?;
        if !missing.is_empty() {
            return Err(StoreRequestError::InvalidState.into());
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
                // Generation metadata is only rewritable while its immutable
                // manifest survives. Writing metadata for a reclaimed
                // generation would recreate the directory without the
                // manifest, and that manifest-less row is retention-protected
                // as the newest superseded generation forever after.
                if self
                    .paths
                    .generation_manifest(&previous.descriptor.scope, &previous.generation_id)?
                    .is_file()
                {
                    previous.state = GenerationState::Superseded;
                    previous.diagnostic = None;
                    self.save_generation_locked(&previous)?;
                } else {
                    tracing_reclaimed_desired_generation(&previous.generation_id);
                }
            }
            atomic_write_json(&desired_path, &stored)?;
        }
        Ok(stored)
    }

    /// Mode-aware upload finalization (section 7.1 item 3).
    ///
    /// In catalog mode (`RuntimeRecordMode::CatalogV2`), the writer reads,
    /// mutates, and writes `StoredGenerationV2` records, including the
    /// desired pointer. In bridge mode, it delegates to the existing
    /// v1 `finalize_upload`. The caller receives the `MixedStoredGeneration`
    /// so it can branch on the record shape without a separate read.
    pub fn finalize_upload_mixed(
        &self,
        producer_id: &str,
        upload_id: &str,
    ) -> Result<MixedStoredGeneration> {
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            let stored = self.finalize_upload(producer_id, upload_id)?;
            return Ok(MixedStoredGeneration::LegacyV1(stored));
        }

        let _guard = self.lock_mutation()?;
        let mut record = self.load_upload(producer_id, upload_id)?;
        let generation = record
            .generation_id
            .clone()
            .ok_or(StoreRequestError::InvalidState)?;
        let entries = self.load_generation_entries(&record.descriptor.scope, &generation)?;
        let missing = self.missing_hashes(&entries)?;
        if !missing.is_empty() {
            return Err(StoreRequestError::InvalidState.into());
        }
        let mut stored = match self.load_generation_mixed(&record.descriptor.scope, &generation)? {
            MixedStoredGeneration::CurrentV2(rec) => rec,
            MixedStoredGeneration::LegacyV1(_) => {
                bail!("error.code_source_record_mode: catalog store found a v1 stored generation")
            }
        };
        let desired_path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(&record.descriptor.scope)));
        let previous_desired = if desired_path.is_file() {
            match read_mixed_stored_generation(&desired_path)? {
                MixedStoredGeneration::CurrentV2(rec) => Some(rec),
                MixedStoredGeneration::LegacyV1(_) => {
                    bail!(
                        "error.code_source_record_mode: catalog desired pointer is a v1 stored generation"
                    )
                }
            }
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
        self.save_generation_v2_locked(&stored)?;
        record.state = stored.state;
        record.updated_unix_secs = now_unix_secs();
        self.save_upload(&record)?;
        if !superseded {
            if let Some(mut previous) = previous_desired
                && previous.generation_id != stored.generation_id
                && !self.generation_is_activated(&previous.generation_id)?
            {
                if self
                    .paths
                    .generation_manifest(&previous.published_scope, &previous.generation_id)?
                    .is_file()
                {
                    previous.state = GenerationState::Superseded;
                    previous.diagnostic = None;
                    self.save_generation_v2_locked(&previous)?;
                } else {
                    tracing_reclaimed_desired_generation(&previous.generation_id);
                }
            }
            atomic_write_json(&desired_path, &stored)?;
        }
        Ok(MixedStoredGeneration::CurrentV2(stored))
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
            return Err(StoreRequestError::InvalidState.into());
        }
        let generation = record
            .generation_id
            .as_deref()
            .ok_or(StoreRequestError::InvalidState)?;
        let entries = self.load_generation_entries(&record.descriptor.scope, generation)?;
        let mut sizes = entries
            .iter()
            .filter(|entry| entry.content_sha256 == hash)
            .map(|entry| entry.size);
        let size = sizes.next().ok_or(StoreRequestError::InvalidInput)?;
        if sizes.any(|other| other != size) {
            return Err(StoreRequestError::InvalidInput.into());
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

    /// Mode-aware generation-state transition (section 7.1 item 3).
    ///
    /// In catalog mode the writer reads, mutates, and writes the v2 record
    /// (including the desired pointer). In bridge mode it delegates to the
    /// existing v1 `mark_generation_state`. The caller receives the
    /// `MixedStoredGeneration` so it can branch on the record shape without a
    /// separate read.
    pub fn mark_generation_state_mixed(
        &self,
        scope: &PublishedScope,
        generation: &str,
        state: GenerationState,
        diagnostic: Option<String>,
    ) -> Result<MixedStoredGeneration> {
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            let stored = self.mark_generation_state(scope, generation, state, diagnostic)?;
            return Ok(MixedStoredGeneration::LegacyV1(stored));
        }
        let _guard = self.lock_mutation()?;
        let mut stored = match self.load_generation_mixed(scope, generation)? {
            MixedStoredGeneration::CurrentV2(record) => record,
            MixedStoredGeneration::LegacyV1(_) => {
                bail!("error.code_source_record_mode: catalog store found a v1 stored generation")
            }
        };
        stored.state = state;
        stored.diagnostic = diagnostic.map(|value| value.chars().take(512).collect());
        self.save_generation_v2_locked(&stored)?;
        let desired_path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if desired_path.is_file() {
            let desired = read_mixed_stored_generation(&desired_path)?;
            if desired.generation_id() == generation {
                atomic_write_json(&desired_path, &stored)?;
            }
        }
        Ok(MixedStoredGeneration::CurrentV2(stored))
    }

    /// Mode-aware materialization writer (section 7.1 item 3).
    ///
    /// Catalog mode reads, mutates, and writes the v2 record. Bridge mode
    /// delegates to the existing v1 `record_materialization`.
    pub fn record_materialization_mixed(
        &self,
        scope: &PublishedScope,
        generation: &str,
        document_count: u64,
        entity_inventory_sha256: String,
    ) -> Result<MixedStoredGeneration> {
        validate_sha256(&entity_inventory_sha256)?;
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            let stored = self.record_materialization(
                scope,
                generation,
                document_count,
                entity_inventory_sha256,
            )?;
            return Ok(MixedStoredGeneration::LegacyV1(stored));
        }
        let _guard = self.lock_mutation()?;
        let mut stored = match self.load_generation_mixed(scope, generation)? {
            MixedStoredGeneration::CurrentV2(record) => record,
            MixedStoredGeneration::LegacyV1(_) => {
                bail!("error.code_source_record_mode: catalog store found a v1 stored generation")
            }
        };
        stored.materialized_doc_count = Some(document_count);
        stored.entity_inventory_sha256 = Some(entity_inventory_sha256);
        self.save_generation_v2_locked(&stored)?;
        Ok(MixedStoredGeneration::CurrentV2(stored))
    }

    fn save_generation_v2_locked(&self, generation: &StoredGenerationV2) -> Result<()> {
        generation.validate()?;
        atomic_write_json(
            &self
                .paths
                .generation_metadata(&generation.published_scope, &generation.generation_id)?,
            generation,
        )
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
            if entry.file_type()?.is_file() && is_canonical_record_file(&entry) {
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

    // ---- Catalog-mode (v2) activation and cutback writers/readers (P4-A) ----

    /// Persist a catalog-mode v2 activation record through the mutation mutex
    /// and anchor lock via `atomic_write_json` to the existing
    /// `activations/<project_id>.json` path (section 5.2 item 5).
    ///
    /// Refuses on a bridge-mode store (section 4.9): catalog APIs accept
    /// only strict v2 protected records, and a bridge store must never
    /// accept v2 writes even though its read paths already refuse them.
    pub fn save_activation_v2(&self, activation: &ActivationRecordV2) -> Result<()> {
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            bail!("error.code_source_record_mode: bridge store refuses v2 activation writes");
        }
        let _guard = self.lock_mutation()?;
        self.save_activation_v2_locked(activation)
    }

    fn save_activation_v2_locked(&self, activation: &ActivationRecordV2) -> Result<()> {
        activation.validate()?;
        atomic_write_json(&self.paths.activation(&activation.project_id), activation)
    }

    /// Mode-aware single-project activation read (section 5.2 item 5, 7.2).
    ///
    /// In catalog mode the reader decodes the v2 record. In bridge mode it
    /// decodes the v1 record and REFUSES v2 bytes with
    /// `error.code_source_record_mode` rather than silently rewriting them
    /// as v1 (store AGENTS.md invariant). Returns `Ok(None)` when the file
    /// is absent.
    pub fn load_activation_mixed(&self, project_id: &str) -> Result<Option<MixedActivationRecord>> {
        let path = self.paths.activation_for_str(project_id)?;
        if !path.is_file() {
            return Ok(None);
        }
        let mixed = read_mixed_activation(&path)?;
        match (self.shared.record_mode, &mixed) {
            (RuntimeRecordMode::CatalogV2, MixedActivationRecord::LegacyV1(_)) => {
                // Catalog mode found a v1 record: treat as absent so the
                // caller's "no recovery record" path fires rather than a
                // decode-mismatch error (section 7.2).
                Ok(None)
            }
            (RuntimeRecordMode::BridgeV1, MixedActivationRecord::CurrentV2(_)) => Err(anyhow!(
                "error.code_source_record_mode: bridge read path refuses a v2 activation record"
            )),
            _ => {
                if mixed.project_id() != project_id {
                    bail!("activation record identity mismatch");
                }
                Ok(Some(mixed))
            }
        }
    }

    /// Mode-aware activation enumeration (section 5.2 item 5, 7.2).
    pub fn activation_records_mixed(&self) -> Result<Vec<MixedActivationRecord>> {
        let mut records: Vec<MixedActivationRecord> = Vec::new();
        for entry in fs::read_dir(self.root().join("activations"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() && is_canonical_record_file(&entry) {
                let mixed = read_mixed_activation(&entry.path())?;
                match (self.shared.record_mode, &mixed) {
                    (RuntimeRecordMode::CatalogV2, MixedActivationRecord::LegacyV1(_)) => {
                        continue;
                    }
                    (RuntimeRecordMode::BridgeV1, MixedActivationRecord::CurrentV2(_)) => {
                        bail!(
                            "error.code_source_record_mode: bridge read path refuses a v2 activation record"
                        );
                    }
                    _ => records.push(mixed),
                }
            }
        }
        records.sort_by(|left, right| left.project_id().cmp(right.project_id()));
        Ok(records)
    }

    /// Mode-aware generation lookup by id across all scopes (section 7.2).
    pub fn find_generation_mixed(&self, generation: &str) -> Result<MixedStoredGeneration> {
        validate_sha256(generation)?;
        for scope_entry in fs::read_dir(self.root().join("scopes"))? {
            let metadata = scope_entry?
                .path()
                .join("generations")
                .join(generation)
                .join("metadata.json");
            if metadata.is_file() {
                let mixed = read_mixed_stored_generation(&metadata)?;
                match (self.shared.record_mode, &mixed) {
                    (RuntimeRecordMode::CatalogV2, MixedStoredGeneration::LegacyV1(_)) => {
                        continue;
                    }
                    (RuntimeRecordMode::BridgeV1, MixedStoredGeneration::CurrentV2(_)) => {
                        bail!(
                            "error.code_source_record_mode: bridge read path refuses a v2 stored generation"
                        );
                    }
                    _ => return Ok(mixed),
                }
            }
        }
        bail!("generation not found")
    }

    /// Mode-aware generation load by scope and id (section 7.2).
    pub fn load_generation_mixed(
        &self,
        scope: &PublishedScope,
        generation: &str,
    ) -> Result<MixedStoredGeneration> {
        let mixed =
            read_mixed_stored_generation(&self.paths.generation_metadata(scope, generation)?)?;
        match (self.shared.record_mode, &mixed) {
            (RuntimeRecordMode::CatalogV2, MixedStoredGeneration::LegacyV1(_)) => {
                bail!(
                    "error.code_source_record_mode: catalog read path found a v1 stored generation"
                );
            }
            (RuntimeRecordMode::BridgeV1, MixedStoredGeneration::CurrentV2(_)) => {
                bail!(
                    "error.code_source_record_mode: bridge read path refuses a v2 stored generation"
                );
            }
            _ => Ok(mixed),
        }
    }

    /// Write the typed cutback state onto the project's v2 activation record
    /// under catalog mode, updating both `cutback` and the derived
    /// `cutback_pending` mirror in one atomic write (section 5.2 item 5).
    ///
    /// Refuses on a bridge-mode store (section 4.9) and bails with a typed
    /// error when no activation record exists for the project: the reducer
    /// only marks cutback state on projects with an active collected
    /// activation, so a missing record is a lost-record invariant violation.
    pub fn mark_cutback_state(&self, project_id: &str, cutback: CutbackStateV2) -> Result<()> {
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            bail!("error.code_source_record_mode: bridge store refuses v2 activation writes");
        }
        let _guard = self.lock_mutation()?;
        cutback
            .validate()
            .map_err(|error| anyhow!("error.code_source_cutback_state: {error}"))?;
        let Some(mut record) = self.load_activation_v2_locked(project_id)? else {
            bail!("error.code_source_cutback_state: no activation record for project {project_id}");
        };
        let derived_pending = !matches!(cutback, CutbackStateV2::Terminal { .. });
        record.cutback = Some(cutback);
        record.cutback_pending = derived_pending;
        self.save_activation_v2_locked(&record)
    }

    /// Mode-aware cutback-pending marker (section 9.1 step c).
    ///
    /// Sets `cutback_pending = true` and a diagnostic string on the
    /// activation record. In catalog mode this writes through the v2
    /// activation path; in bridge mode it falls back to the v1 record
    /// (the only mode where mark_cutback_pending is valid).
    pub fn mark_cutback_pending_mixed(&self, project_id: &str, diagnostic: &str) -> Result<()> {
        if self.shared.record_mode != RuntimeRecordMode::BridgeV1 {
            // Catalog v2 path
            let _guard = self.lock_mutation()?;
            let Some(mut record) = self.load_activation_v2_locked(project_id)? else {
                return Ok(());
            };
            record.cutback_pending = true;
            record.diagnostic = Some(diagnostic.chars().take(512).collect());
            self.save_activation_v2_locked(&record)
        } else {
            // Bridge v1 path (legacy)
            self.mark_cutback_pending(project_id, diagnostic)
        }
    }

    /// Clear the typed cutback state on a project's activation record
    /// (section 9.1 step e: success clears state). Sets `cutback` to
    /// `None` and `cutback_pending` to `false` (the coherence clause,
    /// section 4.10). Catalog mode only.
    pub fn clear_cutback_state(&self, project_id: &str) -> Result<()> {
        if self.shared.record_mode == RuntimeRecordMode::BridgeV1 {
            bail!("error.code_source_record_mode: bridge store refuses v2 activation writes");
        }
        let _guard = self.lock_mutation()?;
        let Some(mut record) = self.load_activation_v2_locked(project_id)? else {
            return Ok(());
        };
        record.cutback = None;
        record.cutback_pending = false;
        self.save_activation_v2_locked(&record)
    }

    fn load_activation_v2_locked(&self, project_id: &str) -> Result<Option<ActivationRecordV2>> {
        let path = self.paths.activation_for_str(project_id)?;
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        if bytes.len() > MAX_MIGRATION_RECORD_BYTES {
            bail!("activation record exceeds its byte limit");
        }
        let record: ActivationRecordV2 = decode_activation_v2_for_migration(&bytes)?;
        if record.project_id.as_str() != project_id {
            bail!("activation record identity mismatch");
        }
        Ok(Some(record))
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
            if entry.file_type()?.is_file() && is_canonical_record_file(&entry) {
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
        atomic_write_json(&queue_path, record)
    }

    pub fn retirement_records(&self) -> Result<Vec<RetirementRecord>> {
        let mut records = Vec::new();
        for entry in fs::read_dir(self.root().join("retirements"))? {
            let entry = entry?;
            if entry.file_type()?.is_file() && is_canonical_record_file(&entry) {
                records.push(read_json(&entry.path())?);
            }
        }
        Ok(records)
    }

    pub fn retirement_pending(&self, selector: &str) -> Result<bool> {
        let path = self.paths.retirement_for_selector(selector)?;
        Ok(read_retirement_record_nofollow(&path)?.is_some())
    }

    pub fn complete_retirement(&self, record: &RetirementRecord) -> Result<()> {
        let _guard = self.lock_mutation()?;
        validate_retirement_record(record)?;
        let path = self.paths.retirement_for_selector(&record.selector)?;
        let Some(queued) = read_retirement_record_nofollow(&path)? else {
            return Ok(());
        };
        if queued != *record {
            bail!("code-source retirement queue row changed before completion");
        }
        if let Some(generation_id) = &record.generation_id {
            let mut generation = self.find_generation(generation_id)?;
            let is_still_desired = self
                .desired_generation(&generation.descriptor.scope)?
                .is_some_and(|desired| desired.generation_id == generation_id.as_str());
            if !is_still_desired && generation.state != GenerationState::Failed {
                generation.state = GenerationState::Superseded;
                generation.diagnostic = None;
                self.save_generation_locked(&generation)?;
            }
        }
        remove_file_if_exists(&path)
    }

    pub fn reconcile_collision_retirements(&self) -> Result<()> {
        let _guard = self.lock_mutation()?;
        walk_collision_lifecycle_records(
            &self.paths,
            "collision retirement lifecycle",
            |_, _, mut lifecycle| {
                let previous = lifecycle.clone();
                for (generation_id, entry) in &mut lifecycle.entries {
                    let work = CollisionRetirementWorkV1::from_entry(
                        &lifecycle.project_id,
                        generation_id,
                        entry,
                    );
                    work.validate()?;
                    let work_path = self
                        .paths
                        .collision_retirement_work(&lifecycle.project_id, generation_id)?;
                    let existing = read_collision_retirement_work_nofollow(&work_path)?;
                    if existing.as_ref().is_some_and(|existing| existing != &work) {
                        bail!("collision retirement work row rewrites lifecycle evidence");
                    }
                    match entry.state {
                        CollisionRetirementLifecycleStateV1::Pending => {
                            if existing.is_none() {
                                atomic_write_json(&work_path, &work)?;
                            }
                            entry.state = CollisionRetirementLifecycleStateV1::Queued;
                        }
                        CollisionRetirementLifecycleStateV1::Queued => {
                            if existing.is_none() {
                                bail!("queued collision retirement lacks its work row");
                            }
                        }
                        CollisionRetirementLifecycleStateV1::Completed => {
                            remove_file_if_exists(&work_path)?;
                        }
                    }
                }
                lifecycle.validate_transition_from(&previous)?;
                if lifecycle != previous {
                    atomic_write_json(
                        &self
                            .paths
                            .collision_retirement_pending(&lifecycle.project_id),
                        &lifecycle,
                    )?;
                }
                Ok(())
            },
        )
    }

    pub fn collision_retirement_work_records(&self) -> Result<Vec<CollisionRetirementWorkV1>> {
        let _guard = self.lock_mutation()?;
        let mut lifecycle_entries = BTreeMap::new();
        walk_collision_lifecycle_records(
            &self.paths,
            "collision retirement lifecycle",
            |_, _, lifecycle| {
                for (generation_id, entry) in lifecycle.entries {
                    let identity = (lifecycle.project_id.clone(), generation_id);
                    if lifecycle_entries.insert(identity, entry).is_some() {
                        bail!("collision retirement lifecycle identity is duplicated");
                    }
                }
                Ok(())
            },
        )?;
        let mut missing_queued_work = lifecycle_entries
            .iter()
            .filter_map(|(identity, entry)| {
                (entry.state == CollisionRetirementLifecycleStateV1::Queued)
                    .then_some(identity.clone())
            })
            .collect::<BTreeSet<_>>();
        let path = self.root().join("collision-retirement-work");
        let Some(directory) = NofollowDirectory::open_existing(&path)? else {
            if missing_queued_work.is_empty() {
                return Ok(Vec::new());
            }
            bail!("queued collision retirement lacks its work row");
        };
        let mut records = Vec::new();
        let mut completed_lag_paths = Vec::new();
        for name in sorted_regular_entry_names(
            &path,
            MAX_MIGRATION_INVENTORY_GENERATIONS,
            "collision retirement work",
        )? {
            let work_id = name
                .strip_suffix(".json")
                .ok_or_else(|| anyhow!("collision retirement work filename is not canonical"))?;
            validate_sha256(work_id)?;
            let bytes = directory
                .read_regular(
                    &name,
                    MAX_COLLISION_RETIREMENT_RECORD_BYTES,
                    "collision retirement work",
                )?
                .ok_or_else(|| anyhow!("collision retirement work disappeared"))?;
            let record = decode_collision_retirement_work(&bytes)?;
            if collision_retirement_work_id(&record.project_id, &record.generation_id) != work_id {
                bail!("collision retirement work path and identity disagree");
            }
            let identity = (record.project_id.clone(), record.generation_id.clone());
            let entry = lifecycle_entries
                .get(&identity)
                .ok_or_else(|| anyhow!("collision retirement work row is orphaned"))?;
            if !record.matches_entry(entry) {
                bail!("collision retirement work row rewrites lifecycle evidence");
            }
            match entry.state {
                CollisionRetirementLifecycleStateV1::Pending => {
                    bail!("collision retirement work precedes its queued lifecycle state");
                }
                CollisionRetirementLifecycleStateV1::Queued => {
                    if !missing_queued_work.remove(&identity) {
                        bail!("collision retirement work identity is duplicated");
                    }
                    records.push(record);
                }
                CollisionRetirementLifecycleStateV1::Completed => {
                    completed_lag_paths.push(path.join(&name));
                }
            }
        }
        directory.ensure_still_current()?;
        if !missing_queued_work.is_empty() {
            bail!("queued collision retirement lacks its work row");
        }
        for completed_lag_path in completed_lag_paths {
            remove_file_if_exists(&completed_lag_path)?;
        }
        records.sort_by(|left, right| {
            left.project_id
                .cmp(&right.project_id)
                .then_with(|| left.generation_id.cmp(&right.generation_id))
        });
        Ok(records)
    }

    pub fn repair_and_complete_collision_retirement(
        &self,
        project_id: &ProjectId,
        generation_id: &str,
    ) -> Result<()> {
        validate_sha256(generation_id)?;
        let _guard = self.lock_mutation()?;
        let mut lifecycle = self
            .collision_lifecycle_for_project_locked(project_id)?
            .ok_or_else(|| anyhow!("collision retirement lifecycle is missing"))?;
        let previous = lifecycle.clone();
        let entry = lifecycle
            .entries
            .get(generation_id)
            .ok_or_else(|| anyhow!("collision retirement generation is absent"))?;
        if entry.state != CollisionRetirementLifecycleStateV1::Queued {
            bail!("collision retirement terminal transition requires queued lifecycle state");
        }
        let work_path = self
            .paths
            .collision_retirement_work(project_id, generation_id)?;
        let work = read_collision_retirement_work_nofollow(&work_path)?
            .ok_or_else(|| anyhow!("queued collision retirement work row is missing"))?;
        if !work.matches_entry(entry) {
            bail!("collision retirement work row rewrites lifecycle evidence");
        }

        let activation_path = self.paths.activation(project_id);
        if activation_path.is_file() {
            let activation = read_mixed_activation(&activation_path)?;
            if activation.project_id() != project_id.as_str() {
                bail!("collision retirement activation identity disagrees");
            }
            if activation.generation_id() == generation_id {
                bail!("collision retirement generation remains active");
            }
        }

        let metadata_path = self
            .paths
            .generation_metadata(&entry.former_scope, generation_id)?;
        let mut stored = match read_mixed_stored_generation(&metadata_path)? {
            MixedStoredGeneration::LegacyV1(_) => {
                bail!("collision retirement refuses legacy generation metadata")
            }
            MixedStoredGeneration::CurrentV2(record) => record,
        };
        if stored.generation_id != generation_id
            || stored.published_scope != entry.former_scope
            || stored.descriptor.scope != entry.former_scope
        {
            bail!("collision retirement generation metadata disagrees with lifecycle evidence");
        }
        stored.state = GenerationState::Superseded;
        stored.diagnostic = None;
        let stored = MixedStoredGeneration::CurrentV2(stored);
        self.save_mixed_generation_locked(&stored)?;
        self.update_desired_if_same_mixed(&stored)?;
        if stored.state() != GenerationState::Superseded {
            bail!("collision retirement generation did not reach superseded state");
        }

        let entry = lifecycle
            .entries
            .get_mut(generation_id)
            .ok_or_else(|| anyhow!("collision retirement generation is absent"))?;
        entry.state = CollisionRetirementLifecycleStateV1::Completed;
        lifecycle.validate_transition_from(&previous)?;
        atomic_write_json(
            &self
                .paths
                .collision_retirement_pending(&lifecycle.project_id),
            &lifecycle,
        )?;
        remove_file_if_exists(&work_path)
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

    /// Enumerate the exact stored generations and blob references used by
    /// offline project retirement. The mutation lock keeps metadata and
    /// manifests coherent for the complete snapshot.
    pub fn retirement_generation_inventory(&self) -> Result<Vec<RetirementGenerationInventory>> {
        let _guard = self.lock_mutation()?;
        let mut inventory = Vec::new();
        for generation in self.list_generations()? {
            generation.validate()?;
            let published_scope = generation.published_scope().clone();
            let generation_id = generation.generation_id().to_string();
            let blob_hashes = self
                .load_generation_entries(&published_scope, &generation_id)?
                .into_iter()
                .map(|entry| entry.content_sha256)
                .collect();
            inventory.push(RetirementGenerationInventory {
                published_scope,
                generation_id,
                blob_hashes,
            });
        }
        inventory.sort_by(|left, right| {
            scope_hash(&left.published_scope)
                .cmp(&scope_hash(&right.published_scope))
                .then_with(|| left.generation_id.cmp(&right.generation_id))
        });
        Ok(inventory)
    }

    /// Delete one owner-validated generation by exact scope and generation
    /// identity. Missing generations are an idempotent success.
    pub fn delete_retirement_generation(
        &self,
        scope: &PublishedScope,
        generation_id: &str,
    ) -> Result<()> {
        validate_sha256(generation_id)?;
        let _guard = self.lock_mutation()?;
        let directory = self.paths.generation_directory(scope, generation_id)?;
        if !directory.exists() {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("generation path is not a regular directory");
        }
        let stored = read_mixed_stored_generation(&directory.join("metadata.json"))?;
        stored.validate()?;
        if stored.generation_id() != generation_id || stored.published_scope() != scope {
            bail!("generation metadata does not match exact retirement identity");
        }
        fs::remove_dir_all(&directory)?;
        sync_parent(&directory)?;
        Ok(())
    }

    pub fn retirement_generation_exists(
        &self,
        scope: &PublishedScope,
        generation_id: &str,
    ) -> Result<bool> {
        validate_sha256(generation_id)?;
        let directory = self.paths.generation_directory(scope, generation_id)?;
        if !directory.exists() {
            return Ok(false);
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("generation path is not a regular directory");
        }
        let stored = read_mixed_stored_generation(&directory.join("metadata.json"))?;
        stored.validate()?;
        if stored.generation_id() != generation_id || stored.published_scope() != scope {
            bail!("generation metadata does not match exact retirement identity");
        }
        Ok(true)
    }

    /// Delete candidate blobs only when no remaining generation manifest
    /// references them. Missing blobs are an idempotent success.
    pub fn sweep_retirement_blobs(&self, candidates: &BTreeSet<String>) -> Result<()> {
        for hash in candidates {
            validate_sha256(hash)?;
        }
        let _guard = self.lock_mutation()?;
        let mut retained = BTreeSet::new();
        for generation in self.list_generations()? {
            generation.validate()?;
            for entry in self
                .load_generation_entries(generation.published_scope(), generation.generation_id())?
            {
                if candidates.contains(&entry.content_sha256) {
                    retained.insert(entry.content_sha256);
                }
            }
        }
        for hash in candidates.difference(&retained) {
            let path = self.blob_path(hash);
            match fs::remove_file(&path) {
                Ok(()) => sync_parent(&path)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Read the raw manifest.jsonl bytes for a generation (section 10.2
    /// link 4: bounded manifest verification). Returns the file bytes
    /// for digest verification and entry validation.
    ///
    /// Uses a bounded O_NOFOLLOW descriptor read on Unix to prevent
    /// symlink following and resource exhaustion (R2F3). The file size
    /// is checked before allocation against the store limits.
    pub fn read_generation_manifest_bytes(
        &self,
        scope: &PublishedScope,
        generation: &str,
    ) -> Result<Vec<u8>> {
        let path = self.paths.generation_manifest(scope, generation)?;
        let limits = self.limits();
        // Bounded nofollow read (R2F3).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let metadata = std::fs::metadata(&path)
                .with_context(|| format!("stat manifest at {}", path.display()))?;
            let size = metadata.len() as usize;
            if size > limits.max_manifest_logical_bytes as usize {
                anyhow::bail!(
                    "manifest at {} exceeds byte limit ({} > {})",
                    path.display(),
                    size,
                    limits.max_manifest_logical_bytes
                );
            }
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .with_context(|| format!("opening manifest at {}", path.display()))?;
            let mut buf = Vec::with_capacity(size);
            std::io::Read::read_to_end(&mut file, &mut buf)
                .with_context(|| format!("reading manifest at {}", path.display()))?;
            Ok(buf)
        }
        #[cfg(not(unix))]
        {
            let metadata = std::fs::metadata(&path)
                .with_context(|| format!("stat manifest at {}", path.display()))?;
            let size = metadata.len() as usize;
            if size > limits.max_manifest_logical_bytes as usize {
                anyhow::bail!(
                    "manifest at {} exceeds byte limit ({} > {})",
                    path.display(),
                    size,
                    limits.max_manifest_logical_bytes
                );
            }
            std::fs::read(&path).with_context(|| format!("reading manifest at {}", path.display()))
        }
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

    /// Mode-aware desired-generation read (section 7.1 item 3).
    ///
    /// In catalog mode the reader decodes the v2 pointer; in bridge mode it
    /// decodes the v1 pointer and REFUSES v2 bytes with
    /// `error.code_source_record_mode`. Returns `Ok(None)` when the pointer
    /// file is absent.
    pub fn desired_generation_mixed(
        &self,
        scope: &PublishedScope,
    ) -> Result<Option<MixedStoredGeneration>> {
        let path = self
            .root()
            .join("desired")
            .join(format!("{}.json", scope_hash(scope)));
        if !path.is_file() {
            return Ok(None);
        }
        let mixed = read_mixed_stored_generation(&path)?;
        match (self.shared.record_mode, &mixed) {
            (RuntimeRecordMode::CatalogV2, MixedStoredGeneration::LegacyV1(_)) => {
                bail!(
                    "error.code_source_record_mode: catalog read path found a v1 stored generation"
                );
            }
            (RuntimeRecordMode::BridgeV1, MixedStoredGeneration::CurrentV2(_)) => {
                bail!(
                    "error.code_source_record_mode: bridge read path refuses a v2 stored generation"
                );
            }
            _ => Ok(Some(mixed)),
        }
    }

    /// Generation ids named by a `desired/<scope>.json` pointer.
    ///
    /// A desired pointer is a GC root whatever state its generation carries.
    /// Reclaiming a still-desired generation leaves the pointer dangling, and
    /// the next publication then resurrects that generation's metadata without
    /// its immutable manifest.
    fn desired_generation_ids(&self) -> Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        for entry in fs::read_dir(self.root().join("desired"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || !is_canonical_record_file(&entry) {
                continue;
            }
            ids.insert(
                read_mixed_stored_generation(&entry.path())?
                    .generation_id()
                    .to_string(),
            );
        }
        Ok(ids)
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
        self.gc_blobs_for_scopes_with_bridge(&BTreeSet::new(), &BTreeSet::new())
    }

    /// Backwards-compatible variant: no bridge generation ids to protect.
    pub fn gc_blobs_for_scopes(
        &self,
        catalog_scopes: &BTreeSet<PublishedScope>,
    ) -> Result<MaintenanceStats> {
        self.gc_blobs_for_scopes_with_bridge(catalog_scopes, &BTreeSet::new())
    }

    /// Blob GC with catalog scopes and open-bridge generation ids
    /// (section 9.5 GC root).
    ///
    /// `bridge_generation_ids` carries the set of non-null
    /// `code_bridge_generation` values from the catalog's
    /// `scope_migrations`. Each is a GC root: the bridge holds the
    /// named generation alive until the first new-scope activation
    /// retires it or a scope-bridge-clear removes the reference.
    pub fn gc_blobs_for_scopes_with_bridge(
        &self,
        catalog_scopes: &BTreeSet<PublishedScope>,
        bridge_generation_ids: &BTreeSet<String>,
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
            bridge_generation_ids,
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
        for generation in &generations {
            if protected.contains(generation.generation_id()) {
                continue;
            }
            let directory = self
                .paths
                .generation_directory(&generation.descriptor().scope, generation.generation_id())?;
            let metadata = match fs::symlink_metadata(&directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_dir()
                || metadata.modified().unwrap_or(SystemTime::now()) > cutoff
            {
                continue;
            }
            fs::remove_dir_all(&directory)?;
            sync_parent(&directory)?;
            stats.reclaimed_generations = stats.reclaimed_generations.saturating_add(1);
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
        bridge_generation_ids: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        let desired_roots = self.desired_generation_ids()?;
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
            if activation.file_type()?.is_file() && is_canonical_record_file(&activation) {
                let activation = read_mixed_activation(&activation.path())?;
                if let Some(scope) = activation.published_scope() {
                    authority_scopes.insert(scope.clone());
                }
                activations.push(activation);
            }
        }
        let collision_lifecycle = self.collision_retirement_pending_records_for_gc()?;
        for record in &collision_lifecycle {
            authority_scopes.extend(
                record
                    .entries
                    .values()
                    .filter(|entry| entry.state != CollisionRetirementLifecycleStateV1::Completed)
                    .map(|entry| entry.former_scope.clone()),
            );
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
            let mut protected = protected_generation_ids_from_records(
                &legacy_generations,
                &legacy_activations,
                &collision_lifecycle,
                retained_generations,
            )?;
            protected.extend(desired_roots);
            protected.extend(bridge_generation_ids.iter().cloned());
            return Ok(protected);
        }

        let mut protected = mixed_protected_generation_ids_from_records(
            generations,
            &activations,
            &collision_lifecycle,
            retained_generations,
            &authority_scopes,
            &effective_roots,
        )?;
        protected.extend(desired_roots);
        protected.extend(bridge_generation_ids.iter().cloned());
        Ok(protected)
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
                if record
                    .entries
                    .values()
                    .any(|entry| entry.state != CollisionRetirementLifecycleStateV1::Completed)
                {
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
            if !entry.file_type()?.is_file() || !is_canonical_record_file(&entry) {
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
        let mixed = self.find_generation_mixed(generation)?;
        let scope = mixed.descriptor().scope.clone();
        let sizes = self
            .load_generation_entries(&scope, generation)?
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
    if let Some(hash) = snapshot_id.strip_prefix("collected-") {
        if hash.len() != 32
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("invalid collected snapshot id");
        }
        return Ok(());
    }
    // Phase 3 plan section 4.6: a LegacyLocal project whose history record
    // selects the non-head-bound derivation uses this shape instead of the
    // collected one. Same 32-hex width as the collected shape above
    // (legacy_local_snapshot_id matches nongit_snapshot_id/
    // collected_snapshot_id's [..16]-byte convention, not the head-bound
    // family's 16-hex suffix).
    if let Some(hash) = snapshot_id.strip_prefix("legacylocal-") {
        if hash.len() != 32
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("invalid legacy-local snapshot id");
        }
        return Ok(());
    }
    bail!("invalid migration snapshot id");
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
    let parsed =
        Uuid::parse_str(upload_id).map_err(|_| anyhow!(StoreRequestError::InvalidInput))?;
    if parsed.to_string() != upload_id {
        return Err(StoreRequestError::InvalidInput.into());
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
        .ok_or(StoreRequestError::InvalidInput)?;
    if cursor_generation != generation {
        return Err(StoreRequestError::InvalidInput.into());
    }
    offset
        .parse()
        .map_err(|_| StoreRequestError::InvalidInput.into())
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

fn read_collision_retirement_work_nofollow(
    path: &Path,
) -> Result<Option<CollisionRetirementWorkV1>> {
    let Some(bytes) = read_optional_regular_nofollow(
        path,
        MAX_COLLISION_RETIREMENT_RECORD_BYTES,
        "collision retirement work",
    )?
    else {
        return Ok(None);
    };
    Ok(Some(decode_collision_retirement_work(&bytes)?))
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
    // A retirement record retires a SELECTOR; its snapshot id is whatever the
    // OUTGOING workspace entry carried, which is not a migration id. The
    // local-to-collected transition retires a `local:` selector whose
    // snapshot is head-bound (`head-<sha12>-<hex8>`) or dirty
    // (`nongit-<hex16>`), so the strict migration shape rejected the first
    // collected activation of any previously locally indexed project and the
    // activation loop retried forever. The general shape validator is the
    // right gate here: non-empty, bounded, no separators or control bytes,
    // not dot-shaped. `validate_migration_snapshot_id` stays strict for its
    // own consumers, whose ids are collected- or legacylocal-shaped by
    // construction.
    validate_snapshot_id(&record.snapshot_id)?;
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
        for (generation_id, entry) in &lifecycle.entries {
            if entry.state == CollisionRetirementLifecycleStateV1::Completed {
                continue;
            }
            let generation = generations_by_id
                .get(generation_id.as_str())
                .ok_or_else(|| {
                    anyhow!("collision retirement lifecycle references missing generation metadata")
                })?;
            if generation.is_legacy_v1() {
                bail!("protected legacy generation lacks strict v2 ownership");
            }
            if entry.former_scope != generation.descriptor().scope
                || entry.manifest_sha256 != generation.descriptor().manifest_sha256
            {
                bail!("collision retirement lifecycle does not match generation metadata");
            }
            if entry.selector_evidence == CollisionRetirementSelectorEvidenceV1::NoDurableSelector
                && (generation.state() == GenerationState::Active
                    || activations.iter().any(|activation| {
                        activation.project_id() == lifecycle.project_id.as_str()
                            && activation.generation_id() == generation_id.as_str()
                    }))
            {
                bail!("retained collision lifecycle suppresses active selector authority");
            }
            protected.insert(generation_id.clone());
        }
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
        for (generation_id, entry) in &pending.entries {
            if entry.state == CollisionRetirementLifecycleStateV1::Completed {
                continue;
            }
            let generation = generations_by_id
                .get(generation_id.as_str())
                .ok_or_else(|| {
                    anyhow!("collision retirement lifecycle references missing generation metadata")
                })?;
            if entry.former_scope != generation.descriptor.scope
                || entry.manifest_sha256 != generation.descriptor.manifest_sha256
            {
                bail!("collision retirement lifecycle does not match generation metadata");
            }
            if entry.selector_evidence == CollisionRetirementSelectorEvidenceV1::NoDurableSelector
                && (generation.state == GenerationState::Active
                    || activations.iter().any(|activation| {
                        activation.project_id.as_str() == pending.project_id.as_str()
                            && activation.generation_id == generation_id.as_str()
                    }))
            {
                bail!("retained collision lifecycle suppresses active selector authority");
            }
            protected.insert(generation_id.clone());
        }
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
    for row in &inventory.collision_work {
        field(&mut hasher, b"collision-work");
        text(&mut hasher, &row.work_id);
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

fn collision_retirement_work_id(project_id: &ProjectId, generation_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bbox-code-source-collision-retirement-work-v1\0");
    digest.update((project_id.as_str().len() as u64).to_be_bytes());
    digest.update(project_id.as_str().as_bytes());
    digest.update(generation_id.as_bytes());
    hex::encode(digest.finalize())
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

/// True for the store's canonical `<key>.json` record files.
///
/// [`atomic_write`] stages `<key>.<uuid>.tmp` beside its destination, so a
/// crash between create and rename leaves debris that record enumeration must
/// skip instead of parsing as a record.
fn is_canonical_record_file(entry: &fs::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.ends_with(".json"))
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

/// Observation hook for a desired pointer whose generation was already
/// reclaimed. The pointer is repointed at the new generation by the caller;
/// the reclaimed generation stays reclaimed.
fn tracing_reclaimed_desired_generation(_generation_id: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_code_source::{
        SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint, manifest_sha256, source_selector,
    };

    pub(super) fn descriptor(entries: &[ManifestEntry]) -> GenerationDescriptor {
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

    fn install_retirement_generation(
        store: &CodeSourceStore,
        scope: &PublishedScope,
        producer_id: &str,
        blob_hashes: &[String],
    ) -> String {
        let entries = blob_hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| ManifestEntry {
                relative_path: format!("src/{index}.rs"),
                content_sha256: hash.clone(),
                size: 1,
            })
            .collect::<Vec<_>>();
        let mut descriptor = descriptor(&entries);
        descriptor.scope = scope.clone();
        let generation_id = generation_id(producer_id, &descriptor);
        let generation = StoredGenerationV2 {
            version: MIGRATION_STORE_VERSION,
            generation_id: generation_id.clone(),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor,
            published_scope: scope.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: None,
            entity_inventory_sha256: None,
        };
        let directory = store
            .paths
            .generation_directory(scope, &generation_id)
            .unwrap();
        fs::create_dir_all(&directory).unwrap();
        write_manifest_jsonl(&directory.join("manifest.jsonl"), &entries).unwrap();
        atomic_write_json(&directory.join("metadata.json"), &generation).unwrap();
        for hash in blob_hashes {
            let blob = store.blob_path(hash);
            fs::create_dir_all(blob.parent().unwrap()).unwrap();
            fs::write(blob, b"x").unwrap();
        }
        generation_id
    }

    #[test]
    fn retirement_exact_generation_delete_preserves_other_generation_in_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let scope = PublishedScope::try_new("owner-a", ".").unwrap();
        let first =
            install_retirement_generation(&store, &scope, "producer-a", &[format!("{:064x}", 1)]);
        let second =
            install_retirement_generation(&store, &scope, "producer-b", &[format!("{:064x}", 2)]);

        store.delete_retirement_generation(&scope, &first).unwrap();

        assert!(
            !store
                .paths
                .generation_directory(&scope, &first)
                .unwrap()
                .exists()
        );
        assert!(
            store
                .paths
                .generation_directory(&scope, &second)
                .unwrap()
                .is_dir()
        );
    }

    #[test]
    fn retirement_blob_sweep_deletes_unique_and_preserves_shared() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let retiring_scope = PublishedScope::try_new("owner-a", ".").unwrap();
        let retained_scope = PublishedScope::try_new("owner-b", ".").unwrap();
        let shared = format!("{:064x}", 10);
        let unique = format!("{:064x}", 11);
        let retiring = install_retirement_generation(
            &store,
            &retiring_scope,
            "producer-a",
            &[shared.clone(), unique.clone()],
        );
        install_retirement_generation(
            &store,
            &retained_scope,
            "producer-b",
            std::slice::from_ref(&shared),
        );

        store
            .delete_retirement_generation(&retiring_scope, &retiring)
            .unwrap();
        store
            .sweep_retirement_blobs(&BTreeSet::from([shared.clone(), unique.clone()]))
            .unwrap();

        assert!(store.blob_path(&shared).is_file());
        assert!(!store.blob_path(&unique).exists());
    }

    pub(super) fn manifest_bytes(entries: &[ManifestEntry]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for entry in entries {
            serde_json::to_writer(&mut bytes, entry).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }

    #[test]
    fn migration_snapshot_id_accepts_collected_and_legacylocal_shapes() {
        assert!(validate_migration_snapshot_id(&format!("collected-{}", "a".repeat(32))).is_ok());
        assert!(validate_migration_snapshot_id(&format!("legacylocal-{}", "a".repeat(32))).is_ok());
        for invalid in [
            format!("collected-{}", "a".repeat(31)),
            format!("collected-{}", "A".repeat(32)),
            format!("legacylocal-{}", "a".repeat(31)),
            format!("legacylocal-{}", "a".repeat(16)),
            format!("legacylocal-{}", "A".repeat(32)),
            "head-abc123-0011223344556677".to_string(),
            "nongit-0011223344556677".to_string(),
            "unknown-prefix".to_string(),
            String::new(),
        ] {
            assert!(
                validate_migration_snapshot_id(&invalid).is_err(),
                "{invalid} should be rejected"
            );
        }
    }

    #[test]
    fn migration_existing_open_never_creates_a_missing_store() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store_root = root.join("code-sources");
        assert!(
            CodeSourceStore::open_existing_for_migration(&store_root, StoreLimits::default())
                .unwrap()
                .is_none()
        );
        assert!(!store_root.exists());

        let store = CodeSourceStore::open(&store_root, StoreLimits::default()).unwrap();
        drop(store);
        let existing =
            CodeSourceStore::open_existing_for_migration(&store_root, StoreLimits::default())
                .unwrap()
                .unwrap();
        assert_eq!(existing.root(), store_root.canonicalize().unwrap());
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

    pub(super) fn stored_generation_v1(
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
        let unprotected = write_legacy_generation_fixture(
            &unprotected_paths,
            "host-unprotected",
            1,
            GenerationState::Failed,
        );
        let unprotected_generation_dir = unprotected_paths
            .generation_metadata(&unprotected.descriptor.scope, &unprotected.generation_id)
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let guard = unprotected_paths.lock_migration_inventory().unwrap();
        let first = guard.snapshot_current_v2(&StoreLimits::default()).unwrap();
        let second = guard.snapshot_current_v2(&StoreLimits::default()).unwrap();
        assert!(first.generations.is_empty());
        assert!(first.effective_manifest.selections.is_empty());
        assert_eq!(first.canonical_sha256, second.canonical_sha256);
        drop(guard);
        let stats = CodeSourceStore::open(
            unprotected_paths.root().to_path_buf(),
            StoreLimits {
                unreferenced_blob_grace_hours: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap()
        .gc_blobs()
        .unwrap();
        assert_eq!(stats.reclaimed_generations, 1);
        assert!(!unprotected_generation_dir.exists());

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
        let mut work_bytes_len = 0;
        for index in 0..8 {
            let project_id = ProjectId::parse(format!("completed-{index}")).unwrap();
            let generation_id = format!("{:064x}", index + 1);
            let lifecycle = CollisionRetirementLifecycleV1 {
                version: STORE_VERSION,
                project_id: project_id.clone(),
                entries: BTreeMap::from([(
                    generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Completed,
                        former_scope: PublishedScope::try_new(format!("repo-{index}"), ".")
                            .unwrap(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            materialized_selector(project_id.as_str(), &generation_id),
                        ),
                        snapshot_id: format!("collected-{:032x}", index + 1),
                        manifest_sha256: "b".repeat(64),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                )]),
            };
            fs::write(
                paths.collision_retirement_pending(&project_id),
                encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
            )
            .unwrap();
            if index == 0 {
                let work = CollisionRetirementWorkV1::from_entry(
                    &project_id,
                    &generation_id,
                    lifecycle.entry(&generation_id).unwrap(),
                );
                let work_path = paths
                    .collision_retirement_work(&project_id, &generation_id)
                    .unwrap();
                fs::create_dir_all(work_path.parent().unwrap()).unwrap();
                let work_bytes = serde_json::to_vec_pretty(&work).unwrap();
                work_bytes_len = work_bytes.len();
                fs::write(work_path, work_bytes).unwrap();
            }
        }
        let limits = StoreLimits {
            max_migration_survivor_rows: 1,
            max_migration_survivor_bytes: effective.len() + work_bytes_len,
            ..StoreLimits::default()
        };
        let guard = paths.lock_migration_inventory().unwrap();

        let inventory = guard.snapshot_current_v2(&limits).unwrap();

        assert!(inventory.collision_pending.is_empty());
        assert_eq!(inventory.collision_work.len(), 1);
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

    /// Regression: the first collected activation of a previously locally
    /// indexed project retires the outgoing `local:` selector, whose
    /// snapshot id is head-bound or dirty-shaped, never a migration id.
    /// Rejecting those failed the whole activation and the daemon retried
    /// forever on backoff.
    #[test]
    fn retirement_accepts_outgoing_local_snapshot_shapes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);

        // A clean local checkout: head-<sha12>-<hex8>.
        let head_bound = RetirementRecord {
            version: STORE_VERSION,
            project_id: "project-a".into(),
            selector: "local:project-a".into(),
            snapshot_id: "head-abc123def456-0011223344556677".into(),
            generation_id: None,
        };
        store
            .enqueue_retirement(&head_bound)
            .expect("a head-bound outgoing snapshot must be retirable");
        assert!(store.retirement_pending(&head_bound.selector).unwrap());
        store.complete_retirement(&head_bound).unwrap();
        assert!(!store.retirement_pending(&head_bound.selector).unwrap());

        // A dirty local worktree: nongit-<hex16>.
        let dirty = RetirementRecord {
            version: STORE_VERSION,
            project_id: "project-b".into(),
            selector: "local:project-b".into(),
            snapshot_id: format!("nongit-{}", "a".repeat(32)),
            generation_id: None,
        };
        store
            .enqueue_retirement(&dirty)
            .expect("a dirty-worktree outgoing snapshot must be retirable");
        assert!(store.retirement_pending(&dirty.selector).unwrap());
        store.complete_retirement(&dirty).unwrap();

        // A collected outgoing snapshot (collected-to-collected) still works.
        let collected = RetirementRecord {
            version: STORE_VERSION,
            project_id: "project-c".into(),
            selector: "local:project-c".into(),
            snapshot_id: format!("collected-{}", "b".repeat(32)),
            generation_id: None,
        };
        store
            .enqueue_retirement(&collected)
            .expect("a collected outgoing snapshot must stay retirable");
        store.complete_retirement(&collected).unwrap();
    }

    /// Widening the snapshot shape must not open the path lane: the general
    /// validator still refuses separators, traversal, control bytes, and
    /// empty or oversized ids.
    #[test]
    fn retirement_still_refuses_unsafe_snapshot_ids() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);

        for unsafe_id in [
            "../../etc/passwd".to_string(),
            "head-abc/def".to_string(),
            "head-abc\\def".to_string(),
            "head-abc\u{0}def".to_string(),
            "..".to_string(),
            ".".to_string(),
            String::new(),
            "h".repeat(MAX_SNAPSHOT_ID_BYTES + 1),
        ] {
            let record = RetirementRecord {
                version: STORE_VERSION,
                project_id: "project-a".into(),
                selector: "local:project-a".into(),
                snapshot_id: unsafe_id.clone(),
                generation_id: None,
            };
            assert!(
                store.enqueue_retirement(&record).is_err(),
                "{unsafe_id:?} must be refused"
            );
        }
    }

    #[test]
    fn ordinary_retirement_preserves_failed_or_requeued_desired_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let descriptor = descriptor(&[]);
        let scope = descriptor.scope.clone();
        let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        let ready = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        let retirement = RetirementRecord {
            version: STORE_VERSION,
            project_id: "project-a".into(),
            selector: materialized_selector("project-a", &ready.generation_id),
            snapshot_id: format!("collected-{}", "a".repeat(32)),
            generation_id: Some(ready.generation_id.clone()),
        };

        store.enqueue_retirement(&retirement).unwrap();
        store
            .mark_generation_state(
                &scope,
                &ready.generation_id,
                GenerationState::Failed,
                Some("staged verification failed".into()),
            )
            .unwrap();
        assert!(store.retirement_pending(&retirement.selector).unwrap());
        store.complete_retirement(&retirement).unwrap();
        let failed = store.load_generation(&scope, &ready.generation_id).unwrap();
        assert_eq!(failed.state, GenerationState::Failed);
        assert_eq!(
            failed.diagnostic.as_deref(),
            Some("staged verification failed")
        );
        assert!(!store.retirement_pending(&retirement.selector).unwrap());

        let replay = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        assert_eq!(replay.state, GenerationState::Ready);
        store.enqueue_retirement(&retirement).unwrap();
        store.complete_retirement(&retirement).unwrap();
        assert_eq!(
            store
                .load_generation(&scope, &ready.generation_id)
                .unwrap()
                .state,
            GenerationState::Ready
        );

        let replacement = store.begin_upload("host-b", descriptor).unwrap();
        store
            .complete_manifest("host-b", &replacement.upload_id)
            .unwrap();
        store
            .finalize_upload("host-b", &replacement.upload_id)
            .unwrap();
        store
            .mark_generation_state(&scope, &ready.generation_id, GenerationState::Ready, None)
            .unwrap();
        store.enqueue_retirement(&retirement).unwrap();
        store.complete_retirement(&retirement).unwrap();
        assert_eq!(
            store
                .load_generation(&scope, &ready.generation_id)
                .unwrap()
                .state,
            GenerationState::Superseded
        );
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
        let record = collision_lifecycle_fixture();
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
        invalid_selector
            .entries
            .get_mut(&"a".repeat(64))
            .unwrap()
            .selector_evidence =
            CollisionRetirementSelectorEvidenceV1::ExactMaterialized("selector-a".into());
        assert!(encode_collision_retirement_pending_for_migration(&invalid_selector).is_err());
        let mut invalid_hash = record;
        invalid_hash
            .entries
            .get_mut(&"b".repeat(64))
            .unwrap()
            .plan_hash = "not-a-hash".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
        invalid_hash
            .entries
            .get_mut(&"b".repeat(64))
            .unwrap()
            .plan_hash = "d".repeat(64);
        invalid_hash
            .entries
            .get_mut(&"b".repeat(64))
            .unwrap()
            .snapshot_id = "snapshot-a".into();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
        invalid_hash.entries.clear();
        assert!(encode_collision_retirement_pending_for_migration(&invalid_hash).is_err());
    }

    fn collision_lifecycle_fixture() -> CollisionRetirementLifecycleV1 {
        CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            project_id: ProjectId::parse("project-a").unwrap(),
            entries: BTreeMap::from([
                (
                    "a".repeat(64),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            materialized_selector("project-a", &"a".repeat(64)),
                        ),
                        snapshot_id: format!("collected-{}", "e".repeat(32)),
                        manifest_sha256: "b".repeat(64),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
                (
                    "b".repeat(64),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: PublishedScope::try_new("repo-family", ".").unwrap(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "f".repeat(32)),
                        manifest_sha256: "1".repeat(64),
                        inventory_hash: "2".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
            ]),
        }
    }

    fn collision_terminal_fixture(
        store: &CodeSourceStore,
    ) -> (CollisionRetirementLifecycleV1, String, String) {
        let descriptor = descriptor(&[]);
        let scope = descriptor.scope.clone();
        let project_id = ProjectId::parse("project-terminal").unwrap();
        let records = ["host-exact", "host-retained"]
            .into_iter()
            .map(|producer_id| {
                let generation_id = generation_id(producer_id, &descriptor);
                let record = StoredGenerationV2 {
                    version: MIGRATION_STORE_VERSION,
                    generation_id: generation_id.clone(),
                    producer_id: producer_id.to_string(),
                    ordinal: 1,
                    descriptor: descriptor.clone(),
                    published_scope: scope.clone(),
                    state: GenerationState::Ready,
                    diagnostic: Some("pending collision retirement".to_string()),
                    created_unix_secs: 1,
                    materialized_doc_count: None,
                    entity_inventory_sha256: None,
                };
                let metadata_path = store
                    .paths
                    .generation_metadata(&scope, &generation_id)
                    .unwrap();
                fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
                fs::write(
                    metadata_path,
                    encode_stored_generation_v2_for_migration(&record).unwrap(),
                )
                .unwrap();
                (generation_id, record)
            })
            .collect::<Vec<_>>();
        let exact_generation_id = records[0].0.clone();
        let retained_generation_id = records[1].0.clone();
        let lifecycle = CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    exact_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            materialized_selector(project_id.as_str(), &exact_generation_id),
                        ),
                        snapshot_id: format!("collected-{}", "e".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
                (
                    retained_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope,
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "f".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256,
                        inventory_hash: "2".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
            ]),
        };
        (lifecycle, exact_generation_id, retained_generation_id)
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
    fn collision_lifecycle_reconciles_mixed_entries_to_independent_work() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();

        let queued = read_collision_lifecycle(&store, &lifecycle.project_id);
        assert!(
            queued
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Queued)
        );
        let work = store.collision_retirement_work_records().unwrap();
        assert_eq!(work.len(), 2);
        assert_eq!(work[0].generation_id, "a".repeat(64));
        assert!(work[0].exact_selector().is_some());
        assert_eq!(work[1].generation_id, "b".repeat(64));
        assert!(work[1].exact_selector().is_none());
        assert!(store.retirement_records().unwrap().is_empty());
    }

    #[test]
    fn retained_only_collision_lifecycle_completes_without_selector_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let (mut lifecycle, exact_generation_id, retained_generation_id) =
            collision_terminal_fixture(&store);
        lifecycle.entries.remove(&exact_generation_id);
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();

        let work = store.collision_retirement_work_records().unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].generation_id, retained_generation_id);
        assert!(work[0].exact_selector().is_none());
        assert!(store.retirement_records().unwrap().is_empty());
        store
            .repair_and_complete_collision_retirement(
                &lifecycle.project_id,
                &retained_generation_id,
            )
            .unwrap();
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&retained_generation_id)
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_entries_complete_independently_and_remain_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let (lifecycle, exact_generation_id, retained_generation_id) =
            collision_terminal_fixture(&store);
        write_collision_lifecycle(&store, &lifecycle);
        store.reconcile_collision_retirements().unwrap();

        store
            .repair_and_complete_collision_retirement(
                &lifecycle.project_id,
                &retained_generation_id,
            )
            .unwrap();
        let partial = read_collision_lifecycle(&store, &lifecycle.project_id);
        assert_eq!(
            partial.entry(&exact_generation_id).unwrap().state,
            CollisionRetirementLifecycleStateV1::Queued
        );
        assert_eq!(
            partial.entry(&retained_generation_id).unwrap().state,
            CollisionRetirementLifecycleStateV1::Completed
        );
        assert_eq!(store.collision_retirement_work_records().unwrap().len(), 1);
        store
            .repair_and_complete_collision_retirement(&lifecycle.project_id, &exact_generation_id)
            .unwrap();

        let completed = read_collision_lifecycle(&store, &lifecycle.project_id);
        assert!(
            completed
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Completed)
        );
        assert!(
            store
                .collision_retirement_work_records()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn collision_terminal_transition_refuses_intervening_metadata_and_legacy_then_retries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let (mut lifecycle, exact_generation_id, retained_generation_id) =
            collision_terminal_fixture(&store);
        lifecycle.entries.remove(&exact_generation_id);
        write_collision_lifecycle(&store, &lifecycle);
        store.reconcile_collision_retirements().unwrap();

        let entry = lifecycle.entry(&retained_generation_id).unwrap();
        let metadata_path = store
            .paths
            .generation_metadata(&entry.former_scope, &retained_generation_id)
            .unwrap();
        let expected =
            decode_stored_generation_v2_for_migration(&fs::read(&metadata_path).unwrap()).unwrap();
        let mut intervening = expected.clone();
        intervening.producer_id = "host-intervening".to_string();
        intervening.generation_id =
            generation_id(&intervening.producer_id, &intervening.descriptor);
        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&intervening).unwrap(),
        )
        .unwrap();

        assert!(
            store
                .repair_and_complete_collision_retirement(
                    &lifecycle.project_id,
                    &retained_generation_id,
                )
                .unwrap_err()
                .to_string()
                .contains("metadata disagrees")
        );
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&retained_generation_id)
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Queued
        );
        assert_eq!(store.collision_retirement_work_records().unwrap().len(), 1);

        let legacy = StoredGeneration {
            version: STORE_VERSION,
            generation_id: expected.generation_id.clone(),
            producer_id: expected.producer_id.clone(),
            ordinal: expected.ordinal,
            descriptor: expected.descriptor.clone(),
            state: GenerationState::Ready,
            diagnostic: expected.diagnostic.clone(),
            created_unix_secs: expected.created_unix_secs,
            materialized_doc_count: expected.materialized_doc_count,
            entity_inventory_sha256: expected.entity_inventory_sha256.clone(),
        };
        fs::write(&metadata_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        assert!(
            store
                .repair_and_complete_collision_retirement(
                    &lifecycle.project_id,
                    &retained_generation_id,
                )
                .unwrap_err()
                .to_string()
                .contains("refuses legacy generation metadata")
        );
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&retained_generation_id)
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Queued
        );

        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&expected).unwrap(),
        )
        .unwrap();
        store
            .repair_and_complete_collision_retirement(
                &lifecycle.project_id,
                &retained_generation_id,
            )
            .unwrap();
        assert_eq!(
            decode_stored_generation_v2_for_migration(&fs::read(&metadata_path).unwrap())
                .unwrap()
                .state,
            GenerationState::Superseded
        );
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&retained_generation_id)
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_reconciliation_refuses_queued_absence_and_repairs_completed_lag() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        for entry in lifecycle.entries.values_mut() {
            entry.state = CollisionRetirementLifecycleStateV1::Queued;
        }
        write_collision_lifecycle(&store, &lifecycle);

        assert!(store.reconcile_collision_retirements().is_err());
        assert!(store.collision_retirement_work_records().is_err());

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let mut lifecycle = collision_lifecycle_fixture();
        write_collision_lifecycle(&store, &lifecycle);
        store.reconcile_collision_retirements().unwrap();
        let work_path = store
            .paths
            .collision_retirement_work(&lifecycle.project_id, &"a".repeat(64))
            .unwrap();
        assert!(work_path.is_file());
        lifecycle.entries.get_mut(&"a".repeat(64)).unwrap().state =
            CollisionRetirementLifecycleStateV1::Completed;
        write_collision_lifecycle(&store, &lifecycle);

        store.reconcile_collision_retirements().unwrap();

        assert!(!work_path.exists());
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&"b".repeat(64))
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Queued
        );
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&"a".repeat(64))
                .unwrap()
                .state,
            CollisionRetirementLifecycleStateV1::Completed
        );
    }

    #[test]
    fn collision_lifecycle_reconciliation_rejects_mutated_work_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        write_collision_lifecycle(&store, &lifecycle);
        store.reconcile_collision_retirements().unwrap();

        let work_path = store
            .paths
            .collision_retirement_work(&lifecycle.project_id, &"b".repeat(64))
            .unwrap();
        let mut work: CollisionRetirementWorkV1 =
            serde_json::from_slice(&fs::read(&work_path).unwrap()).unwrap();
        work.inventory_hash = "9".repeat(64);
        fs::write(&work_path, serde_json::to_vec_pretty(&work).unwrap()).unwrap();

        assert!(store.reconcile_collision_retirements().is_err());
        assert_eq!(
            read_collision_lifecycle(&store, &lifecycle.project_id)
                .entry(&"b".repeat(64))
                .unwrap()
                .inventory_hash,
            "2".repeat(64)
        );
    }

    #[test]
    fn collision_lifecycle_descendant_validation_is_transitive_and_immutable() {
        let pending = collision_lifecycle_fixture();
        let mut completed = pending.clone();
        for entry in completed.entries.values_mut() {
            entry.state = CollisionRetirementLifecycleStateV1::Completed;
        }

        completed.validate_descendant_from(&pending).unwrap();
        assert!(completed.validate_transition_from(&pending).is_err());

        let mut rewritten = completed.clone();
        rewritten
            .entries
            .get_mut(&"b".repeat(64))
            .unwrap()
            .inventory_hash = "9".repeat(64);
        assert!(rewritten.validate_descendant_from(&pending).is_err());

        let mut regressed = completed;
        regressed.entries.get_mut(&"a".repeat(64)).unwrap().state =
            CollisionRetirementLifecycleStateV1::Pending;
        assert!(regressed.validate_descendant_from(&pending).is_ok());
        assert!(pending.validate_descendant_from(&regressed).is_err());
    }

    #[test]
    fn collision_lifecycle_lookup_rejects_noncanonical_and_oversized_rows() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
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
        assert!(store.reconcile_collision_retirements().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn collision_lifecycle_lookup_refuses_a_symlinked_row() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let lifecycle = collision_lifecycle_fixture();
        let lifecycle_directory = store.root().join("collision-retirements");
        fs::create_dir_all(&lifecycle_directory).unwrap();
        let target = store.root().join("outside-lifecycle.json");
        fs::write(
            &target,
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();
        symlink(&target, lifecycle_directory.join("project-a.json")).unwrap();

        assert!(store.reconcile_collision_retirements().is_err());
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
        // A collision-retired generation has left the producer lane: its
        // scope's desired pointer no longer names it (a still-desired
        // generation is a GC root whatever its state, per the M8 guard).
        // This test exercises the COLLISION record's lifecycle gate alone.
        std::fs::remove_file(
            store
                .root()
                .join("desired")
                .join(format!("{}.json", scope_hash(&descriptor.scope))),
        )
        .unwrap();

        let project_id = ProjectId::parse("project-a").unwrap();
        let selector = materialized_selector(project_id.as_str(), &stored.generation_id);
        let generation_id = stored.generation_id.clone();
        let pending = CollisionRetirementLifecycleV1 {
            version: STORE_VERSION,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: descriptor.scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            selector,
                        ),
                        snapshot_id: format!("collected-{}", "e".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
                (
                    "f".repeat(64),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Completed,
                        former_scope: descriptor.scope,
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "f".repeat(32)),
                        manifest_sha256: "1".repeat(64),
                        inventory_hash: "2".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                ),
            ]),
        };
        let pending_path = store.paths.collision_retirement_pending(&project_id);
        atomic_write(
            &pending_path,
            &encode_collision_retirement_pending_for_migration(&pending).unwrap(),
        )
        .unwrap();

        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 0);
        assert!(store.blob_path(&hash).is_file());
        let mut completed = pending;
        completed.entries.get_mut(&generation_id).unwrap().state =
            CollisionRetirementLifecycleStateV1::Completed;
        atomic_write(
            &pending_path,
            &encode_collision_retirement_pending_for_migration(&completed).unwrap(),
        )
        .unwrap();
        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 1);
        assert!(!store.blob_path(&hash).exists());
        assert!(pending_path.is_file());
    }

    #[test]
    fn collision_pending_gc_scan_is_missing_safe_and_corruption_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let collision_directory = store.root().join("collision-retirements");
        assert!(collision_directory.is_dir());
        assert_eq!(store.gc_blobs().unwrap().reclaimed_blobs, 0);
        assert!(collision_directory.is_dir());

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
                project_id: project_id.clone(),
                entries: BTreeMap::from([(
                    generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Completed,
                        former_scope: PublishedScope::try_new(format!("gc-repo-{index:04}"), ".")
                            .unwrap(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            materialized_selector(project_id.as_str(), &generation_id),
                        ),
                        snapshot_id: format!("collected-{:032x}", index + 1),
                        manifest_sha256: "b".repeat(64),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                )]),
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

    #[test]
    fn gc_keeps_a_failed_generation_named_by_the_desired_pointer() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut limits = StoreLimits::default();
        limits.retained_generations = 0;
        limits.unreferenced_blob_grace_hours = 0;
        let store = CodeSourceStore::open(root.join("code-sources"), limits).unwrap();
        let bytes = b"desired generation";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let descriptor = descriptor(&entries);
        let scope = descriptor.scope.clone();
        let upload = store.begin_upload("host-a", descriptor).unwrap();
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
            .mark_generation_state(
                &scope,
                &ready.generation_id,
                GenerationState::Failed,
                Some("staged activation failed".into()),
            )
            .unwrap();

        let stats = store.gc_blobs().unwrap();

        assert_eq!(stats.reclaimed_generations, 0);
        assert_eq!(stats.reclaimed_blobs, 0);
        assert!(store.blob_path(&hash).is_file());
        assert!(
            store
                .paths
                .generation_manifest(&scope, &ready.generation_id)
                .unwrap()
                .is_file()
        );
        assert_eq!(
            store
                .load_generation(&scope, &ready.generation_id)
                .unwrap()
                .state,
            GenerationState::Failed
        );
        assert_eq!(
            store
                .desired_generation(&scope)
                .unwrap()
                .unwrap()
                .generation_id,
            ready.generation_id
        );
    }

    #[test]
    fn finalize_does_not_resurrect_a_reclaimed_desired_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"reclaimed generation";
        let hash = sha256_hex(bytes);
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let descriptor = descriptor(&entries);
        let scope = descriptor.scope.clone();
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
        let reclaimed = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        let reclaimed_directory = store
            .paths
            .generation_directory(&scope, &reclaimed.generation_id)
            .unwrap();
        fs::remove_dir_all(&reclaimed_directory).unwrap();

        let replacement = store.begin_upload("host-b", descriptor).unwrap();
        store
            .put_manifest_page("host-b", &replacement.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest("host-b", &replacement.upload_id)
            .unwrap();
        let published = store
            .finalize_upload("host-b", &replacement.upload_id)
            .unwrap();

        assert!(!reclaimed_directory.exists());
        assert_eq!(published.state, GenerationState::Ready);
        assert_eq!(
            store
                .desired_generation(&scope)
                .unwrap()
                .unwrap()
                .generation_id,
            published.generation_id
        );
        store.gc_blobs().unwrap();
        store.scrub_retained().unwrap();
        assert!(store.blob_path(&hash).is_file());
    }

    #[test]
    fn record_enumeration_skips_crash_orphaned_temp_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = open_store(&root);
        let bytes = b"activated generation";
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
        store
            .record_health_failure("project-a", "missing_blob_data", "one blob failed")
            .unwrap();
        store
            .enqueue_retirement(&RetirementRecord {
                version: STORE_VERSION,
                project_id: "project-a".into(),
                selector: materialized_selector("project-a", &ready.generation_id),
                snapshot_id: format!("collected-{}", "a".repeat(32)),
                generation_id: Some(ready.generation_id.clone()),
            })
            .unwrap();
        for relative in [
            "activations",
            "health",
            "retirements",
            "desired",
            "collision-retirements",
        ] {
            fs::write(
                store
                    .root()
                    .join(relative)
                    .join(format!("orphan.{}.tmp", Uuid::new_v4())),
                b"{\"version\":",
            )
            .unwrap();
        }

        assert_eq!(store.activation_records().unwrap().len(), 1);
        assert_eq!(store.health_records().unwrap().len(), 1);
        assert_eq!(store.retirement_records().unwrap().len(), 1);
        store.gc_blobs().unwrap();
        assert!(store.blob_path(&hash).is_file());
    }

    // ---- Phase 4-A cutback substrate tests ----

    fn sample_v2_generation() -> StoredGenerationV2 {
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "a".repeat(64),
            size: 1,
        }];
        let descriptor = descriptor(&entries);
        let legacy = stored_generation_v1("host-a", descriptor.clone());
        StoredGenerationV2::from_v1_for_migration(legacy, descriptor.scope).unwrap()
    }

    fn sample_v2_activation(generation: &StoredGenerationV2) -> ActivationRecordV2 {
        ActivationRecordV2::from_v1_for_migration(
            activation_v1(&generation.generation_id),
            generation,
        )
        .unwrap()
    }

    #[test]
    fn cutback_codec_round_trips_every_variant_through_activation_v2() {
        let generation = sample_v2_generation();
        let mut activation = sample_v2_activation(&generation);
        let states = vec![
            CutbackStateV2::Structural {
                reason: CutbackReason::NoLocalAttachment,
            },
            CutbackStateV2::Structural {
                reason: CutbackReason::AmbiguousAttachment,
            },
            CutbackStateV2::Structural {
                reason: CutbackReason::ScopeMismatch,
            },
            CutbackStateV2::Transient {
                attempt: 1,
                error_class: CutbackErrorClass::WriterContention,
                deadline_unix_secs: 1_700_000_000,
            },
            CutbackStateV2::Transient {
                attempt: 8,
                error_class: CutbackErrorClass::IoPressure,
                deadline_unix_secs: 1_700_000_001,
            },
            CutbackStateV2::ManualRetryRequired {
                error_class: CutbackErrorClass::IndexCommit,
                attempt: 9,
            },
            CutbackStateV2::Terminal {
                error_class: CutbackErrorClass::SecurityFailure,
            },
        ];
        for state in &states {
            let derived_pending = !matches!(state, CutbackStateV2::Terminal { .. });
            activation.cutback = Some(state.clone());
            activation.cutback_pending = derived_pending;
            let bytes = encode_activation_v2_for_migration(&activation).unwrap();
            let decoded = decode_activation_v2_for_migration(&bytes).unwrap();
            assert_eq!(decoded.cutback.as_ref(), Some(state));
            assert_eq!(decoded.cutback_pending, derived_pending);
        }
    }

    #[test]
    fn old_v2_bytes_without_cutback_decode_to_none() {
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        assert!(activation.cutback.is_none());
        let mut json: serde_json::Value =
            serde_json::from_slice(&encode_activation_v2_for_migration(&activation).unwrap())
                .unwrap();
        assert!(
            json.as_object().unwrap().get("cutback").is_none()
                || json.as_object().unwrap()["cutback"].is_null()
        );
        json.as_object_mut().unwrap().remove("cutback");
        let bytes = serde_json::to_vec(&json).unwrap();
        let decoded = decode_activation_v2_for_migration(&bytes).unwrap();
        assert!(decoded.cutback.is_none());
        assert!(!decoded.cutback_pending);
    }

    #[test]
    fn validate_refuses_transient_with_zero_attempt() {
        let generation = sample_v2_generation();
        let mut activation = sample_v2_activation(&generation);
        activation.cutback = Some(CutbackStateV2::Transient {
            attempt: 0,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: 1_700_000_000,
        });
        activation.cutback_pending = true;
        assert!(activation.validate().is_err());
    }

    #[test]
    fn validate_refuses_coherence_violation_for_typed_cutback() {
        let generation = sample_v2_generation();
        let mut activation = sample_v2_activation(&generation);
        // A non-Terminal typed cutback requires cutback_pending == true.
        activation.cutback = Some(CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        });
        activation.cutback_pending = false;
        let err = activation.validate().unwrap_err().to_string();
        assert!(err.contains("code_source_cutback_coherence"), "{err}");

        // Terminal requires cutback_pending == false.
        activation.cutback = Some(CutbackStateV2::Terminal {
            error_class: CutbackErrorClass::SecurityFailure,
        });
        activation.cutback_pending = true;
        let err = activation.validate().unwrap_err().to_string();
        assert!(err.contains("code_source_cutback_coherence"), "{err}");
    }

    #[test]
    fn validate_admits_legacy_migration_shape() {
        let generation = sample_v2_generation();
        let mut activation = sample_v2_activation(&generation);
        // The legacy-migration shape (cutback: None, cutback_pending: true)
        // is admitted at store-level validate; the startup relationship
        // chain is the sole refuser (section 4.10).
        activation.cutback = None;
        activation.cutback_pending = true;
        assert!(activation.validate().is_ok());
    }

    #[test]
    fn mixed_read_bridge_refuses_v2_activation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        store.save_activation_v2(&activation).unwrap();

        // Reopen in bridge mode: the v2 bytes must be refused.
        drop(store);
        let bridge =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let err = bridge
            .load_activation_mixed("project-a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("code_source_record_mode"), "{err}");

        let err = bridge.activation_records_mixed().unwrap_err().to_string();
        assert!(err.contains("code_source_record_mode"), "{err}");
    }

    #[test]
    fn mixed_read_catalog_reads_v2_and_treats_v1_as_absent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let bridge =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let generation = sample_v2_generation();
        let activation = activation_v1(&generation.generation_id);
        bridge.save_activation(&activation).unwrap();

        // Catalog mode sees the v1 record as absent (not a decode error).
        drop(bridge);
        let catalog = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        assert!(
            catalog
                .load_activation_mixed("project-a")
                .unwrap()
                .is_none()
        );
        assert!(catalog.activation_records_mixed().unwrap().is_empty());
    }

    #[test]
    fn mixed_read_catalog_round_trips_v2_activation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        store.save_activation_v2(&activation).unwrap();

        let loaded = store
            .load_activation_mixed("project-a")
            .unwrap()
            .expect("v2 record should load in catalog mode");
        assert!(loaded.is_current_v2());
        assert_eq!(loaded.generation_id(), activation.generation_id);
        assert!(loaded.cutback().is_none());

        let records = store.activation_records_mixed().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].is_current_v2());
    }

    #[test]
    fn mark_cutback_state_updates_cutback_and_derived_pending() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        store.save_activation_v2(&activation).unwrap();

        // Structural -> cutback_pending derived true.
        store
            .mark_cutback_state(
                "project-a",
                CutbackStateV2::Structural {
                    reason: CutbackReason::NoLocalAttachment,
                },
            )
            .unwrap();
        let loaded = store.load_activation_mixed("project-a").unwrap().unwrap();
        match loaded {
            MixedActivationRecord::CurrentV2(record) => {
                assert!(record.cutback_pending);
                assert!(matches!(
                    record.cutback,
                    Some(CutbackStateV2::Structural {
                        reason: CutbackReason::NoLocalAttachment
                    })
                ));
            }
            _ => panic!("expected v2 record"),
        }

        // Terminal -> cutback_pending derived false.
        store
            .mark_cutback_state(
                "project-a",
                CutbackStateV2::Terminal {
                    error_class: CutbackErrorClass::SecurityFailure,
                },
            )
            .unwrap();
        let loaded = store.load_activation_mixed("project-a").unwrap().unwrap();
        match loaded {
            MixedActivationRecord::CurrentV2(record) => {
                assert!(!record.cutback_pending);
                assert!(matches!(
                    record.cutback,
                    Some(CutbackStateV2::Terminal {
                        error_class: CutbackErrorClass::SecurityFailure
                    })
                ));
            }
            _ => panic!("expected v2 record"),
        }
    }

    #[test]
    fn mark_cutback_state_refuses_invalid_transient() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        store.save_activation_v2(&activation).unwrap();

        let err = store
            .mark_cutback_state(
                "project-a",
                CutbackStateV2::Transient {
                    attempt: 0,
                    error_class: CutbackErrorClass::IoPressure,
                    deadline_unix_secs: 1_700_000_000,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("code_source_cutback_state"), "{err}");
    }

    #[test]
    fn save_activation_v2_refuses_on_bridge_mode_store() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let bridge =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        assert_eq!(bridge.record_mode(), RuntimeRecordMode::BridgeV1);
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        let err = bridge
            .save_activation_v2(&activation)
            .unwrap_err()
            .to_string();
        assert!(err.contains("code_source_record_mode"), "{err}");
        assert!(err.contains("refuses v2 activation writes"), "{err}");
    }

    #[test]
    fn mark_cutback_state_refuses_on_bridge_mode_store() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let bridge =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let err = bridge
            .mark_cutback_state(
                "project-a",
                CutbackStateV2::Structural {
                    reason: CutbackReason::NoLocalAttachment,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("code_source_record_mode"), "{err}");
        assert!(err.contains("refuses v2 activation writes"), "{err}");
    }

    #[test]
    fn mark_cutback_state_errors_on_missing_activation_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let err = store
            .mark_cutback_state(
                "project-a",
                CutbackStateV2::Structural {
                    reason: CutbackReason::NoLocalAttachment,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("code_source_cutback_state"), "{err}");
        assert!(
            err.contains("no activation record for project project-a"),
            "{err}"
        );
    }

    #[test]
    fn cutback_skip_serializing_none() {
        let generation = sample_v2_generation();
        let activation = sample_v2_activation(&generation);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&activation).unwrap()).unwrap();
        // The optional `cutback` field is skipped when None, so it must not
        // appear as a JSON key (cutback_pending is a separate required key).
        assert!(
            json.as_object().unwrap().get("cutback").is_none(),
            "cutback key should be absent when None"
        );
    }
}

/// Phase 3 P3-C blob-GC mode split (plan section 7 item 3, F8).
///
/// The daemon's hourly maintenance pass keeps calling the EMPTY-scope
/// `gc_blobs()` in bridge mode and only passes catalog scopes in catalog
/// mode. These tests pin why that asymmetry exists, so a later "unification"
/// has to delete an explicit assertion rather than a comment.
#[cfg(test)]
mod blob_gc_mode_tests {
    use super::tests::{descriptor, manifest_bytes, stored_generation_v1};
    use super::*;

    /// Local copy of the legacy-generation fixture: the parent test module's
    /// helper takes `CodeSourceStorePaths` directly, which is private to the
    /// store, so this writes through a store-derived path set instead.
    fn write_legacy_generation(
        paths: &CodeSourceStorePaths,
        state: GenerationState,
    ) -> StoredGeneration {
        let entries = Vec::new();
        let descriptor = descriptor(&entries);
        let mut record = stored_generation_v1("host-a", descriptor.clone());
        record.state = state;
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

    fn store_with_grace_zero(root: &Path) -> CodeSourceStore {
        CodeSourceStore::open(
            root.join("code-sources"),
            StoreLimits {
                unreferenced_blob_grace_hours: 0,
                ..StoreLimits::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn bridge_v1_store_gc_succeeds_with_empty_scopes_and_wedges_with_catalog_scopes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = store_with_grace_zero(&root);
        let generation = write_legacy_generation(&store.paths, GenerationState::Active);
        let scope = generation.descriptor.scope.clone();

        // Bridge parity: the empty-scope call is what the daemon keeps
        // making, and it stays on the legacy classifier arm.
        store.gc_blobs().unwrap();

        // The exact hazard the catalog-mode wiring must never hit on the
        // bridge: a non-empty scope set flips this store onto the mixed
        // classifier, which refuses every v1 row and would permanently wedge
        // blob GC for the whole bridge window.
        let error = store
            .gc_blobs_for_scopes(&BTreeSet::from([scope]))
            .expect_err("a v1-only store must refuse a catalog scope set");
        assert!(
            error
                .to_string()
                .contains("protected legacy generation lacks strict v2 ownership"),
            "{error}"
        );
    }

    #[test]
    fn catalog_scope_root_protects_a_retained_only_v2_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = store_with_grace_zero(&root);
        let legacy = write_legacy_generation(&store.paths, GenerationState::Superseded);
        let scope = legacy.descriptor.scope.clone();
        // Promote the row to a strict v2 record: retained-generation
        // protection is a v2-only arm by construction.
        let record = StoredGenerationV2::from_v1_for_migration(legacy, scope.clone()).unwrap();
        let metadata = store
            .paths
            .generation_metadata(&scope, &record.generation_id)
            .unwrap();
        fs::write(&metadata, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        let generation_dir = metadata.parent().unwrap().to_path_buf();

        // With no anchor, no activation, and no desired record, the scope is
        // not an authority scope at all, so the retained generation is
        // unprotected and reclaimed.
        let stats = store.gc_blobs_for_scopes(&BTreeSet::new()).unwrap();
        assert_eq!(stats.reclaimed_generations, 1);
        assert!(!generation_dir.exists());

        // Same shape, but the catalog scope set names the scope: the
        // retained generation is protected through its scope root. This is
        // the production behavior the hourly pass gains in catalog mode.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = store_with_grace_zero(&root);
        let legacy = write_legacy_generation(&store.paths, GenerationState::Superseded);
        let scope = legacy.descriptor.scope.clone();
        let record = StoredGenerationV2::from_v1_for_migration(legacy, scope.clone()).unwrap();
        let metadata = store
            .paths
            .generation_metadata(&scope, &record.generation_id)
            .unwrap();
        fs::write(&metadata, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        let generation_dir = metadata.parent().unwrap().to_path_buf();

        let stats = store.gc_blobs_for_scopes(&BTreeSet::from([scope])).unwrap();
        assert_eq!(stats.reclaimed_generations, 0);
        assert!(generation_dir.exists());
    }

    /// Section 7.1 item 3: catalog-mode upload finalization emits a
    /// `StoredGenerationV2` readable by the mixed read path. The
    /// desired pointer is also v2.
    #[test]
    fn catalog_mode_finalize_upload_emits_v2_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = CodeSourceStore::open_with_mode(
            root.join("code-sources"),
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let descriptor = descriptor(&[]);
        let scope = descriptor.scope.clone();

        let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        let mixed = store
            .finalize_upload_mixed("host-a", &upload.upload_id)
            .unwrap();

        // The finalized generation must be v2 in catalog mode.
        match &mixed {
            MixedStoredGeneration::CurrentV2(record) => {
                assert_eq!(record.state, GenerationState::Ready);
                assert_eq!(record.published_scope, scope);
            }
            MixedStoredGeneration::LegacyV1(_) => {
                panic!("catalog-mode finalize_upload must emit v2, not v1")
            }
        }

        let generation_id = mixed.generation_id();

        // find_generation_mixed reads it as v2.
        let found = store.find_generation_mixed(generation_id).unwrap();
        assert!(matches!(found, MixedStoredGeneration::CurrentV2(_)));

        // desired_generation_mixed reads the pointer as v2.
        let desired = store
            .desired_generation_mixed(&scope)
            .unwrap()
            .expect("desired pointer must exist after finalize");
        assert!(matches!(desired, MixedStoredGeneration::CurrentV2(_)));
        assert_eq!(desired.generation_id(), generation_id);
    }

    /// Section 7.1 item 3: bridge-mode upload finalization is
    /// byte-identical to the pre-P4C v1 path.
    #[test]
    fn bridge_mode_finalize_upload_emits_v1_generation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store =
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap();
        let descriptor = descriptor(&[]);
        let scope = descriptor.scope.clone();

        let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        let mixed = store
            .finalize_upload_mixed("host-a", &upload.upload_id)
            .unwrap();

        // The finalized generation must be v1 in bridge mode.
        match &mixed {
            MixedStoredGeneration::LegacyV1(record) => {
                assert_eq!(record.state, GenerationState::Ready);
                assert_eq!(record.descriptor.scope, scope);
            }
            MixedStoredGeneration::CurrentV2(_) => {
                panic!("bridge-mode finalize_upload must emit v1, not v2")
            }
        }

        // The existing v1 finalize_upload returns the same record.
        let v1 = store
            .desired_generation(&scope)
            .unwrap()
            .expect("desired pointer must exist");
        assert_eq!(v1.generation_id, mixed.generation_id());
        assert_eq!(v1.state, GenerationState::Ready);
    }
}
