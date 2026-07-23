//! Strict persistence codecs for accepted knowledge and gap publications.
//!
//! Phase 1 owns preparation and verification only. The project-catalog
//! transaction owner installs the returned immutable generation and mutable
//! pointer bytes. This module deliberately exposes no standalone write path.

#![allow(dead_code)] // P1-C seams are consumed by the later migration transaction slice.

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use bbox_chunker::EdgeConfidence;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow, canonical_store_lock_path,
};
use bbox_corpus_core::project_catalog::{AttachmentId, ProjectId};
use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapNote, GapResolution};
use bbox_knowledge::knowledge::{
    Approval, Category, KnowledgeEdgeKind, KnowledgeEntry, Priority, Scope, Status,
};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

const ACCEPTED_PUBLICATION_VERSION: u32 = 1;
const MAX_PROJECTS_BASENAME_BYTES: usize = 255;
const MAX_FULL_REF_BYTES: usize = 1024;
const MAX_RECORD_ID_BYTES: usize = 256;
const MAX_REPOSITORY_RELATIVE_FILENAME_BYTES: usize = 4096;

pub(crate) const MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE: usize = 100_000;
pub(crate) const MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_POINTER_BYTES: usize = 64 * 1024;

pub(crate) type AcceptedPublicationStoreResult<T> = Result<T, AcceptedPublicationStoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPublicationStoreError {
    code: &'static str,
    detail: String,
}

impl AcceptedPublicationStoreError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let detail = detail
            .into()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(512)
            .collect();
        Self { code, detail }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for AcceptedPublicationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for AcceptedPublicationStoreError {}

fn invalid_id(kind: &'static str) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new(
        "error.accepted_publication_invalid_id",
        format!("invalid {kind}"),
    )
}

fn invalid_generation(detail: impl Into<String>) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new("error.accepted_publication_invalid_generation", detail)
}

fn invalid_pointer(detail: impl Into<String>) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new("error.accepted_publication_invalid_pointer", detail)
}

fn byte_limit(label: &'static str) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new(
        "error.accepted_publication_byte_limit",
        format!("{label} exceeds its byte limit"),
    )
}

macro_rules! validated_string_type {
    ($name:ident, $validator:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn parse(value: impl Into<String>) -> AcceptedPublicationStoreResult<Self> {
                let value = value.into();
                $validator(&value)?;
                Ok(Self(value))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    };
}

fn validate_sha256(value: &str) -> AcceptedPublicationStoreResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_id("SHA-256"));
    }
    Ok(())
}

fn validate_git_object_id(value: &str) -> AcceptedPublicationStoreResult<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_id("accepted commit"));
    }
    Ok(())
}

fn validate_full_publisher_ref(value: &str) -> AcceptedPublicationStoreResult<()> {
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
        return Err(invalid_id("full publisher ref"));
    }
    let Some(branch) = value.strip_prefix("refs/heads/") else {
        return Err(invalid_id("full publisher ref"));
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
        return Err(invalid_id("full publisher ref"));
    }
    Ok(())
}

fn validate_record_id(value: &str) -> AcceptedPublicationStoreResult<()> {
    if value.is_empty()
        || value.len() > MAX_RECORD_ID_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_id("publication record id"));
    }
    Ok(())
}

fn validate_repository_relative_filename(value: &str) -> AcceptedPublicationStoreResult<()> {
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
        return Err(invalid_id("repository-relative publication filename"));
    }
    Ok(())
}

validated_string_type!(PublicationSha256, validate_sha256);
validated_string_type!(AcceptedPublicationGenerationId, validate_sha256);
validated_string_type!(GitObjectId, validate_git_object_id);
validated_string_type!(FullPublisherRef, validate_full_publisher_ref);
validated_string_type!(PublicationRecordId, validate_record_id);
validated_string_type!(
    NormalizedRepoRelativeFilename,
    validate_repository_relative_filename
);

impl PublicationSha256 {
    fn digest(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }
}

impl AcceptedPublicationGenerationId {
    fn digest(generation_bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"bbox-accepted-publication-generation-v1\0");
        hasher.update(generation_bytes);
        Self(hex::encode(hasher.finalize()))
    }
}

fn deserialize_unique_btree_map<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<K, V>(PhantomData<(K, V)>);

    impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
    where
        K: Deserialize<'de> + Ord,
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with unique keys")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = access.next_entry()? {
                if values.insert(key, value).is_some() {
                    return Err(de::Error::custom("duplicate strict-map key"));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgeCategoryV1 {
    Profile,
    Convention,
    Steering,
    Build,
    Tool,
    Memory,
    Workflow,
    Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgeScopeV1 {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgePriorityV1 {
    Critical,
    Standard,
    Supplementary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgeStatusV1 {
    Active,
    Draft,
    Superseded,
    Disabled,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgeApprovalV1 {
    UserConfirmed,
    AgentInferred,
    Imported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedKnowledgeEdgeKindV1 {
    Contradicts,
    RelatesTo,
    TensionWith,
    Supports,
    DependsOn,
    DerivedFrom,
    Supersedes,
    References,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedEdgeConfidenceV1 {
    Exact,
    Heuristic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedGapKindV1 {
    PacketAst,
    Tooling,
    Agent,
    Workflow,
    RefactorPrimitive,
    McpSurface,
    Ontology,
    EvalCoverage,
    DocsRunbook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedGapImpactV1 {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedBlockingLevelV1 {
    None,
    WorkaroundAvailable,
    BlocksTask,
    BlocksClassOfWork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AcceptedGapResolutionV1 {
    Unresolved,
    Acknowledged,
    Addressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedKnowledgeEdgeV1 {
    pub(crate) target: String,
    pub(crate) kind: AcceptedKnowledgeEdgeKindV1,
    pub(crate) note: Option<String>,
    pub(crate) source_arc: Option<String>,
    pub(crate) confidence: AcceptedEdgeConfidenceV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedKnowledgeEntryV1 {
    pub(crate) id: PublicationRecordId,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) cluster: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) variants: BTreeMap<String, String>,
    pub(crate) category: AcceptedKnowledgeCategoryV1,
    pub(crate) scope: AcceptedKnowledgeScopeV1,
    pub(crate) providers: Vec<String>,
    pub(crate) priority: AcceptedKnowledgePriorityV1,
    pub(crate) weight: u32,
    pub(crate) status: AcceptedKnowledgeStatusV1,
    pub(crate) approval: AcceptedKnowledgeApprovalV1,
    pub(crate) render: bool,
    pub(crate) decay: bool,
    pub(crate) review_at: Option<String>,
    pub(crate) supersedes: Option<String>,
    pub(crate) links: Vec<AcceptedKnowledgeEdgeV1>,
    pub(crate) rationale: Option<String>,
    pub(crate) expires_at: Option<String>,
    pub(crate) source: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedGapEntryV1 {
    pub(crate) id: PublicationRecordId,
    pub(crate) title: String,
    pub(crate) gap_kind: AcceptedGapKindV1,
    pub(crate) domain: String,
    pub(crate) wanted_capability: String,
    pub(crate) missing_primitive: Option<String>,
    pub(crate) fallback_used: Option<String>,
    pub(crate) evidence: Vec<String>,
    pub(crate) impact: AcceptedGapImpactV1,
    pub(crate) blocking_level: AcceptedBlockingLevelV1,
    pub(crate) dedupe_key: String,
    pub(crate) suggested_owner: Option<String>,
    pub(crate) notes: Option<String>,
    pub(crate) supersedes: Option<String>,
    pub(crate) superseded_by: Option<String>,
    pub(crate) resolution: AcceptedGapResolutionV1,
    pub(crate) task_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) bro: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) resolved_at: Option<String>,
    pub(crate) resolution_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationFileManifestEntryV1 {
    pub(crate) record_id: PublicationRecordId,
    pub(crate) source_content_sha256: PublicationSha256,
    pub(crate) normalized_record_sha256: PublicationSha256,
    pub(crate) encoded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationHashesV1 {
    pub(crate) knowledge_file_manifest_sha256: PublicationSha256,
    pub(crate) gap_file_manifest_sha256: PublicationSha256,
    pub(crate) normalized_knowledge_sha256: PublicationSha256,
    pub(crate) normalized_gaps_sha256: PublicationSha256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationCountsV1 {
    pub(crate) knowledge_files: u64,
    pub(crate) knowledge_entries: u64,
    pub(crate) gap_files: u64,
    pub(crate) gap_entries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationGenerationV1 {
    pub(crate) version: u32,
    pub(crate) project_id: ProjectId,
    pub(crate) scope: PublishedScope,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) knowledge_file_manifest:
        BTreeMap<NormalizedRepoRelativeFilename, PublicationFileManifestEntryV1>,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) gap_file_manifest:
        BTreeMap<NormalizedRepoRelativeFilename, PublicationFileManifestEntryV1>,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) normalized_knowledge: BTreeMap<PublicationRecordId, AcceptedKnowledgeEntryV1>,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) normalized_gaps: BTreeMap<PublicationRecordId, AcceptedGapEntryV1>,
    pub(crate) hashes: AcceptedPublicationHashesV1,
    pub(crate) counts: AcceptedPublicationCountsV1,
    pub(crate) total_encoded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationPriorPointerV1 {
    pub(crate) attachment_id: AttachmentId,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) accepted_scope: PublishedScope,
    pub(crate) accepted_generation: AcceptedPublicationGenerationId,
    pub(crate) generation_hash: PublicationSha256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationPointerV1 {
    pub(crate) version: u32,
    pub(crate) project_id: ProjectId,
    pub(crate) attachment_id: AttachmentId,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) accepted_scope: PublishedScope,
    pub(crate) accepted_generation: AcceptedPublicationGenerationId,
    pub(crate) generation_hash: PublicationSha256,
    pub(crate) prior_pointer: Option<AcceptedPublicationPriorPointerV1>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedKnowledgeSourceV1 {
    pub(crate) repository_relative_filename: String,
    pub(crate) source_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedGapSourceV1 {
    pub(crate) repository_relative_filename: String,
    pub(crate) source_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedPublicationBuildInputV1 {
    pub(crate) project_id: ProjectId,
    pub(crate) attachment_id: AttachmentId,
    pub(crate) scope: PublishedScope,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) knowledge: Vec<AcceptedKnowledgeSourceV1>,
    pub(crate) gaps: Vec<AcceptedGapSourceV1>,
    pub(crate) prior_pointer: Option<AcceptedPublicationPriorPointerV1>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedAcceptedPublicationV1 {
    pub(crate) generation_id: AcceptedPublicationGenerationId,
    pub(crate) generation: AcceptedPublicationGenerationV1,
    pub(crate) generation_bytes: Vec<u8>,
    pub(crate) generation_hash: PublicationSha256,
    pub(crate) pointer: AcceptedPublicationPointerV1,
    pub(crate) pointer_bytes: Vec<u8>,
    pub(crate) pointer_hash: PublicationSha256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcceptedPublicationLimits {
    pub(crate) max_source_file_bytes: u64,
    pub(crate) max_knowledge_entries: usize,
    pub(crate) max_gap_entries: usize,
    pub(crate) max_knowledge_source_bytes: u64,
    pub(crate) max_gap_source_bytes: u64,
    pub(crate) max_generation_bytes: usize,
    pub(crate) max_pointer_bytes: usize,
}

impl Default for AcceptedPublicationLimits {
    fn default() -> Self {
        Self {
            max_source_file_bytes: MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES,
            max_knowledge_entries: MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE,
            max_gap_entries: MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE,
            max_knowledge_source_bytes: MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE,
            max_gap_source_bytes: MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE,
            max_generation_bytes: MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES,
            max_pointer_bytes: MAX_ACCEPTED_PUBLICATION_POINTER_BYTES,
        }
    }
}

impl AcceptedPublicationLimits {
    fn validate(&self) -> AcceptedPublicationStoreResult<()> {
        let valid = self.max_source_file_bytes > 0
            && self.max_source_file_bytes <= MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES
            && self.max_knowledge_entries > 0
            && self.max_knowledge_entries <= MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE
            && self.max_gap_entries > 0
            && self.max_gap_entries <= MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE
            && self.max_knowledge_source_bytes > 0
            && self.max_knowledge_source_bytes <= MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE
            && self.max_gap_source_bytes > 0
            && self.max_gap_source_bytes <= MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE
            && self.max_generation_bytes > 0
            && self.max_generation_bytes <= MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES
            && self.max_pointer_bytes > 0
            && self.max_pointer_bytes <= MAX_ACCEPTED_PUBLICATION_POINTER_BYTES;
        if !valid {
            return Err(AcceptedPublicationStoreError::new(
                "error.accepted_publication_invalid_limits",
                "accepted-publication limits must be nonzero and may only lower hard ceilings",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPublicationStorePaths {
    anchor: PathBuf,
    root: PathBuf,
    pointers: PathBuf,
    generations: PathBuf,
    lock: PathBuf,
}

impl AcceptedPublicationStorePaths {
    pub(crate) fn derive(projects_path: &Path) -> AcceptedPublicationStoreResult<Self> {
        if !projects_path.is_absolute() {
            return Err(AcceptedPublicationStoreError::new(
                "error.accepted_publication_unsafe_path",
                "configured projects path must be absolute",
            ));
        }
        let parent = projects_path.parent().ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_unsafe_path",
                "configured projects path has no parent",
            )
        })?;
        let basename = projects_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| valid_basename(name))
            .ok_or_else(|| {
                AcceptedPublicationStoreError::new(
                    "error.accepted_publication_unsafe_path",
                    "configured projects filename is unsafe",
                )
            })?;
        if matches!(
            basename,
            "accepted-publications.json" | "accepted-publications"
        ) {
            return Err(AcceptedPublicationStoreError::new(
                "error.accepted_publication_path_collision",
                "configured projects filename collides with accepted-publication storage",
            ));
        }

        let anchor = parent.join("accepted-publications.json");
        let root = parent.join("accepted-publications");
        let paths = Self {
            lock: canonical_store_lock_path(&anchor),
            pointers: root.join("pointers"),
            generations: root.join("generations"),
            anchor,
            root,
        };
        let mut unique = vec![
            &paths.anchor,
            &paths.root,
            &paths.pointers,
            &paths.generations,
            &paths.lock,
            projects_path,
        ];
        unique.sort();
        unique.dedup();
        if unique.len() != 6 {
            return Err(AcceptedPublicationStoreError::new(
                "error.accepted_publication_path_collision",
                "derived accepted-publication paths are not unique",
            ));
        }
        Ok(paths)
    }

    pub(crate) fn anchor(&self) -> &Path {
        &self.anchor
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn pointers(&self) -> &Path {
        &self.pointers
    }

    pub(crate) fn generations(&self) -> &Path {
        &self.generations
    }

    pub(crate) fn lock(&self) -> &Path {
        &self.lock
    }

    pub(crate) fn pointer(&self, project_id: &ProjectId) -> PathBuf {
        self.pointers.join(format!("{project_id}.json"))
    }

    pub(crate) fn generation(
        &self,
        project_id: &ProjectId,
        generation_id: &AcceptedPublicationGenerationId,
    ) -> PathBuf {
        self.generations
            .join(project_id.as_str())
            .join(format!("{generation_id}.json"))
    }
}

fn valid_basename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROJECTS_BASENAME_BYTES
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
}

#[derive(Debug)]
pub(crate) struct AcceptedPublicationLockGuard {
    anchor: PathBuf,
    _guard: StoreLockGuard,
}

pub(crate) fn acquire_accepted_publication_lock(
    paths: &AcceptedPublicationStorePaths,
) -> AcceptedPublicationStoreResult<AcceptedPublicationLockGuard> {
    let guard = acquire_store_lock_nofollow(paths.anchor()).map_err(|error| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_io",
            format!("acquiring accepted-publication lock failed: {error}"),
        )
    })?;
    Ok(AcceptedPublicationLockGuard {
        anchor: paths.anchor.clone(),
        _guard: guard,
    })
}

fn ensure_matching_guard(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
) -> AcceptedPublicationStoreResult<()> {
    if guard.anchor != paths.anchor {
        return Err(AcceptedPublicationStoreError::new(
            "error.accepted_publication_wrong_lock",
            "accepted-publication guard belongs to a different store",
        ));
    }
    Ok(())
}

pub(crate) fn normalize_knowledge_entry_v1(
    entry: &KnowledgeEntry,
) -> AcceptedPublicationStoreResult<AcceptedKnowledgeEntryV1> {
    let id = PublicationRecordId::parse(entry.id.clone())?;
    let scope = match entry.scope {
        Scope::Global => {
            return Err(invalid_generation(
                "accepted project publication contains global knowledge",
            ));
        }
        Scope::Project => AcceptedKnowledgeScopeV1::Project,
    };
    let category = match &entry.category {
        Category::Profile => AcceptedKnowledgeCategoryV1::Profile,
        Category::Convention => AcceptedKnowledgeCategoryV1::Convention,
        Category::Steering => AcceptedKnowledgeCategoryV1::Steering,
        Category::Build => AcceptedKnowledgeCategoryV1::Build,
        Category::Tool => AcceptedKnowledgeCategoryV1::Tool,
        Category::Memory => AcceptedKnowledgeCategoryV1::Memory,
        Category::Workflow => AcceptedKnowledgeCategoryV1::Workflow,
        Category::Decision => AcceptedKnowledgeCategoryV1::Decision,
    };
    let priority = match &entry.priority {
        Priority::Critical => AcceptedKnowledgePriorityV1::Critical,
        Priority::Standard => AcceptedKnowledgePriorityV1::Standard,
        Priority::Supplementary => AcceptedKnowledgePriorityV1::Supplementary,
    };
    let status = match &entry.status {
        Status::Active => AcceptedKnowledgeStatusV1::Active,
        Status::Draft => AcceptedKnowledgeStatusV1::Draft,
        Status::Superseded => AcceptedKnowledgeStatusV1::Superseded,
        Status::Disabled => AcceptedKnowledgeStatusV1::Disabled,
        Status::Deleted => AcceptedKnowledgeStatusV1::Deleted,
    };
    let approval = match &entry.approval {
        Approval::UserConfirmed => AcceptedKnowledgeApprovalV1::UserConfirmed,
        Approval::AgentInferred => AcceptedKnowledgeApprovalV1::AgentInferred,
        Approval::Imported => AcceptedKnowledgeApprovalV1::Imported,
    };
    let links = entry
        .links
        .iter()
        .map(|edge| {
            let kind = match edge.kind {
                KnowledgeEdgeKind::Contradicts => AcceptedKnowledgeEdgeKindV1::Contradicts,
                KnowledgeEdgeKind::RelatesTo => AcceptedKnowledgeEdgeKindV1::RelatesTo,
                KnowledgeEdgeKind::TensionWith => AcceptedKnowledgeEdgeKindV1::TensionWith,
                KnowledgeEdgeKind::Supports => AcceptedKnowledgeEdgeKindV1::Supports,
                KnowledgeEdgeKind::DependsOn => AcceptedKnowledgeEdgeKindV1::DependsOn,
                KnowledgeEdgeKind::DerivedFrom => AcceptedKnowledgeEdgeKindV1::DerivedFrom,
                KnowledgeEdgeKind::Supersedes => AcceptedKnowledgeEdgeKindV1::Supersedes,
                KnowledgeEdgeKind::References => AcceptedKnowledgeEdgeKindV1::References,
            };
            let confidence = match edge.confidence {
                EdgeConfidence::Exact => AcceptedEdgeConfidenceV1::Exact,
                EdgeConfidence::Heuristic => AcceptedEdgeConfidenceV1::Heuristic,
                EdgeConfidence::Unknown => AcceptedEdgeConfidenceV1::Unknown,
            };
            AcceptedKnowledgeEdgeV1 {
                target: edge.target.clone(),
                kind,
                note: edge.note.clone(),
                source_arc: edge.source_arc.clone(),
                confidence,
            }
        })
        .collect();

    Ok(AcceptedKnowledgeEntryV1 {
        id,
        title: entry.title.clone(),
        content: entry.content.clone(),
        cluster: entry.cluster.clone(),
        variants: entry
            .variants
            .iter()
            .map(|(provider, content)| (provider.clone(), content.clone()))
            .collect(),
        category,
        scope,
        providers: entry.providers.clone(),
        priority,
        weight: entry.weight,
        status,
        approval,
        render: entry.render,
        decay: entry.decay,
        review_at: entry.review_at.clone(),
        supersedes: entry.supersedes.clone(),
        links,
        rationale: entry.rationale.clone(),
        expires_at: entry.expires_at.clone(),
        source: entry.source.clone(),
        created_at: entry.created_at.clone(),
        updated_at: entry.updated_at.clone(),
    })
}

pub(crate) fn normalize_gap_entry_v1(
    gap: &GapNote,
) -> AcceptedPublicationStoreResult<AcceptedGapEntryV1> {
    let id = PublicationRecordId::parse(gap.id.clone())?;
    if gap.title.trim().is_empty()
        || gap.domain.trim().is_empty()
        || gap.wanted_capability.trim().is_empty()
    {
        return Err(invalid_generation(
            "accepted gap is missing a required durable field",
        ));
    }
    validate_gap_dedupe_key(&gap.dedupe_key)?;
    let gap_kind = match gap.gap_kind {
        GapKind::PacketAst => AcceptedGapKindV1::PacketAst,
        GapKind::Tooling => AcceptedGapKindV1::Tooling,
        GapKind::Agent => AcceptedGapKindV1::Agent,
        GapKind::Workflow => AcceptedGapKindV1::Workflow,
        GapKind::RefactorPrimitive => AcceptedGapKindV1::RefactorPrimitive,
        GapKind::McpSurface => AcceptedGapKindV1::McpSurface,
        GapKind::Ontology => AcceptedGapKindV1::Ontology,
        GapKind::EvalCoverage => AcceptedGapKindV1::EvalCoverage,
        GapKind::DocsRunbook => AcceptedGapKindV1::DocsRunbook,
    };
    let impact = match gap.impact {
        GapImpact::Low => AcceptedGapImpactV1::Low,
        GapImpact::Medium => AcceptedGapImpactV1::Medium,
        GapImpact::High => AcceptedGapImpactV1::High,
        GapImpact::Critical => AcceptedGapImpactV1::Critical,
    };
    let blocking_level = match gap.blocking_level {
        BlockingLevel::None => AcceptedBlockingLevelV1::None,
        BlockingLevel::WorkaroundAvailable => AcceptedBlockingLevelV1::WorkaroundAvailable,
        BlockingLevel::BlocksTask => AcceptedBlockingLevelV1::BlocksTask,
        BlockingLevel::BlocksClassOfWork => AcceptedBlockingLevelV1::BlocksClassOfWork,
    };
    let resolution = match gap.resolution {
        GapResolution::Unresolved => AcceptedGapResolutionV1::Unresolved,
        GapResolution::Acknowledged => AcceptedGapResolutionV1::Acknowledged,
        GapResolution::Addressed => AcceptedGapResolutionV1::Addressed,
    };
    let updated_at = if gap.updated_at.is_empty() {
        gap.created_at.clone()
    } else {
        gap.updated_at.clone()
    };

    Ok(AcceptedGapEntryV1 {
        id,
        title: gap.title.clone(),
        gap_kind,
        domain: gap.domain.clone(),
        wanted_capability: gap.wanted_capability.clone(),
        missing_primitive: gap.missing_primitive.clone(),
        fallback_used: gap.fallback_used.clone(),
        evidence: gap.evidence.clone(),
        impact,
        blocking_level,
        dedupe_key: gap.dedupe_key.clone(),
        suggested_owner: gap.suggested_owner.clone(),
        notes: gap.notes.clone(),
        supersedes: gap.supersedes.clone(),
        superseded_by: gap.superseded_by.clone(),
        resolution,
        task_id: gap.task_id.clone(),
        session_id: gap.session_id.clone(),
        provider: gap.provider.clone(),
        bro: gap.bro.clone(),
        thread_id: gap.thread_id.clone(),
        created_at: gap.created_at.clone(),
        updated_at,
        resolved_at: gap.resolved_at.clone(),
        resolution_note: gap.resolution_note.clone(),
    })
}

fn validate_gap_dedupe_key(key: &str) -> AcceptedPublicationStoreResult<()> {
    let mut segments = key.split('/');
    if segments.by_ref().take(3).count() < 3
        || key.split('/').any(|segment| segment.trim().is_empty())
    {
        return Err(invalid_generation(
            "accepted gap has an invalid durable dedupe key",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum PublicationLane {
    Knowledge,
    Gaps,
}

fn expected_repository_relative_filename(
    scope: &PublishedScope,
    lane: PublicationLane,
    record_id: &PublicationRecordId,
) -> AcceptedPublicationStoreResult<NormalizedRepoRelativeFilename> {
    scope
        .validate()
        .map_err(|error| invalid_generation(error.to_string()))?;
    let lane = match lane {
        PublicationLane::Knowledge => "knowledge",
        PublicationLane::Gaps => "gaps",
    };
    let path = if scope.bbox_root_relpath() == "." {
        format!(".bbox/{lane}/{record_id}.json")
    } else {
        format!(
            "{}/.bbox/{lane}/{record_id}.json",
            scope.bbox_root_relpath()
        )
    };
    NormalizedRepoRelativeFilename::parse(path)
}

fn canonical_json_hash<T: Serialize>(
    value: &T,
    label: &'static str,
) -> AcceptedPublicationStoreResult<PublicationSha256> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_encode",
            format!("encoding {label} failed: {error}"),
        )
    })?;
    Ok(PublicationSha256::digest(&bytes))
}

fn encode_bounded_json<T: Serialize>(
    value: &T,
    max_bytes: usize,
    label: &'static str,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_encode",
            format!("encoding {label} failed: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > max_bytes {
        return Err(byte_limit(label));
    }
    Ok(bytes)
}

fn decode_bounded_json<T>(
    bytes: &[u8],
    max_bytes: usize,
    label: &'static str,
) -> AcceptedPublicationStoreResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    if bytes.len() > max_bytes {
        return Err(byte_limit(label));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_decode",
            format!("decoding {label} failed: {error}"),
        )
    })
}

fn usize_to_u64(value: usize, label: &'static str) -> AcceptedPublicationStoreResult<u64> {
    u64::try_from(value).map_err(|_| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_count_overflow",
            format!("{label} count does not fit in u64"),
        )
    })
}

pub(crate) fn prepare_accepted_publication_v1(
    input: AcceptedPublicationBuildInputV1,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<PreparedAcceptedPublicationV1> {
    limits.validate()?;
    input
        .scope
        .validate()
        .map_err(|error| invalid_generation(error.to_string()))?;
    if input.knowledge.len() > limits.max_knowledge_entries
        || input.gaps.len() > limits.max_gap_entries
    {
        return Err(AcceptedPublicationStoreError::new(
            "error.accepted_publication_entry_limit",
            "accepted publication exceeds a lane entry limit",
        ));
    }

    let mut knowledge_file_manifest = BTreeMap::new();
    let mut normalized_knowledge = BTreeMap::new();
    let mut knowledge_source_bytes = 0_u64;
    for source in input.knowledge {
        let encoded_bytes = u64::try_from(source.source_bytes.len())
            .map_err(|_| byte_limit("accepted knowledge source file"))?;
        if encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted knowledge source file"));
        }
        knowledge_source_bytes = knowledge_source_bytes
            .checked_add(encoded_bytes)
            .ok_or_else(|| byte_limit("accepted knowledge source lane"))?;
        if knowledge_source_bytes > limits.max_knowledge_source_bytes {
            return Err(byte_limit("accepted knowledge source lane"));
        }
        let entry: KnowledgeEntry =
            serde_json::from_slice(&source.source_bytes).map_err(|error| {
                invalid_generation(format!(
                    "accepted knowledge source is invalid JSON: {error}"
                ))
            })?;
        let normalized = normalize_knowledge_entry_v1(&entry)?;
        let expected = expected_repository_relative_filename(
            &input.scope,
            PublicationLane::Knowledge,
            &normalized.id,
        )?;
        let supplied = NormalizedRepoRelativeFilename::parse(source.repository_relative_filename)?;
        if supplied != expected {
            return Err(invalid_generation(
                "accepted knowledge filename does not match its scope and record id",
            ));
        }
        let record_id = normalized.id.clone();
        let normalized_record_sha256 =
            canonical_json_hash(&normalized, "accepted knowledge record")?;
        if normalized_knowledge
            .insert(record_id.clone(), normalized)
            .is_some()
            || knowledge_file_manifest
                .insert(
                    supplied,
                    PublicationFileManifestEntryV1 {
                        record_id,
                        source_content_sha256: PublicationSha256::digest(&source.source_bytes),
                        normalized_record_sha256,
                        encoded_bytes,
                    },
                )
                .is_some()
        {
            return Err(invalid_generation(
                "accepted knowledge publication contains a duplicate record",
            ));
        }
    }

    let mut gap_file_manifest = BTreeMap::new();
    let mut normalized_gaps = BTreeMap::new();
    let mut gap_source_bytes = 0_u64;
    for source in input.gaps {
        let encoded_bytes = u64::try_from(source.source_bytes.len())
            .map_err(|_| byte_limit("accepted gap source file"))?;
        if encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted gap source file"));
        }
        gap_source_bytes = gap_source_bytes
            .checked_add(encoded_bytes)
            .ok_or_else(|| byte_limit("accepted gap source lane"))?;
        if gap_source_bytes > limits.max_gap_source_bytes {
            return Err(byte_limit("accepted gap source lane"));
        }
        let gap: GapNote = serde_json::from_slice(&source.source_bytes).map_err(|error| {
            invalid_generation(format!("accepted gap source is invalid JSON: {error}"))
        })?;
        let normalized = normalize_gap_entry_v1(&gap)?;
        let expected = expected_repository_relative_filename(
            &input.scope,
            PublicationLane::Gaps,
            &normalized.id,
        )?;
        let supplied = NormalizedRepoRelativeFilename::parse(source.repository_relative_filename)?;
        if supplied != expected {
            return Err(invalid_generation(
                "accepted gap filename does not match its scope and record id",
            ));
        }
        let record_id = normalized.id.clone();
        let normalized_record_sha256 = canonical_json_hash(&normalized, "accepted gap record")?;
        if normalized_gaps
            .insert(record_id.clone(), normalized)
            .is_some()
            || gap_file_manifest
                .insert(
                    supplied,
                    PublicationFileManifestEntryV1 {
                        record_id,
                        source_content_sha256: PublicationSha256::digest(&source.source_bytes),
                        normalized_record_sha256,
                        encoded_bytes,
                    },
                )
                .is_some()
        {
            return Err(invalid_generation(
                "accepted gap publication contains a duplicate record",
            ));
        }
    }

    let total_encoded_bytes = knowledge_source_bytes
        .checked_add(gap_source_bytes)
        .ok_or_else(|| byte_limit("accepted publication source total"))?;
    let hashes = AcceptedPublicationHashesV1 {
        knowledge_file_manifest_sha256: canonical_json_hash(
            &knowledge_file_manifest,
            "accepted knowledge manifest",
        )?,
        gap_file_manifest_sha256: canonical_json_hash(&gap_file_manifest, "accepted gap manifest")?,
        normalized_knowledge_sha256: canonical_json_hash(
            &normalized_knowledge,
            "normalized accepted knowledge",
        )?,
        normalized_gaps_sha256: canonical_json_hash(&normalized_gaps, "normalized accepted gaps")?,
    };
    let counts = AcceptedPublicationCountsV1 {
        knowledge_files: usize_to_u64(knowledge_file_manifest.len(), "accepted knowledge file")?,
        knowledge_entries: usize_to_u64(normalized_knowledge.len(), "accepted knowledge entry")?,
        gap_files: usize_to_u64(gap_file_manifest.len(), "accepted gap file")?,
        gap_entries: usize_to_u64(normalized_gaps.len(), "accepted gap entry")?,
    };
    let generation = AcceptedPublicationGenerationV1 {
        version: ACCEPTED_PUBLICATION_VERSION,
        project_id: input.project_id.clone(),
        scope: input.scope.clone(),
        full_ref: input.full_ref.clone(),
        accepted_commit: input.accepted_commit.clone(),
        knowledge_file_manifest,
        gap_file_manifest,
        normalized_knowledge,
        normalized_gaps,
        hashes,
        counts,
        total_encoded_bytes,
    };
    validate_generation_v1(&generation, limits)?;
    let generation_bytes = encode_bounded_json(
        &generation,
        limits.max_generation_bytes,
        "accepted publication generation",
    )?;
    let generation_hash = PublicationSha256::digest(&generation_bytes);
    let generation_id = AcceptedPublicationGenerationId::digest(&generation_bytes);
    if input.prior_pointer.as_ref().is_some_and(|prior| {
        prior.accepted_generation == generation_id || prior.generation_hash == generation_hash
    }) {
        return Err(invalid_pointer(
            "prior pointer must name a distinct accepted generation",
        ));
    }
    if let Some(prior) = &input.prior_pointer {
        validate_prior_pointer_v1(prior)?;
    }
    let pointer = AcceptedPublicationPointerV1 {
        version: ACCEPTED_PUBLICATION_VERSION,
        project_id: input.project_id,
        attachment_id: input.attachment_id,
        full_ref: input.full_ref,
        accepted_commit: input.accepted_commit,
        accepted_scope: input.scope,
        accepted_generation: generation_id.clone(),
        generation_hash: generation_hash.clone(),
        prior_pointer: input.prior_pointer,
    };
    validate_pointer_v1(&pointer)?;
    let pointer_bytes = encode_bounded_json(
        &pointer,
        limits.max_pointer_bytes,
        "accepted publication pointer",
    )?;
    let pointer_hash = PublicationSha256::digest(&pointer_bytes);
    Ok(PreparedAcceptedPublicationV1 {
        generation_id,
        generation,
        generation_bytes,
        generation_hash,
        pointer,
        pointer_bytes,
        pointer_hash,
    })
}

pub(crate) fn encode_generation_v1(
    generation: &AcceptedPublicationGenerationV1,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    limits.validate()?;
    validate_generation_v1(generation, limits)?;
    encode_bounded_json(
        generation,
        limits.max_generation_bytes,
        "accepted publication generation",
    )
}

pub(crate) fn decode_generation_v1(
    bytes: &[u8],
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationGenerationV1> {
    limits.validate()?;
    let generation = decode_bounded_json(
        bytes,
        limits.max_generation_bytes,
        "accepted publication generation",
    )?;
    validate_generation_v1(&generation, limits)?;
    Ok(generation)
}

pub(crate) fn encode_pointer_v1(
    pointer: &AcceptedPublicationPointerV1,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    limits.validate()?;
    validate_pointer_v1(pointer)?;
    encode_bounded_json(
        pointer,
        limits.max_pointer_bytes,
        "accepted publication pointer",
    )
}

pub(crate) fn decode_pointer_v1(
    bytes: &[u8],
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationPointerV1> {
    limits.validate()?;
    let pointer = decode_bounded_json(
        bytes,
        limits.max_pointer_bytes,
        "accepted publication pointer",
    )?;
    validate_pointer_v1(&pointer)?;
    Ok(pointer)
}

fn validate_generation_v1(
    generation: &AcceptedPublicationGenerationV1,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<()> {
    if generation.version != ACCEPTED_PUBLICATION_VERSION {
        return Err(invalid_generation(
            "accepted publication generation has an unsupported version",
        ));
    }
    generation
        .scope
        .validate()
        .map_err(|error| invalid_generation(error.to_string()))?;
    if generation.knowledge_file_manifest.len() > limits.max_knowledge_entries
        || generation.normalized_knowledge.len() > limits.max_knowledge_entries
        || generation.gap_file_manifest.len() > limits.max_gap_entries
        || generation.normalized_gaps.len() > limits.max_gap_entries
    {
        return Err(AcceptedPublicationStoreError::new(
            "error.accepted_publication_entry_limit",
            "accepted publication generation exceeds a lane entry limit",
        ));
    }
    if generation.knowledge_file_manifest.len() != generation.normalized_knowledge.len()
        || generation.gap_file_manifest.len() != generation.normalized_gaps.len()
    {
        return Err(invalid_generation(
            "accepted publication manifest and normalized record counts disagree",
        ));
    }

    let mut knowledge_source_bytes = 0_u64;
    for (filename, manifest) in &generation.knowledge_file_manifest {
        if manifest.encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted knowledge source file"));
        }
        knowledge_source_bytes = knowledge_source_bytes
            .checked_add(manifest.encoded_bytes)
            .ok_or_else(|| byte_limit("accepted knowledge source lane"))?;
        let normalized = generation
            .normalized_knowledge
            .get(&manifest.record_id)
            .ok_or_else(|| {
                invalid_generation("accepted knowledge manifest names a missing normalized record")
            })?;
        if normalized.id != manifest.record_id
            || normalized.scope != AcceptedKnowledgeScopeV1::Project
        {
            return Err(invalid_generation(
                "accepted knowledge map key, record id, or scope disagrees",
            ));
        }
        let expected = expected_repository_relative_filename(
            &generation.scope,
            PublicationLane::Knowledge,
            &manifest.record_id,
        )?;
        if filename != &expected {
            return Err(invalid_generation(
                "accepted knowledge manifest filename is not canonical",
            ));
        }
        if canonical_json_hash(normalized, "accepted knowledge record")?
            != manifest.normalized_record_sha256
        {
            return Err(invalid_generation(
                "accepted knowledge normalized record hash disagrees",
            ));
        }
    }
    if knowledge_source_bytes > limits.max_knowledge_source_bytes {
        return Err(byte_limit("accepted knowledge source lane"));
    }
    for (record_id, normalized) in &generation.normalized_knowledge {
        if record_id != &normalized.id {
            return Err(invalid_generation(
                "accepted knowledge normalized map key disagrees with its record id",
            ));
        }
    }

    let mut gap_source_bytes = 0_u64;
    for (filename, manifest) in &generation.gap_file_manifest {
        if manifest.encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted gap source file"));
        }
        gap_source_bytes = gap_source_bytes
            .checked_add(manifest.encoded_bytes)
            .ok_or_else(|| byte_limit("accepted gap source lane"))?;
        let normalized = generation
            .normalized_gaps
            .get(&manifest.record_id)
            .ok_or_else(|| {
                invalid_generation("accepted gap manifest names a missing normalized record")
            })?;
        validate_normalized_gap_v1(normalized)?;
        if normalized.id != manifest.record_id {
            return Err(invalid_generation(
                "accepted gap map key and record id disagree",
            ));
        }
        let expected = expected_repository_relative_filename(
            &generation.scope,
            PublicationLane::Gaps,
            &manifest.record_id,
        )?;
        if filename != &expected {
            return Err(invalid_generation(
                "accepted gap manifest filename is not canonical",
            ));
        }
        if canonical_json_hash(normalized, "accepted gap record")?
            != manifest.normalized_record_sha256
        {
            return Err(invalid_generation(
                "accepted gap normalized record hash disagrees",
            ));
        }
    }
    if gap_source_bytes > limits.max_gap_source_bytes {
        return Err(byte_limit("accepted gap source lane"));
    }
    for (record_id, normalized) in &generation.normalized_gaps {
        if record_id != &normalized.id {
            return Err(invalid_generation(
                "accepted gap normalized map key disagrees with its record id",
            ));
        }
    }

    let expected_total = knowledge_source_bytes
        .checked_add(gap_source_bytes)
        .ok_or_else(|| byte_limit("accepted publication source total"))?;
    if generation.total_encoded_bytes != expected_total {
        return Err(invalid_generation(
            "accepted publication total encoded bytes disagrees",
        ));
    }
    let expected_counts = AcceptedPublicationCountsV1 {
        knowledge_files: usize_to_u64(
            generation.knowledge_file_manifest.len(),
            "accepted knowledge file",
        )?,
        knowledge_entries: usize_to_u64(
            generation.normalized_knowledge.len(),
            "accepted knowledge entry",
        )?,
        gap_files: usize_to_u64(generation.gap_file_manifest.len(), "accepted gap file")?,
        gap_entries: usize_to_u64(generation.normalized_gaps.len(), "accepted gap entry")?,
    };
    if generation.counts != expected_counts {
        return Err(invalid_generation(
            "accepted publication stored counts disagree",
        ));
    }
    let expected_hashes = AcceptedPublicationHashesV1 {
        knowledge_file_manifest_sha256: canonical_json_hash(
            &generation.knowledge_file_manifest,
            "accepted knowledge manifest",
        )?,
        gap_file_manifest_sha256: canonical_json_hash(
            &generation.gap_file_manifest,
            "accepted gap manifest",
        )?,
        normalized_knowledge_sha256: canonical_json_hash(
            &generation.normalized_knowledge,
            "normalized accepted knowledge",
        )?,
        normalized_gaps_sha256: canonical_json_hash(
            &generation.normalized_gaps,
            "normalized accepted gaps",
        )?,
    };
    if generation.hashes != expected_hashes {
        return Err(invalid_generation(
            "accepted publication aggregate hashes disagree",
        ));
    }
    Ok(())
}

fn validate_normalized_gap_v1(gap: &AcceptedGapEntryV1) -> AcceptedPublicationStoreResult<()> {
    if gap.title.trim().is_empty()
        || gap.domain.trim().is_empty()
        || gap.wanted_capability.trim().is_empty()
        || gap.updated_at.is_empty()
    {
        return Err(invalid_generation(
            "accepted normalized gap is missing a required durable field",
        ));
    }
    validate_gap_dedupe_key(&gap.dedupe_key)
}

fn validate_prior_pointer_v1(
    pointer: &AcceptedPublicationPriorPointerV1,
) -> AcceptedPublicationStoreResult<()> {
    pointer
        .accepted_scope
        .validate()
        .map_err(|error| invalid_pointer(error.to_string()))
}

fn validate_pointer_v1(
    pointer: &AcceptedPublicationPointerV1,
) -> AcceptedPublicationStoreResult<()> {
    if pointer.version != ACCEPTED_PUBLICATION_VERSION {
        return Err(invalid_pointer(
            "accepted publication pointer has an unsupported version",
        ));
    }
    pointer
        .accepted_scope
        .validate()
        .map_err(|error| invalid_pointer(error.to_string()))?;
    if let Some(prior) = &pointer.prior_pointer {
        validate_prior_pointer_v1(prior)?;
        if prior.accepted_generation == pointer.accepted_generation
            || prior.generation_hash == pointer.generation_hash
        {
            return Err(invalid_pointer(
                "prior pointer must name a distinct accepted generation",
            ));
        }
    }
    Ok(())
}

pub(crate) fn verify_pointer_generation_v1(
    pointer: &AcceptedPublicationPointerV1,
    generation_bytes: &[u8],
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationGenerationV1> {
    validate_pointer_v1(pointer)?;
    verify_generation_binding(
        &pointer.project_id,
        &pointer.full_ref,
        &pointer.accepted_commit,
        &pointer.accepted_scope,
        &pointer.accepted_generation,
        &pointer.generation_hash,
        generation_bytes,
        limits,
    )
}

fn verify_prior_generation_v1(
    project_id: &ProjectId,
    pointer: &AcceptedPublicationPriorPointerV1,
    generation_bytes: &[u8],
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationGenerationV1> {
    validate_prior_pointer_v1(pointer)?;
    verify_generation_binding(
        project_id,
        &pointer.full_ref,
        &pointer.accepted_commit,
        &pointer.accepted_scope,
        &pointer.accepted_generation,
        &pointer.generation_hash,
        generation_bytes,
        limits,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_generation_binding(
    project_id: &ProjectId,
    full_ref: &FullPublisherRef,
    accepted_commit: &GitObjectId,
    accepted_scope: &PublishedScope,
    generation_id: &AcceptedPublicationGenerationId,
    generation_hash: &PublicationSha256,
    generation_bytes: &[u8],
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationGenerationV1> {
    if &PublicationSha256::digest(generation_bytes) != generation_hash {
        return Err(invalid_generation(
            "accepted publication generation byte hash disagrees",
        ));
    }
    if &AcceptedPublicationGenerationId::digest(generation_bytes) != generation_id {
        return Err(invalid_generation(
            "accepted publication generation id disagrees",
        ));
    }
    let generation = decode_generation_v1(generation_bytes, limits)?;
    if &generation.project_id != project_id
        || &generation.full_ref != full_ref
        || &generation.accepted_commit != accepted_commit
        || &generation.scope != accepted_scope
    {
        return Err(invalid_generation(
            "accepted publication pointer and generation binding disagree",
        ));
    }
    Ok(generation)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifiedAcceptedPublicationSelectionV1 {
    Current,
    Prior,
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedAcceptedPublicationV1 {
    pub(crate) selection: VerifiedAcceptedPublicationSelectionV1,
    pub(crate) pointer: AcceptedPublicationPointerV1,
    pub(crate) generation_id: AcceptedPublicationGenerationId,
    pub(crate) generation: AcceptedPublicationGenerationV1,
    pub(crate) generation_bytes: Vec<u8>,
}

pub(crate) fn verify_installed_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    expected_pointer_sha256: &PublicationSha256,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<VerifiedAcceptedPublicationV1> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let pointer_bytes = read_pointer_locked(paths, project_id, limits.max_pointer_bytes)?;
    if &PublicationSha256::digest(&pointer_bytes) != expected_pointer_sha256 {
        return Err(invalid_pointer(
            "installed accepted publication pointer hash disagrees",
        ));
    }
    verify_selected_from_pointer_locked(paths, project_id, pointer_bytes, limits)
}

pub(crate) fn verify_selected_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<VerifiedAcceptedPublicationV1> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let pointer_bytes = read_pointer_locked(paths, project_id, limits.max_pointer_bytes)?;
    verify_selected_from_pointer_locked(paths, project_id, pointer_bytes, limits)
}

fn verify_selected_from_pointer_locked(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    pointer_bytes: Vec<u8>,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<VerifiedAcceptedPublicationV1> {
    let pointer = decode_pointer_v1(&pointer_bytes, limits)?;
    if &pointer.project_id != project_id {
        return Err(invalid_pointer(
            "accepted publication pointer project id disagrees with its path",
        ));
    }
    let current_bytes = read_generation_locked(
        paths,
        project_id,
        &pointer.accepted_generation,
        limits.max_generation_bytes,
    );
    if let Ok(generation_bytes) = current_bytes
        && let Ok(generation) = verify_pointer_generation_v1(&pointer, &generation_bytes, limits)
    {
        return Ok(VerifiedAcceptedPublicationV1 {
            selection: VerifiedAcceptedPublicationSelectionV1::Current,
            generation_id: pointer.accepted_generation.clone(),
            pointer,
            generation,
            generation_bytes,
        });
    }

    let Some(prior) = pointer.prior_pointer.as_ref() else {
        return Err(invalid_generation(
            "current accepted publication generation did not verify",
        ));
    };
    let generation_bytes = read_generation_locked(
        paths,
        project_id,
        &prior.accepted_generation,
        limits.max_generation_bytes,
    )?;
    let generation = verify_prior_generation_v1(project_id, prior, &generation_bytes, limits)
        .map_err(|_| {
            invalid_generation("neither current nor prior accepted publication generation verified")
        })?;
    Ok(VerifiedAcceptedPublicationV1 {
        selection: VerifiedAcceptedPublicationSelectionV1::Prior,
        generation_id: prior.accepted_generation.clone(),
        pointer,
        generation,
        generation_bytes,
    })
}

fn read_pointer_locked(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    max_bytes: usize,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    let directory = NofollowDirectory::open_existing(paths.pointers())
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication pointer directory is missing",
            )
        })?;
    let filename = format!("{project_id}.json");
    let bytes = directory
        .read_regular(&filename, max_bytes, "accepted-publication pointer")
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication pointer is missing",
            )
        })?;
    directory
        .ensure_still_current()
        .map_err(accepted_io_error)?;
    Ok(bytes)
}

fn read_generation_locked(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    generation_id: &AcceptedPublicationGenerationId,
    max_bytes: usize,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    let project_generations = paths.generations().join(project_id.as_str());
    let directory = NofollowDirectory::open_existing(&project_generations)
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication generation directory is missing",
            )
        })?;
    let filename = format!("{generation_id}.json");
    let bytes = directory
        .read_regular(&filename, max_bytes, "accepted-publication generation")
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication generation is missing",
            )
        })?;
    directory
        .ensure_still_current()
        .map_err(accepted_io_error)?;
    Ok(bytes)
}

fn accepted_io_error(error: impl fmt::Display) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new(
        "error.accepted_publication_io",
        format!("accepted-publication I/O failed: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use bbox_knowledge::knowledge::KnowledgeEdge;

    use super::*;

    fn project_id() -> ProjectId {
        ProjectId::parse("p_example").unwrap()
    }

    fn attachment_id() -> AttachmentId {
        AttachmentId::parse("att_0123456789abcdef0123456789abcdef").unwrap()
    }

    fn scope(relative: &str) -> PublishedScope {
        PublishedScope::try_new("repo_example", relative).unwrap()
    }

    fn knowledge(id: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.to_string(),
            title: "Keep publication strict".to_string(),
            content: "Accepted bytes are detached from checkout paths.".to_string(),
            cluster: Some("runtime".to_string()),
            variants: HashMap::from([
                ("zeta".to_string(), "last".to_string()),
                ("alpha".to_string(), "first".to_string()),
            ]),
            category: Category::Convention,
            scope: Scope::Project,
            project: Some("/temporary/checkout".to_string()),
            providers: vec!["provider-a".to_string()],
            priority: Priority::Critical,
            weight: 140,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: Some("2030-01-01T00:00:00Z".to_string()),
            supersedes: None,
            links: vec![KnowledgeEdge {
                target: "other".to_string(),
                kind: KnowledgeEdgeKind::DependsOn,
                note: Some("ordered".to_string()),
                source_arc: Some("arc-example".to_string()),
                confidence: EdgeConfidence::Exact,
            }],
            rationale: Some("one authority".to_string()),
            expires_at: None,
            source: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            recall_count: 99,
            last_recalled: Some("2026-01-03T00:00:00Z".to_string()),
        }
    }

    fn gap(id: &str) -> GapNote {
        GapNote {
            id: id.to_string(),
            title: "Need bounded committed reads".to_string(),
            gap_kind: GapKind::Tooling,
            domain: "publication".to_string(),
            wanted_capability: "Read a committed blob with a byte ceiling".to_string(),
            missing_primitive: Some("bounded git object read".to_string()),
            fallback_used: None,
            evidence: vec!["unbounded output allocation".to_string()],
            impact: GapImpact::High,
            blocking_level: BlockingLevel::WorkaroundAvailable,
            dedupe_key: "tooling/publication/bounded-read".to_string(),
            suggested_owner: Some("corpus".to_string()),
            notes: Some("keep the source hash".to_string()),
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Acknowledged,
            project: Some("/temporary/checkout".to_string()),
            write_dir: Some("/temporary/carrier".to_string()),
            provisional_checkout_id: Some("checkout-local".to_string()),
            task_id: Some("task-example".to_string()),
            session_id: Some("session-example".to_string()),
            provider: Some("provider-a".to_string()),
            bro: Some("reviewer".to_string()),
            thread_id: Some("thread-example".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: String::new(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    fn knowledge_source(id: &str, relative: &str) -> AcceptedKnowledgeSourceV1 {
        AcceptedKnowledgeSourceV1 {
            repository_relative_filename: relative.to_string(),
            source_bytes: serde_json::to_vec(&knowledge(id)).unwrap(),
        }
    }

    fn gap_source(id: &str, relative: &str) -> AcceptedGapSourceV1 {
        AcceptedGapSourceV1 {
            repository_relative_filename: relative.to_string(),
            source_bytes: serde_json::to_vec(&gap(id)).unwrap(),
        }
    }

    fn build_input() -> AcceptedPublicationBuildInputV1 {
        AcceptedPublicationBuildInputV1 {
            project_id: project_id(),
            attachment_id: attachment_id(),
            scope: scope("."),
            full_ref: FullPublisherRef::parse("refs/heads/main").unwrap(),
            accepted_commit: GitObjectId::parse("a".repeat(40)).unwrap(),
            knowledge: vec![knowledge_source(
                "knowledge-a",
                ".bbox/knowledge/knowledge-a.json",
            )],
            gaps: vec![gap_source("gap-1234abcd", ".bbox/gaps/gap-1234abcd.json")],
            prior_pointer: None,
        }
    }

    fn prepared() -> PreparedAcceptedPublicationV1 {
        prepare_accepted_publication_v1(build_input(), &AcceptedPublicationLimits::default())
            .unwrap()
    }

    #[test]
    fn paths_are_fixed_siblings_and_derivation_has_no_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects = root.join("custom-projects.json");
        let paths = AcceptedPublicationStorePaths::derive(&projects).unwrap();

        assert_eq!(paths.anchor(), root.join("accepted-publications.json"));
        assert_eq!(paths.lock(), root.join("accepted-publications.json.lock"));
        assert_eq!(paths.root(), root.join("accepted-publications"));
        assert_eq!(
            paths.pointer(&project_id()),
            root.join("accepted-publications/pointers/p_example.json")
        );
        assert_eq!(
            paths.generation(
                &project_id(),
                &AcceptedPublicationGenerationId::parse("b".repeat(64)).unwrap()
            ),
            root.join(format!(
                "accepted-publications/generations/p_example/{}.json",
                "b".repeat(64)
            ))
        );
        assert!(!paths.root().exists());
        assert!(!paths.lock().exists());
    }

    #[test]
    fn paths_reject_relative_and_colliding_catalog_paths() {
        assert!(AcceptedPublicationStorePaths::derive(Path::new("projects.json")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(
            AcceptedPublicationStorePaths::derive(&root.join("accepted-publications.json"))
                .is_err()
        );
        assert!(
            AcceptedPublicationStorePaths::derive(&root.join("accepted-publications")).is_err()
        );
    }

    #[test]
    fn validators_reject_unsafe_ids_refs_and_filenames() {
        for invalid in [
            "main",
            "refs/heads/",
            "refs/heads/a..b",
            "refs/heads/a.lock",
            "refs/heads/a b",
            "refs/heads/a@{b",
        ] {
            assert!(FullPublisherRef::parse(invalid).is_err(), "{invalid}");
        }
        assert!(GitObjectId::parse("A".repeat(40)).is_err());
        assert!(GitObjectId::parse("a".repeat(39)).is_err());
        assert!(PublicationRecordId::parse("../escape").is_err());
        assert!(NormalizedRepoRelativeFilename::parse("/absolute.json").is_err());
        assert!(NormalizedRepoRelativeFilename::parse("a/../b.json").is_err());
    }

    #[test]
    fn knowledge_normalization_is_frozen_and_drops_host_fields() {
        let normalized = normalize_knowledge_entry_v1(&knowledge("knowledge-a")).unwrap();
        assert_eq!(
            normalized.variants.keys().cloned().collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(normalized.scope, AcceptedKnowledgeScopeV1::Project);
        assert_eq!(
            normalized.links[0].kind,
            AcceptedKnowledgeEdgeKindV1::DependsOn
        );
        let json = serde_json::to_value(normalized).unwrap();
        assert!(json.get("project").is_none());
        assert!(json.get("recall_count").is_none());
        assert!(json.get("last_recalled").is_none());
    }

    #[test]
    fn global_knowledge_cannot_enter_an_accepted_project_generation() {
        let mut entry = knowledge("knowledge-a");
        entry.scope = Scope::Global;
        assert!(normalize_knowledge_entry_v1(&entry).is_err());
    }

    #[test]
    fn gap_normalization_drops_host_fields_and_backfills_updated_at() {
        let normalized = normalize_gap_entry_v1(&gap("gap-1234abcd")).unwrap();
        assert_eq!(normalized.updated_at, normalized.created_at);
        assert_eq!(
            normalized.blocking_level,
            AcceptedBlockingLevelV1::WorkaroundAvailable
        );
        let json = serde_json::to_value(normalized).unwrap();
        assert!(json.get("project").is_none());
        assert!(json.get("write_dir").is_none());
        assert!(json.get("provisional_checkout_id").is_none());
        assert_eq!(json["task_id"], "task-example");
    }

    #[test]
    fn preparation_is_deterministic_and_counts_exact_source_bytes() {
        let input = build_input();
        let expected_bytes =
            input.knowledge[0].source_bytes.len() + input.gaps[0].source_bytes.len();
        let expected_knowledge_hash = PublicationSha256::digest(&input.knowledge[0].source_bytes);
        let first =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        let second = prepared();
        assert_eq!(first.generation_bytes, second.generation_bytes);
        assert_eq!(first.pointer_bytes, second.pointer_bytes);
        assert_eq!(first.generation_id, second.generation_id);
        assert_eq!(first.generation.total_encoded_bytes, expected_bytes as u64);
        assert_eq!(first.generation.counts.knowledge_files, 1);
        assert_eq!(first.generation.counts.gap_entries, 1);
        assert_eq!(
            first
                .generation
                .knowledge_file_manifest
                .values()
                .next()
                .unwrap()
                .source_content_sha256,
            expected_knowledge_hash
        );
        assert_ne!(first.generation_id.as_str(), first.generation_hash.as_str());
        assert_eq!(
            decode_generation_v1(
                &first.generation_bytes,
                &AcceptedPublicationLimits::default()
            )
            .unwrap(),
            first.generation
        );
        assert_eq!(
            decode_pointer_v1(&first.pointer_bytes, &AcceptedPublicationLimits::default()).unwrap(),
            first.pointer
        );
    }

    #[test]
    fn input_order_does_not_change_generation() {
        let mut input = build_input();
        input.knowledge.push(knowledge_source(
            "knowledge-b",
            ".bbox/knowledge/knowledge-b.json",
        ));
        let mut reversed = input.clone();
        reversed.knowledge.reverse();
        let first =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        let second =
            prepare_accepted_publication_v1(reversed, &AcceptedPublicationLimits::default())
                .unwrap();
        assert_eq!(first.generation_bytes, second.generation_bytes);
        assert_eq!(first.generation_id, second.generation_id);
    }

    #[test]
    fn subproject_manifest_keys_are_repository_relative() {
        let mut input = build_input();
        input.scope = scope("services/api");
        input.knowledge[0].repository_relative_filename =
            "services/api/.bbox/knowledge/knowledge-a.json".to_string();
        input.gaps[0].repository_relative_filename =
            "services/api/.bbox/gaps/gap-1234abcd.json".to_string();
        let prepared =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        assert!(
            prepared.generation.knowledge_file_manifest.contains_key(
                &NormalizedRepoRelativeFilename::parse(
                    "services/api/.bbox/knowledge/knowledge-a.json"
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn filename_id_mismatch_and_duplicates_fail_closed() {
        let mut mismatch = build_input();
        mismatch.knowledge[0].repository_relative_filename =
            ".bbox/knowledge/other.json".to_string();
        assert!(
            prepare_accepted_publication_v1(mismatch, &AcceptedPublicationLimits::default())
                .is_err()
        );

        let mut duplicate = build_input();
        duplicate.knowledge.push(duplicate.knowledge[0].clone());
        assert!(
            prepare_accepted_publication_v1(duplicate, &AcceptedPublicationLimits::default())
                .is_err()
        );
    }

    #[test]
    fn empty_lanes_are_valid_and_invalid_source_json_is_not() {
        let mut empty = build_input();
        empty.knowledge.clear();
        empty.gaps.clear();
        let prepared =
            prepare_accepted_publication_v1(empty, &AcceptedPublicationLimits::default()).unwrap();
        assert_eq!(prepared.generation.total_encoded_bytes, 0);
        assert!(prepared.generation.normalized_knowledge.is_empty());
        assert!(prepared.generation.normalized_gaps.is_empty());

        let mut invalid = build_input();
        invalid.knowledge[0].source_bytes = b"not-json".to_vec();
        assert!(
            prepare_accepted_publication_v1(invalid, &AcceptedPublicationLimits::default())
                .is_err()
        );
    }

    #[test]
    fn hard_and_configured_caps_fail_before_encoding() {
        let mut input = build_input();
        input.knowledge[0].source_bytes = vec![b' '; 10];
        let limits = AcceptedPublicationLimits {
            max_source_file_bytes: 9,
            ..AcceptedPublicationLimits::default()
        };
        assert!(prepare_accepted_publication_v1(input, &limits).is_err());

        let invalid_limits = AcceptedPublicationLimits {
            max_pointer_bytes: MAX_ACCEPTED_PUBLICATION_POINTER_BYTES + 1,
            ..AcceptedPublicationLimits::default()
        };
        assert!(prepare_accepted_publication_v1(build_input(), &invalid_limits).is_err());

        let oversized = vec![b' '; MAX_ACCEPTED_PUBLICATION_POINTER_BYTES + 1];
        assert!(decode_pointer_v1(&oversized, &AcceptedPublicationLimits::default()).is_err());
    }

    #[test]
    fn generation_tampering_is_rejected() {
        let prepared = prepared();
        let limits = AcceptedPublicationLimits::default();

        let mut wrong_total = prepared.generation.clone();
        wrong_total.total_encoded_bytes += 1;
        let bytes = serde_json::to_vec(&wrong_total).unwrap();
        assert!(decode_generation_v1(&bytes, &limits).is_err());

        let mut wrong_count = prepared.generation.clone();
        wrong_count.counts.knowledge_entries += 1;
        let bytes = serde_json::to_vec(&wrong_count).unwrap();
        assert!(decode_generation_v1(&bytes, &limits).is_err());

        let mut wrong_hash = prepared.generation.clone();
        wrong_hash.hashes.normalized_gaps_sha256 =
            PublicationSha256::parse("f".repeat(64)).unwrap();
        let bytes = serde_json::to_vec(&wrong_hash).unwrap();
        assert!(decode_generation_v1(&bytes, &limits).is_err());

        let mut wrong_record_hash = prepared.generation.clone();
        wrong_record_hash
            .gap_file_manifest
            .values_mut()
            .next()
            .unwrap()
            .normalized_record_sha256 = PublicationSha256::parse("e".repeat(64)).unwrap();
        let bytes = serde_json::to_vec(&wrong_record_hash).unwrap();
        assert!(decode_generation_v1(&bytes, &limits).is_err());
    }

    #[test]
    fn pointer_binding_tampering_is_rejected() {
        let prepared = prepared();
        let mut pointer = prepared.pointer.clone();
        pointer.accepted_commit = GitObjectId::parse("b".repeat(40)).unwrap();
        assert!(
            verify_pointer_generation_v1(
                &pointer,
                &prepared.generation_bytes,
                &AcceptedPublicationLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_fields_and_duplicate_map_keys_are_rejected() {
        let prepared = prepared();
        let mut pointer = serde_json::to_value(&prepared.pointer).unwrap();
        pointer
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(
            decode_pointer_v1(
                &serde_json::to_vec(&pointer).unwrap(),
                &AcceptedPublicationLimits::default()
            )
            .is_err()
        );

        let mut generation = serde_json::to_value(&prepared.generation).unwrap();
        generation["hashes"]
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(
            decode_generation_v1(
                &serde_json::to_vec(&generation).unwrap(),
                &AcceptedPublicationLimits::default()
            )
            .is_err()
        );

        #[derive(Deserialize)]
        struct StrictMap {
            #[serde(deserialize_with = "deserialize_unique_btree_map")]
            values: BTreeMap<String, u64>,
        }
        let duplicate = serde_json::from_str::<StrictMap>(r#"{"values":{"same":1,"same":2}}"#);
        assert!(duplicate.is_err());
    }

    #[test]
    fn serialized_generation_contains_no_checkout_path_or_ancestry() {
        let prepared = prepared();
        let json = String::from_utf8(prepared.generation_bytes).unwrap();
        assert!(!json.contains("/temporary/checkout"));
        assert!(!json.contains("/temporary/carrier"));
        assert!(!json.contains("merge_base"));
        assert!(!json.contains("parent_shas"));
        assert!(!json.contains("recall_count"));
    }

    fn install_prepared(
        paths: &AcceptedPublicationStorePaths,
        prepared: &PreparedAcceptedPublicationV1,
    ) {
        fs::create_dir_all(paths.pointers()).unwrap();
        fs::create_dir_all(paths.generations().join(project_id().as_str())).unwrap();
        fs::write(
            paths.pointer(&project_id()),
            prepared.pointer_bytes.as_slice(),
        )
        .unwrap();
        fs::write(
            paths.generation(&project_id(), &prepared.generation_id),
            prepared.generation_bytes.as_slice(),
        )
        .unwrap();
    }

    #[test]
    fn installed_verification_requires_exact_pointer_and_generation_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = AcceptedPublicationStorePaths::derive(&root.join("projects.json")).unwrap();
        let prepared = prepared();
        install_prepared(&paths, &prepared);
        let guard = acquire_accepted_publication_lock(&paths).unwrap();
        let verified = verify_installed_locked(
            &paths,
            &guard,
            &project_id(),
            &prepared.pointer_hash,
            &AcceptedPublicationLimits::default(),
        )
        .unwrap();
        assert_eq!(
            verified.selection,
            VerifiedAcceptedPublicationSelectionV1::Current
        );
        assert_eq!(verified.generation_id, prepared.generation_id);

        let wrong_hash = PublicationSha256::parse("f".repeat(64)).unwrap();
        assert!(
            verify_installed_locked(
                &paths,
                &guard,
                &project_id(),
                &wrong_hash,
                &AcceptedPublicationLimits::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn corrupt_current_generation_falls_back_only_to_verified_prior() {
        let limits = AcceptedPublicationLimits::default();
        let first = prepared();
        let prior = AcceptedPublicationPriorPointerV1 {
            attachment_id: first.pointer.attachment_id.clone(),
            full_ref: first.pointer.full_ref.clone(),
            accepted_commit: first.pointer.accepted_commit.clone(),
            accepted_scope: first.pointer.accepted_scope.clone(),
            accepted_generation: first.pointer.accepted_generation.clone(),
            generation_hash: first.pointer.generation_hash.clone(),
        };
        let mut second_input = build_input();
        second_input.accepted_commit = GitObjectId::parse("b".repeat(40)).unwrap();
        let mut second_knowledge = knowledge("knowledge-a");
        second_knowledge.content = "new accepted content".to_string();
        second_input.knowledge[0].source_bytes = serde_json::to_vec(&second_knowledge).unwrap();
        second_input.prior_pointer = Some(prior);
        let second = prepare_accepted_publication_v1(second_input, &limits).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = AcceptedPublicationStorePaths::derive(&root.join("projects.json")).unwrap();
        install_prepared(&paths, &first);
        install_prepared(&paths, &second);
        fs::write(
            paths.generation(&project_id(), &second.generation_id),
            b"corrupt",
        )
        .unwrap();

        let guard = acquire_accepted_publication_lock(&paths).unwrap();
        let verified = verify_selected_locked(&paths, &guard, &project_id(), &limits).unwrap();
        assert_eq!(
            verified.selection,
            VerifiedAcceptedPublicationSelectionV1::Prior
        );
        assert_eq!(verified.generation_id, first.generation_id);

        fs::write(
            paths.generation(&project_id(), &first.generation_id),
            b"also corrupt",
        )
        .unwrap();
        assert!(verify_selected_locked(&paths, &guard, &project_id(), &limits).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verification_rejects_symlinked_pointer_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = AcceptedPublicationStorePaths::derive(&root.join("projects.json")).unwrap();
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(paths.root()).unwrap();
        symlink(&elsewhere, paths.pointers()).unwrap();
        let guard = acquire_accepted_publication_lock(&paths).unwrap();
        assert!(
            verify_selected_locked(
                &paths,
                &guard,
                &project_id(),
                &AcceptedPublicationLimits::default(),
            )
            .is_err()
        );
    }
}
