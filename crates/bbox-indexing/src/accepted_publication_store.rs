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
use std::time::Duration;

use bbox_chunker::EdgeConfidence;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow_with_timeout,
    canonical_store_lock_path,
};
use bbox_corpus_core::project_catalog::{AttachmentId, ProjectId};
use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapNote, GapResolution};
use bbox_knowledge::knowledge::{
    Approval, Category, KnowledgeEdgeKind, KnowledgeEntry, Priority, Scope, Status,
};
use bbox_knowledge_source::validate_publication_generation_id;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

const ACCEPTED_PUBLICATION_VERSION: u32 = 1;
const ACCEPTED_PUBLICATION_POINTER_V2: u32 = 2;
const MAX_PROJECTS_BASENAME_BYTES: usize = 255;
const MAX_FULL_REF_BYTES: usize = 1024;
const MAX_RECORD_ID_BYTES: usize = 256;
const MAX_REPOSITORY_RELATIVE_FILENAME_BYTES: usize = 4096;

pub(crate) const MAX_ACCEPTED_PUBLICATION_SOURCE_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_ENTRIES_PER_LANE: usize = 100_000;
pub(crate) const MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE: u64 = 128 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const MAX_ACCEPTED_PUBLICATION_POINTER_BYTES: usize = 64 * 1024;
const ACCEPTED_PUBLICATION_LOCK_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) type AcceptedPublicationStoreResult<T> = Result<T, AcceptedPublicationStoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedPublicationStoreError {
    code: &'static str,
    detail: String,
}

impl AcceptedPublicationStoreError {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
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

// The visibility parameter governs the TYPE only. `parse` stays crate-private
// on every instantiation: the runtime facade may hand a validated value out,
// but no crate-external caller may mint one (plan section 4.2).
macro_rules! validated_string_type {
    ($vis:vis $name:ident, $validator:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        $vis struct $name(String);

        impl $name {
            pub(crate) fn parse(value: impl Into<String>) -> AcceptedPublicationStoreResult<Self> {
                let value = value.into();
                $validator(&value)?;
                Ok(Self(value))
            }

            $vis fn as_str(&self) -> &str {
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

validated_string_type!(pub PublicationSha256, validate_sha256);
validated_string_type!(pub(crate) AcceptedPublicationGenerationId, validate_sha256);
validated_string_type!(pub(crate) GitObjectId, validate_git_object_id);
validated_string_type!(pub(crate) FullPublisherRef, validate_full_publisher_ref);
validated_string_type!(pub PublicationRecordId, validate_record_id);
validated_string_type!(
    pub NormalizedRepoRelativeFilename,
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
pub enum AcceptedKnowledgeCategoryV1 {
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
pub enum AcceptedKnowledgeScopeV1 {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedKnowledgePriorityV1 {
    Critical,
    Standard,
    Supplementary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedKnowledgeStatusV1 {
    Active,
    Draft,
    Superseded,
    Disabled,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedKnowledgeApprovalV1 {
    UserConfirmed,
    AgentInferred,
    Imported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedKnowledgeEdgeKindV1 {
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
pub enum AcceptedEdgeConfidenceV1 {
    Exact,
    Heuristic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedGapKindV1 {
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
pub enum AcceptedGapImpactV1 {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedBlockingLevelV1 {
    None,
    WorkaroundAvailable,
    BlocksTask,
    BlocksClassOfWork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedGapResolutionV1 {
    Unresolved,
    Acknowledged,
    Addressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedKnowledgeEdgeV1 {
    pub target: String,
    pub kind: AcceptedKnowledgeEdgeKindV1,
    pub note: Option<String>,
    pub source_arc: Option<String>,
    pub confidence: AcceptedEdgeConfidenceV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedKnowledgeEntryV1 {
    pub id: PublicationRecordId,
    pub title: String,
    pub content: String,
    pub cluster: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_btree_map")]
    pub variants: BTreeMap<String, String>,
    pub category: AcceptedKnowledgeCategoryV1,
    pub scope: AcceptedKnowledgeScopeV1,
    pub providers: Vec<String>,
    pub priority: AcceptedKnowledgePriorityV1,
    pub weight: u32,
    pub status: AcceptedKnowledgeStatusV1,
    pub approval: AcceptedKnowledgeApprovalV1,
    pub render: bool,
    pub decay: bool,
    pub review_at: Option<String>,
    pub supersedes: Option<String>,
    pub links: Vec<AcceptedKnowledgeEdgeV1>,
    pub rationale: Option<String>,
    pub expires_at: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedGapEntryV1 {
    pub id: PublicationRecordId,
    pub title: String,
    pub gap_kind: AcceptedGapKindV1,
    pub domain: String,
    pub wanted_capability: String,
    pub missing_primitive: Option<String>,
    pub fallback_used: Option<String>,
    pub evidence: Vec<String>,
    pub impact: AcceptedGapImpactV1,
    pub blocking_level: AcceptedBlockingLevelV1,
    pub dedupe_key: String,
    pub suggested_owner: Option<String>,
    pub notes: Option<String>,
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
    pub resolution: AcceptedGapResolutionV1,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub provider: Option<String>,
    pub bro: Option<String>,
    pub thread_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub resolution_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationFileManifestEntryV1 {
    pub record_id: PublicationRecordId,
    pub source_content_sha256: PublicationSha256,
    pub normalized_record_sha256: PublicationSha256,
    pub encoded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedGraphSourceV1 {
    pub source_content_sha256: PublicationSha256,
    pub encoded_bytes: u64,
    pub source_bytes: Vec<u8>,
}

/// One accepted `.bbox/evidence` source file. Same shape as the graph source,
/// kept as its own type so the two lanes cannot be crossed by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedEvidenceSourceV1 {
    pub source_content_sha256: PublicationSha256,
    pub encoded_bytes: u64,
    pub source_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedPublicationHashesV1 {
    pub knowledge_file_manifest_sha256: PublicationSha256,
    pub gap_file_manifest_sha256: PublicationSha256,
    pub normalized_knowledge_sha256: PublicationSha256,
    pub normalized_gaps_sha256: PublicationSha256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_sources_sha256: Option<PublicationSha256>,
    /// `None` for an empty evidence lane, which is what every generation
    /// written before the lane existed recomputes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_sources_sha256: Option<PublicationSha256>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedPublicationCountsV1 {
    pub knowledge_files: u64,
    pub knowledge_entries: u64,
    pub gap_files: u64,
    pub gap_entries: u64,
    #[serde(default)]
    pub graph_files: u64,
    #[serde(default)]
    pub evidence_files: u64,
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
    #[serde(default, deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) graph_sources: BTreeMap<NormalizedRepoRelativeFilename, AcceptedGraphSourceV1>,
    /// Absent in every generation written before the evidence lane; `default`
    /// decodes those to the empty map so the record recomputes to itself.
    #[serde(default, deserialize_with = "deserialize_unique_btree_map")]
    pub(crate) evidence_sources: BTreeMap<NormalizedRepoRelativeFilename, AcceptedEvidenceSourceV1>,
    pub(crate) hashes: AcceptedPublicationHashesV1,
    pub(crate) counts: AcceptedPublicationCountsV1,
    pub(crate) total_encoded_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AcceptedPublicationSourceBindingV2 {
    Attachment {
        attachment_id: AttachmentId,
    },
    Producer {
        producer_id: String,
        source_generation_id: String,
        source_generation_sha256: PublicationSha256,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationPriorPointerV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<AttachmentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_binding: Option<AcceptedPublicationSourceBindingV2>,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) accepted_scope: PublishedScope,
    pub(crate) accepted_generation: AcceptedPublicationGenerationId,
    pub(crate) generation_hash: PublicationSha256,
}

/// The operator's standing grant for policy-driven acceptance, carried by
/// the pointer that the operator installed (`publisher-auto-advance.md`).
///
/// It lives on the mutable pointer rather than in accepted CONTENT because
/// the grant is an operator fact about a project, not a producer-attested
/// fact about a commit. A producer supplies candidate bytes; it never
/// supplies this. The field is additive and optional, so every pointer
/// written before the feature decodes and re-encodes byte-identically and
/// keeps its compare-and-swap digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationAutoAdvanceV1 {
    pub(crate) enabled: bool,
    /// The `audit_reason` of the operator advance that installed this
    /// grant, retained so `bbox_audit` history can name the human act that
    /// authorized every later policy acceptance.
    pub(crate) granted_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptedPublicationPointerV1 {
    pub(crate) version: u32,
    pub(crate) project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<AttachmentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_binding: Option<AcceptedPublicationSourceBindingV2>,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) accepted_scope: PublishedScope,
    pub(crate) accepted_generation: AcceptedPublicationGenerationId,
    pub(crate) generation_hash: PublicationSha256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_advance: Option<AcceptedPublicationAutoAdvanceV1>,
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
    pub(crate) source_binding: AcceptedPublicationBuildSourceV1,
    pub(crate) scope: PublishedScope,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) accepted_commit: GitObjectId,
    pub(crate) knowledge: Vec<AcceptedKnowledgeSourceV1>,
    pub(crate) gaps: Vec<AcceptedGapSourceV1>,
    pub(crate) graphs: Vec<AcceptedGraphSourceV1Input>,
    pub(crate) evidence: Vec<AcceptedEvidenceSourceV1Input>,
    /// The auto-advance grant this pointer will carry. The runtime resolves
    /// it from the installed pointer plus the caller's explicit operator
    /// update before it gets here; the builder only writes what it is told.
    pub(crate) auto_advance: Option<AcceptedPublicationAutoAdvanceV1>,
    pub(crate) prior_pointer: Option<AcceptedPublicationPriorPointerV1>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedGraphSourceV1Input {
    pub(crate) repository_relative_filename: String,
    pub(crate) source_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct AcceptedEvidenceSourceV1Input {
    pub(crate) repository_relative_filename: String,
    pub(crate) source_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcceptedPublicationBuildSourceV1 {
    Attachment(AttachmentId),
    Producer {
        producer_id: String,
        source_generation_id: String,
        source_generation_sha256: PublicationSha256,
    },
}

fn validate_source_producer_id(value: &str) -> AcceptedPublicationStoreResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_pointer(
            "accepted publication producer id is invalid",
        ));
    }
    Ok(())
}

fn validate_source_binding_v2(
    binding: &AcceptedPublicationSourceBindingV2,
) -> AcceptedPublicationStoreResult<()> {
    match binding {
        AcceptedPublicationSourceBindingV2::Attachment { .. } => Ok(()),
        AcceptedPublicationSourceBindingV2::Producer {
            producer_id,
            source_generation_id,
            ..
        } => {
            validate_source_producer_id(producer_id)?;
            validate_publication_generation_id(source_generation_id)
                .map_err(|error| invalid_pointer(error.to_string()))
        }
    }
}

fn prior_source_binding(
    pointer: &AcceptedPublicationPriorPointerV1,
) -> AcceptedPublicationStoreResult<AcceptedPublicationSourceBindingV2> {
    match (&pointer.attachment_id, &pointer.source_binding) {
        (Some(attachment_id), None) => Ok(AcceptedPublicationSourceBindingV2::Attachment {
            attachment_id: attachment_id.clone(),
        }),
        (None, Some(binding)) => {
            validate_source_binding_v2(binding)?;
            Ok(binding.clone())
        }
        _ => Err(invalid_pointer(
            "accepted publication prior arm must carry exactly one source binding",
        )),
    }
}

pub(crate) fn pointer_source_binding(
    pointer: &AcceptedPublicationPointerV1,
) -> AcceptedPublicationStoreResult<AcceptedPublicationSourceBindingV2> {
    match pointer.version {
        ACCEPTED_PUBLICATION_VERSION => match (&pointer.attachment_id, &pointer.source_binding) {
            (Some(attachment_id), None) => Ok(AcceptedPublicationSourceBindingV2::Attachment {
                attachment_id: attachment_id.clone(),
            }),
            _ => Err(invalid_pointer(
                "version-1 accepted publication pointer must carry one attachment binding",
            )),
        },
        ACCEPTED_PUBLICATION_POINTER_V2 => {
            match (&pointer.attachment_id, &pointer.source_binding) {
                (None, Some(binding)) => {
                    validate_source_binding_v2(binding)?;
                    Ok(binding.clone())
                }
                _ => Err(invalid_pointer(
                    "version-2 accepted publication pointer must carry one typed source binding",
                )),
            }
        }
        _ => Err(invalid_pointer(
            "accepted publication pointer has an unsupported version",
        )),
    }
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
    pub(crate) max_graph_source_bytes: u64,
    pub(crate) max_evidence_source_bytes: u64,
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
            max_graph_source_bytes: MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE,
            max_evidence_source_bytes: MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE,
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
            && self.max_graph_source_bytes > 0
            && self.max_graph_source_bytes <= MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE
            && self.max_evidence_source_bytes > 0
            && self.max_evidence_source_bytes <= MAX_ACCEPTED_PUBLICATION_SOURCE_BYTES_PER_LANE
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
    let guard =
        acquire_store_lock_nofollow_with_timeout(paths.anchor(), ACCEPTED_PUBLICATION_LOCK_TIMEOUT)
            .map_err(|error| {
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

    let graph_limits = bbox_knowledge_source::KnowledgeSourceLimits::default();
    let graph_prefix = if input.scope.bbox_root_relpath() == "." {
        ".bbox/graphs/".to_string()
    } else {
        format!("{}/.bbox/graphs/", input.scope.bbox_root_relpath())
    };
    let mut graph_sources = BTreeMap::new();
    let mut graph_documents = BTreeMap::<String, BTreeMap<String, Vec<u8>>>::new();
    let mut graph_source_bytes = 0_u64;
    for source in input.graphs {
        let encoded_bytes = u64::try_from(source.source_bytes.len())
            .map_err(|_| byte_limit("accepted graph source file"))?;
        let graph_jsonl = source
            .repository_relative_filename
            .ends_with("/vertices.jsonl")
            || source
                .repository_relative_filename
                .ends_with("/edges.jsonl");
        if (!graph_jsonl && encoded_bytes == 0) || encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted graph source file"));
        }
        graph_source_bytes = graph_source_bytes
            .checked_add(encoded_bytes)
            .ok_or_else(|| byte_limit("accepted graph source lane"))?;
        if graph_source_bytes > limits.max_graph_source_bytes {
            return Err(byte_limit("accepted graph source lane"));
        }
        let supplied =
            NormalizedRepoRelativeFilename::parse(source.repository_relative_filename.clone())?;
        let relative = source
            .repository_relative_filename
            .strip_prefix(&graph_prefix)
            .ok_or_else(|| invalid_generation("accepted graph filename is outside graph scope"))?;
        let (graph_id, filename) = relative
            .split_once('/')
            .ok_or_else(|| invalid_generation("accepted graph filename has invalid depth"))?;
        if relative.matches('/').count() != 1 {
            return Err(invalid_generation(
                "accepted graph filename has invalid depth",
            ));
        }
        let files = graph_documents.entry(graph_id.to_string()).or_default();
        if files
            .insert(filename.to_string(), source.source_bytes.clone())
            .is_some()
            || graph_sources
                .insert(
                    supplied,
                    AcceptedGraphSourceV1 {
                        source_content_sha256: PublicationSha256::digest(&source.source_bytes),
                        encoded_bytes,
                        source_bytes: source.source_bytes,
                    },
                )
                .is_some()
        {
            return Err(invalid_generation(
                "accepted graph publication contains a duplicate source file",
            ));
        }
    }
    for (graph_id, files) in graph_documents {
        let schema = files
            .get("schema.json")
            .ok_or_else(|| invalid_generation("accepted graph is missing schema.json"))?;
        let vertices = files
            .get("vertices.jsonl")
            .ok_or_else(|| invalid_generation("accepted graph is missing vertices.jsonl"))?;
        let edges = files
            .get("edges.jsonl")
            .ok_or_else(|| invalid_generation("accepted graph is missing edges.jsonl"))?;
        let descriptor = files.get("graph.json").map(Vec::as_slice);
        if files.len() > 4
            || files.keys().any(|filename| {
                !matches!(
                    filename.as_str(),
                    "graph.json" | "schema.json" | "vertices.jsonl" | "edges.jsonl"
                )
            })
        {
            return Err(invalid_generation(
                "accepted graph contains an unknown source file",
            ));
        }
        let loaded = bbox_project_graph::load_graph_documents(
            input.project_id.as_str(),
            &graph_id,
            bbox_project_graph::GraphDocumentBytes {
                descriptor,
                schema,
                vertices,
                edges,
            },
            bbox_project_graph::GraphParseLimits {
                max_vertices: graph_limits.max_graph_rows_per_file as usize,
                max_edges: graph_limits.max_graph_rows_per_file as usize,
            },
            std::path::PathBuf::new(),
        );
        if !loaded.report.valid {
            let diagnostic = loaded
                .report
                .errors
                .iter()
                .map(|error| {
                    format!(
                        "{}:{}:{}",
                        error.file,
                        error.line.unwrap_or_default(),
                        error.message
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(invalid_generation(format!(
                "accepted graph `{graph_id}` failed validation: {diagnostic}"
            )));
        }
    }

    let evidence_prefix = if input.scope.bbox_root_relpath() == "." {
        format!("{}/", bbox_project_graph::EVIDENCE_LANE_ROOT)
    } else {
        format!(
            "{}/{}/",
            input.scope.bbox_root_relpath(),
            bbox_project_graph::EVIDENCE_LANE_ROOT
        )
    };
    let mut evidence_sources = BTreeMap::new();
    let mut evidence_source_bytes = 0_u64;
    for source in input.evidence {
        let encoded_bytes = u64::try_from(source.source_bytes.len())
            .map_err(|_| byte_limit("accepted evidence source file"))?;
        if encoded_bytes == 0 || encoded_bytes > limits.max_source_file_bytes {
            return Err(byte_limit("accepted evidence source file"));
        }
        evidence_source_bytes = evidence_source_bytes
            .checked_add(encoded_bytes)
            .ok_or_else(|| byte_limit("accepted evidence source lane"))?;
        if evidence_source_bytes > limits.max_evidence_source_bytes {
            return Err(byte_limit("accepted evidence source lane"));
        }
        let supplied =
            NormalizedRepoRelativeFilename::parse(source.repository_relative_filename.clone())?;
        let relative = source
            .repository_relative_filename
            .strip_prefix(&evidence_prefix)
            .ok_or_else(|| {
                invalid_generation("accepted evidence filename is outside evidence scope")
            })?;
        if relative != bbox_project_graph::EVIDENCE_BINDINGS_FILENAME {
            return Err(invalid_generation(
                "accepted evidence lane admits only the bindings document",
            ));
        }
        // The accepted set is validated BEFORE install, so an invalid document
        // never displaces the prior accepted bindings: the whole publication
        // is refused rather than landing a half-readable lane.
        let load = bbox_project_graph::parse_evidence_document(
            input.project_id.as_str(),
            &source.source_bytes,
            bbox_project_graph::EvidenceParseLimits::default(),
        );
        if !load.valid() {
            let diagnostic = load
                .errors
                .iter()
                .map(|error| {
                    format!(
                        "{}:{}:{}",
                        error.code,
                        error.binding_id.clone().unwrap_or_default(),
                        error.message
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(invalid_generation(format!(
                "accepted evidence bindings failed validation: {diagnostic}"
            )));
        }
        if evidence_sources
            .insert(
                supplied,
                AcceptedEvidenceSourceV1 {
                    source_content_sha256: PublicationSha256::digest(&source.source_bytes),
                    encoded_bytes,
                    source_bytes: source.source_bytes,
                },
            )
            .is_some()
        {
            return Err(invalid_generation(
                "accepted evidence publication contains a duplicate source file",
            ));
        }
    }

    let total_encoded_bytes = knowledge_source_bytes
        .checked_add(gap_source_bytes)
        .and_then(|bytes| bytes.checked_add(graph_source_bytes))
        .and_then(|bytes| bytes.checked_add(evidence_source_bytes))
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
        graph_sources_sha256: if graph_sources.is_empty() {
            None
        } else {
            Some(canonical_json_hash(
                &graph_sources,
                "accepted graph sources",
            )?)
        },
        evidence_sources_sha256: if evidence_sources.is_empty() {
            None
        } else {
            Some(canonical_json_hash(
                &evidence_sources,
                "accepted evidence sources",
            )?)
        },
    };
    let counts = AcceptedPublicationCountsV1 {
        knowledge_files: usize_to_u64(knowledge_file_manifest.len(), "accepted knowledge file")?,
        knowledge_entries: usize_to_u64(normalized_knowledge.len(), "accepted knowledge entry")?,
        gap_files: usize_to_u64(gap_file_manifest.len(), "accepted gap file")?,
        gap_entries: usize_to_u64(normalized_gaps.len(), "accepted gap entry")?,
        graph_files: usize_to_u64(graph_sources.len(), "accepted graph file")?,
        evidence_files: usize_to_u64(evidence_sources.len(), "accepted evidence file")?,
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
        graph_sources,
        evidence_sources,
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
    let write_v2 = matches!(
        &input.source_binding,
        AcceptedPublicationBuildSourceV1::Producer { .. }
    ) || input
        .prior_pointer
        .as_ref()
        .is_some_and(|prior| prior.source_binding.is_some());
    let (attachment_id, source_binding) = match input.source_binding {
        AcceptedPublicationBuildSourceV1::Attachment(attachment_id) if !write_v2 => {
            (Some(attachment_id), None)
        }
        AcceptedPublicationBuildSourceV1::Attachment(attachment_id) => (
            None,
            Some(AcceptedPublicationSourceBindingV2::Attachment { attachment_id }),
        ),
        AcceptedPublicationBuildSourceV1::Producer {
            producer_id,
            source_generation_id,
            source_generation_sha256,
        } => (
            None,
            Some(AcceptedPublicationSourceBindingV2::Producer {
                producer_id,
                source_generation_id,
                source_generation_sha256,
            }),
        ),
    };
    let pointer = AcceptedPublicationPointerV1 {
        version: if write_v2 {
            ACCEPTED_PUBLICATION_POINTER_V2
        } else {
            ACCEPTED_PUBLICATION_VERSION
        },
        project_id: input.project_id,
        attachment_id,
        source_binding,
        full_ref: input.full_ref,
        accepted_commit: input.accepted_commit,
        accepted_scope: input.scope,
        accepted_generation: generation_id.clone(),
        generation_hash: generation_hash.clone(),
        auto_advance: input.auto_advance,
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

    let graph_source_bytes = validate_accepted_graph_sources(
        &generation.project_id,
        &generation.scope,
        &generation.graph_sources,
        limits,
    )?;
    let evidence_source_bytes = validate_accepted_evidence_sources(
        &generation.project_id,
        &generation.scope,
        &generation.evidence_sources,
        limits,
    )?;
    let expected_total = knowledge_source_bytes
        .checked_add(gap_source_bytes)
        .and_then(|bytes| bytes.checked_add(graph_source_bytes))
        .and_then(|bytes| bytes.checked_add(evidence_source_bytes))
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
        graph_files: usize_to_u64(generation.graph_sources.len(), "accepted graph file")?,
        evidence_files: usize_to_u64(generation.evidence_sources.len(), "accepted evidence file")?,
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
        graph_sources_sha256: if generation.graph_sources.is_empty() {
            None
        } else {
            Some(canonical_json_hash(
                &generation.graph_sources,
                "accepted graph sources",
            )?)
        },
        evidence_sources_sha256: if generation.evidence_sources.is_empty() {
            None
        } else {
            Some(canonical_json_hash(
                &generation.evidence_sources,
                "accepted evidence sources",
            )?)
        },
    };
    if generation.hashes != expected_hashes {
        return Err(invalid_generation(
            "accepted publication aggregate hashes disagree",
        ));
    }
    Ok(())
}

/// Re-validate the accepted evidence lane on read.
///
/// The lane is one document at one exact path, and its bytes must still parse
/// and validate as a complete binding document. A generation written before
/// the lane existed carries an empty map, which validates trivially and
/// contributes zero bytes, so its stored total and hashes recompute to
/// themselves.
fn validate_accepted_evidence_sources(
    project_id: &ProjectId,
    scope: &PublishedScope,
    sources: &BTreeMap<NormalizedRepoRelativeFilename, AcceptedEvidenceSourceV1>,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<u64> {
    let evidence_prefix = if scope.bbox_root_relpath() == "." {
        format!("{}/", bbox_project_graph::EVIDENCE_LANE_ROOT)
    } else {
        format!(
            "{}/{}/",
            scope.bbox_root_relpath(),
            bbox_project_graph::EVIDENCE_LANE_ROOT
        )
    };
    if sources.len() > 1 {
        return Err(invalid_generation(
            "accepted evidence lane admits only the bindings document",
        ));
    }
    let mut total = 0_u64;
    for (filename, source) in sources {
        if source.encoded_bytes == 0
            || source.encoded_bytes > limits.max_source_file_bytes
            || source.encoded_bytes != source.source_bytes.len() as u64
            || source.source_content_sha256 != PublicationSha256::digest(&source.source_bytes)
        {
            return Err(invalid_generation(
                "accepted evidence source bytes disagree with their manifest",
            ));
        }
        total = total
            .checked_add(source.encoded_bytes)
            .ok_or_else(|| byte_limit("accepted evidence source lane"))?;
        if total > limits.max_evidence_source_bytes {
            return Err(byte_limit("accepted evidence source lane"));
        }
        let relative = filename
            .as_str()
            .strip_prefix(&evidence_prefix)
            .ok_or_else(|| {
                invalid_generation("accepted evidence filename is outside evidence scope")
            })?;
        if relative != bbox_project_graph::EVIDENCE_BINDINGS_FILENAME {
            return Err(invalid_generation(
                "accepted evidence lane admits only the bindings document",
            ));
        }
        let load = bbox_project_graph::parse_evidence_document(
            project_id.as_str(),
            &source.source_bytes,
            bbox_project_graph::EvidenceParseLimits::default(),
        );
        if !load.valid() {
            return Err(invalid_generation(
                "accepted evidence bindings failed validation",
            ));
        }
    }
    Ok(total)
}

fn validate_accepted_graph_sources(
    project_id: &ProjectId,
    scope: &PublishedScope,
    sources: &BTreeMap<NormalizedRepoRelativeFilename, AcceptedGraphSourceV1>,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<u64> {
    let graph_prefix = if scope.bbox_root_relpath() == "." {
        ".bbox/graphs/".to_string()
    } else {
        format!("{}/.bbox/graphs/", scope.bbox_root_relpath())
    };
    let graph_limits = bbox_knowledge_source::KnowledgeSourceLimits::default();
    let mut total = 0_u64;
    let mut graph_documents = BTreeMap::<String, BTreeMap<String, &[u8]>>::new();
    for (filename, source) in sources {
        let graph_jsonl = filename.as_str().ends_with("/vertices.jsonl")
            || filename.as_str().ends_with("/edges.jsonl");
        if (!graph_jsonl && source.encoded_bytes == 0)
            || source.encoded_bytes > limits.max_source_file_bytes
            || source.encoded_bytes != source.source_bytes.len() as u64
            || source.source_content_sha256 != PublicationSha256::digest(&source.source_bytes)
        {
            return Err(invalid_generation(
                "accepted graph source bytes disagree with their manifest",
            ));
        }
        total = total
            .checked_add(source.encoded_bytes)
            .ok_or_else(|| byte_limit("accepted graph source lane"))?;
        if total > limits.max_graph_source_bytes {
            return Err(byte_limit("accepted graph source lane"));
        }
        let relative = filename
            .as_str()
            .strip_prefix(&graph_prefix)
            .ok_or_else(|| invalid_generation("accepted graph filename is outside graph scope"))?;
        let (graph_id, graph_file) = relative
            .split_once('/')
            .ok_or_else(|| invalid_generation("accepted graph filename has invalid depth"))?;
        if relative.matches('/').count() != 1
            || graph_documents
                .entry(graph_id.to_string())
                .or_default()
                .insert(graph_file.to_string(), &source.source_bytes)
                .is_some()
        {
            return Err(invalid_generation(
                "accepted graph source path is duplicate or invalid",
            ));
        }
    }
    for (graph_id, files) in graph_documents {
        let schema = files
            .get("schema.json")
            .ok_or_else(|| invalid_generation("accepted graph is missing schema.json"))?;
        let vertices = files
            .get("vertices.jsonl")
            .ok_or_else(|| invalid_generation("accepted graph is missing vertices.jsonl"))?;
        let edges = files
            .get("edges.jsonl")
            .ok_or_else(|| invalid_generation("accepted graph is missing edges.jsonl"))?;
        let descriptor = files.get("graph.json").copied();
        if files.len() > 4
            || files.keys().any(|filename| {
                !matches!(
                    filename.as_str(),
                    "graph.json" | "schema.json" | "vertices.jsonl" | "edges.jsonl"
                )
            })
        {
            return Err(invalid_generation(
                "accepted graph contains an unknown source file",
            ));
        }
        let loaded = bbox_project_graph::load_graph_documents(
            project_id.as_str(),
            &graph_id,
            bbox_project_graph::GraphDocumentBytes {
                descriptor,
                schema,
                vertices,
                edges,
            },
            bbox_project_graph::GraphParseLimits {
                max_vertices: graph_limits.max_graph_rows_per_file as usize,
                max_edges: graph_limits.max_graph_rows_per_file as usize,
            },
            std::path::PathBuf::new(),
        );
        if !loaded.report.valid {
            return Err(invalid_generation(format!(
                "accepted graph `{graph_id}` failed validation: {}",
                loaded
                    .report
                    .errors
                    .iter()
                    .map(|error| error.code.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(total)
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
    prior_source_binding(pointer)?;
    pointer
        .accepted_scope
        .validate()
        .map_err(|error| invalid_pointer(error.to_string()))
}

/// Longest `granted_reason` a pointer may carry. It mirrors the catalog's
/// own bounded audit reason, because the value IS one: the audit reason of
/// the operator advance that installed the grant.
pub(crate) const MAX_AUTO_ADVANCE_REASON_BYTES: usize = 1024;

fn validate_auto_advance_v1(
    policy: &AcceptedPublicationAutoAdvanceV1,
) -> AcceptedPublicationStoreResult<()> {
    if policy.granted_reason.trim().is_empty() {
        return Err(invalid_pointer(
            "an auto-advance grant must record the operator audit reason that installed it",
        ));
    }
    if policy.granted_reason.len() > MAX_AUTO_ADVANCE_REASON_BYTES {
        return Err(invalid_pointer(
            "auto-advance granted_reason exceeds the bounded audit-reason length",
        ));
    }
    if policy.granted_reason.chars().any(char::is_control) {
        return Err(invalid_pointer(
            "auto-advance granted_reason must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_pointer_v1(
    pointer: &AcceptedPublicationPointerV1,
) -> AcceptedPublicationStoreResult<()> {
    pointer_source_binding(pointer)?;
    if let Some(policy) = &pointer.auto_advance {
        validate_auto_advance_v1(policy)?;
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

pub(crate) fn selected_pointer_source_binding(
    pointer: &AcceptedPublicationPointerV1,
    selection: VerifiedAcceptedPublicationSelectionV1,
) -> AcceptedPublicationStoreResult<AcceptedPublicationSourceBindingV2> {
    match selection {
        VerifiedAcceptedPublicationSelectionV1::Current => pointer_source_binding(pointer),
        VerifiedAcceptedPublicationSelectionV1::Prior => pointer
            .prior_pointer
            .as_ref()
            .ok_or_else(|| invalid_pointer("a prior selection requires a prior pointer"))
            .and_then(prior_source_binding),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedAcceptedPublicationV1 {
    pub(crate) selection: VerifiedAcceptedPublicationSelectionV1,
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
        generation,
        generation_bytes,
    })
}

/// One strict selected read plus the binding evidence the runtime facade
/// reports and the later advance path compares against.
///
/// `pointer_sha256` is digested from the exact bytes this read verified,
/// never re-encoded from the decoded pointer: it is a compare-and-swap
/// token, so it must name what is installed rather than what a re-encode
/// would produce.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedAcceptedPublicationBindingV1 {
    pub(crate) pointer: AcceptedPublicationPointerV1,
    pub(crate) pointer_sha256: PublicationSha256,
    pub(crate) verified: VerifiedAcceptedPublicationV1,
}

/// `Ok(None)` means this project has no accepted pointer at all, which is
/// the migration outcome for a project that acknowledged no published
/// content. Every other failure is damage and stays an error.
pub(crate) fn verify_selected_with_binding_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Option<VerifiedAcceptedPublicationBindingV1>> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let Some(pointer_bytes) =
        read_pointer_optional_locked(paths, project_id, limits.max_pointer_bytes)?
    else {
        return Ok(None);
    };
    let pointer_sha256 = PublicationSha256::digest(&pointer_bytes);
    let pointer = decode_pointer_v1(&pointer_bytes, limits)?;
    let verified = verify_selected_from_pointer_locked(paths, project_id, pointer_bytes, limits)?;
    Ok(Some(VerifiedAcceptedPublicationBindingV1 {
        pointer,
        pointer_sha256,
        verified,
    }))
}

/// The durably referenced generation ids for one project: current first,
/// then prior when the pointer carries one. `Ok(None)` means no pointer
/// exists. A pointer that cannot be decoded is an error, never an empty
/// root set, because a collector must not treat unreadable authority as
/// proof that nothing is referenced (plan section 7.8).
pub(crate) fn pointer_generation_roots_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Option<Vec<AcceptedPublicationGenerationId>>> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let Some(pointer_bytes) =
        read_pointer_optional_locked(paths, project_id, limits.max_pointer_bytes)?
    else {
        return Ok(None);
    };
    let pointer = decode_pointer_v1(&pointer_bytes, limits)?;
    let mut roots = vec![pointer.accepted_generation];
    if let Some(prior) = pointer.prior_pointer {
        roots.push(prior.accepted_generation);
    }
    Ok(Some(roots))
}

pub(crate) fn pointer_source_generation_roots_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Option<Vec<String>>> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let Some(pointer_bytes) =
        read_pointer_optional_locked(paths, project_id, limits.max_pointer_bytes)?
    else {
        return Ok(None);
    };
    let pointer = decode_pointer_v1(&pointer_bytes, limits)?;
    let mut roots = Vec::new();
    if let AcceptedPublicationSourceBindingV2::Producer {
        source_generation_id,
        ..
    } = pointer_source_binding(&pointer)?
    {
        roots.push(source_generation_id);
    }
    if let Some(prior) = &pointer.prior_pointer
        && let AcceptedPublicationSourceBindingV2::Producer {
            source_generation_id,
            ..
        } = prior_source_binding(prior)?
    {
        roots.push(source_generation_id);
    }
    Ok(Some(roots))
}

/// Prove this process can act as the accepted-publication authority before
/// routes bind: the store lock is held and every store directory that
/// exists is a real directory rather than a redirect. An absent store root
/// is not a failure. It is the state of a catalog whose projects have not
/// published yet, and the per-project scan reports each of those as
/// publication-missing.
pub(crate) fn probe_global_store_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
) -> AcceptedPublicationStoreResult<()> {
    ensure_matching_guard(paths, guard)?;
    for directory in [paths.root(), paths.pointers(), paths.generations()] {
        if let Some(opened) =
            NofollowDirectory::open_existing(directory).map_err(accepted_io_error)?
        {
            opened.ensure_still_current().map_err(accepted_io_error)?;
        }
    }
    Ok(())
}

/// Stable interruption points in the publish transaction (plan §13.7).
///
/// Every point sits at a transaction boundary rather than inside one
/// durable primitive. The primitives are atomic: a generation file and a
/// pointer file each become visible whole through one rename, so "write
/// failed" and "fsync failed" are the same observable durable state as
/// "did not run". What a fault test must distinguish is which boundary the
/// process died at, and that is exactly this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AcceptedPublicationFaultPoint {
    /// Before the generation file exists durably.
    BeforeGenerationInstall,
    /// After the generation is durable, before the publication lock.
    AfterGenerationInstall,
    /// Inside the lock, before the expected-pointer tokens are checked.
    BeforePointerTokenCheck,
    /// After the tokens verified, before the caller's freshness recheck.
    BeforeFreshnessRecheck,
    /// After freshness, immediately before the atomic pointer replacement.
    BeforePointerSwap,
    /// After the pointer swap is durable, before read-back verification.
    AfterPointerSwap,
}

/// Test-only fault injection for the publish transaction. Production
/// installs none, so the checkpoint calls are one `Option` test away from
/// free.
pub(crate) trait AcceptedPublicationFaultInjector: Send + Sync + fmt::Debug {
    fn checkpoint(
        &self,
        point: AcceptedPublicationFaultPoint,
    ) -> AcceptedPublicationStoreResult<()>;
}

fn checkpoint(
    faults: Option<&dyn AcceptedPublicationFaultInjector>,
    point: AcceptedPublicationFaultPoint,
) -> AcceptedPublicationStoreResult<()> {
    match faults {
        Some(injector) => injector.checkpoint(point),
        None => Ok(()),
    }
}

fn pointer_conflict(detail: impl Into<String>) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new("error.accepted_publication_pointer_conflict", detail)
}

fn repair_required(detail: impl Into<String>) -> AcceptedPublicationStoreError {
    AcceptedPublicationStoreError::new("error.accepted_publication_repair_required", detail)
}

/// What the installed pointer must look like for this commit to proceed.
///
/// Establish carries no token at all: absence is the whole precondition, so
/// a present pointer is a conflict and never an overwrite (D-040). Advance
/// carries the pointer-specific compare-and-swap tokens, because the
/// catalog epoch does not serialize a store the catalog does not own
/// (plan §4.5).
#[derive(Debug, Clone)]
pub(crate) enum PointerExpectationV1 {
    Establish,
    Advance {
        expected_generation: AcceptedPublicationGenerationId,
        expected_pointer_sha256: PublicationSha256,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct GenerationInstallOutcomeV1 {
    /// False when the content-addressed file was already present with
    /// byte-identical content, which is a resumed preparation rather than
    /// a new install.
    pub(crate) created: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PointerCommitReceiptV1 {
    pub(crate) generation_id: AcceptedPublicationGenerationId,
    pub(crate) pointer_sha256: PublicationSha256,
    pub(crate) previous_pointer_sha256: Option<PublicationSha256>,
}

/// Install one immutable generation off-lock (plan §7.2 steps 15 to 17).
///
/// The id is the content digest, so an existing file with identical bytes
/// is the same generation and the install is idempotent. An existing file
/// with different bytes under the same id would mean the content addressing
/// was violated, so it fails closed rather than replacing anything.
pub(crate) fn install_generation_off_lock(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    prepared: &PreparedAcceptedPublicationV1,
    faults: Option<&dyn AcceptedPublicationFaultInjector>,
) -> AcceptedPublicationStoreResult<GenerationInstallOutcomeV1> {
    let project_generations = paths.generations().join(project_id.as_str());
    std::fs::create_dir_all(&project_generations).map_err(accepted_io_error)?;
    std::fs::create_dir_all(paths.pointers()).map_err(accepted_io_error)?;
    let directory = NofollowDirectory::open_existing(&project_generations)
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication generation directory is missing",
            )
        })?;
    let filename = format!("{}.json", prepared.generation_id);
    if let Some(existing) = directory
        .read_regular(
            &filename,
            prepared.generation_bytes.len().max(1),
            "accepted-publication generation",
        )
        .map_err(accepted_io_error)?
    {
        if existing != prepared.generation_bytes {
            return Err(invalid_generation(
                "a different generation is already installed under this content id",
            ));
        }
        return Ok(GenerationInstallOutcomeV1 { created: false });
    }
    checkpoint(
        faults,
        AcceptedPublicationFaultPoint::BeforeGenerationInstall,
    )?;
    // One atomic replace: temporary create, write, fsync, rename, parent
    // fsync. The generation is either absent or complete after a crash.
    directory
        .atomic_replace(&filename, &prepared.generation_bytes)
        .map_err(accepted_io_error)?;
    checkpoint(
        faults,
        AcceptedPublicationFaultPoint::AfterGenerationInstall,
    )?;
    Ok(GenerationInstallOutcomeV1 { created: true })
}

/// Compare-and-swap one pointer under the publication lock (plan §7.3).
///
/// The generation this pointer names is already durable before the lock is
/// taken, so the critical section holds no Git, no source reads, and no
/// encoding: token verification, the caller's freshness recheck, one atomic
/// replacement, and read-back.
///
/// `freshness` is the caller's recheck (catalog epoch, attachment status,
/// live ref) executed inside the lock immediately before the swap. Its
/// refusal is returned verbatim so a catalog refusal keeps its own code.
pub(crate) fn commit_pointer_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    prepared: &PreparedAcceptedPublicationV1,
    expectation: &PointerExpectationV1,
    limits: &AcceptedPublicationLimits,
    faults: Option<&dyn AcceptedPublicationFaultInjector>,
    freshness: &mut dyn FnMut() -> AcceptedPublicationStoreResult<()>,
    swap_attempted: &mut bool,
) -> AcceptedPublicationStoreResult<PointerCommitReceiptV1> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    checkpoint(
        faults,
        AcceptedPublicationFaultPoint::BeforePointerTokenCheck,
    )?;
    let installed = read_pointer_optional_locked(paths, project_id, limits.max_pointer_bytes)?;
    let previous_pointer_sha256 = installed
        .as_deref()
        .map(|bytes| PublicationSha256::digest(bytes));
    match (expectation, installed.as_deref()) {
        (PointerExpectationV1::Establish, None) => {}
        (PointerExpectationV1::Establish, Some(_)) => {
            return Err(pointer_conflict(
                "establish requires pointer absence; this project already publishes",
            ));
        }
        (PointerExpectationV1::Advance { .. }, None) => {
            return Err(pointer_conflict(
                "advance requires an installed pointer; establish creates the first one",
            ));
        }
        (
            PointerExpectationV1::Advance {
                expected_generation,
                expected_pointer_sha256,
            },
            Some(bytes),
        ) => {
            if previous_pointer_sha256.as_ref() != Some(expected_pointer_sha256) {
                return Err(pointer_conflict(
                    "the installed pointer digest is not the expected compare-and-swap token",
                ));
            }
            let current = decode_pointer_v1(bytes, limits)?;
            if &current.accepted_generation != expected_generation {
                return Err(pointer_conflict(
                    "the installed pointer names a different accepted generation",
                ));
            }
            // Advancing from a pointer whose current arm does not verify
            // would discard the evidence a repair needs (plan §4.8).
            let verified =
                verify_selected_from_pointer_locked(paths, project_id, bytes.to_vec(), limits)?;
            if verified.selection != VerifiedAcceptedPublicationSelectionV1::Current {
                return Err(repair_required(
                    "the installed pointer is serving its prior generation; repair before advancing",
                ));
            }
            // The prepared prior must be exactly the pointer being
            // replaced, or this preparation raced another advance.
            let prepared_prior = prepared.pointer.prior_pointer.as_ref().ok_or_else(|| {
                pointer_conflict("an advance must carry the replaced pointer as its prior")
            })?;
            if prepared_prior.accepted_generation != current.accepted_generation
                || prepared_prior.generation_hash != current.generation_hash
                || prepared_prior.accepted_commit != current.accepted_commit
                || prepared_prior.accepted_scope != current.accepted_scope
                || prepared_prior.full_ref != current.full_ref
                || prepared_prior.attachment_id != current.attachment_id
                || prepared_prior.source_binding != current.source_binding
            {
                return Err(pointer_conflict(
                    "the prepared prior pointer does not match the installed pointer",
                ));
            }
        }
    }
    checkpoint(
        faults,
        AcceptedPublicationFaultPoint::BeforeFreshnessRecheck,
    )?;
    freshness()?;
    checkpoint(faults, AcceptedPublicationFaultPoint::BeforePointerSwap)?;
    let directory = NofollowDirectory::open_existing(paths.pointers())
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication pointer directory is missing",
            )
        })?;
    // From here the caller can no longer assume the installed pointer is
    // unchanged: the replacement either lands or it does not, and a failure
    // after this line does not prove which. Callers treat the flag as
    // "reverify before serving cached content".
    *swap_attempted = true;
    directory
        .atomic_replace(&format!("{project_id}.json"), &prepared.pointer_bytes)
        .map_err(accepted_io_error)?;
    checkpoint(faults, AcceptedPublicationFaultPoint::AfterPointerSwap)?;
    // Read back through the same strict path a startup scan uses: the
    // installed bytes must be exactly what was prepared, and they must
    // still agree with the generation they name.
    let verified =
        verify_installed_locked(paths, guard, project_id, &prepared.pointer_hash, limits)?;
    if verified.selection != VerifiedAcceptedPublicationSelectionV1::Current
        || verified.generation_id != prepared.generation_id
    {
        return Err(invalid_pointer(
            "the installed pointer did not read back as its own current generation",
        ));
    }
    Ok(PointerCommitReceiptV1 {
        generation_id: prepared.generation_id.clone(),
        pointer_sha256: prepared.pointer_hash.clone(),
        previous_pointer_sha256,
    })
}

/// Read the installed pointer's advance tokens without verifying content.
/// The publisher surface needs them to build the prior arm and to hand a
/// caller the compare-and-swap tokens for its next advance.
pub(crate) fn installed_pointer_tokens_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<Option<(AcceptedPublicationPointerV1, PublicationSha256)>> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let Some(bytes) = read_pointer_optional_locked(paths, project_id, limits.max_pointer_bytes)?
    else {
        return Ok(None);
    };
    let digest = PublicationSha256::digest(&bytes);
    Ok(Some((decode_pointer_v1(&bytes, limits)?, digest)))
}

/// Phase-2 §7.7: rebind the publisher attachment only. The pointer's full
/// ref, accepted commit, accepted scope, generation id, hashes, and prior
/// pointer are untouched, so the strict pointer/generation startup
/// agreement holds identically before and after; ref and commit changes
/// are exclusively the later atomic advance path. Rebinding a pointer
/// whose selected generation does not verify refuses instead of moving a
/// broken binding.
pub(crate) fn rebind_pointer_attachment_locked(
    paths: &AcceptedPublicationStorePaths,
    guard: &AcceptedPublicationLockGuard,
    project_id: &ProjectId,
    new_attachment: &AttachmentId,
    expected_scope: Option<&PublishedScope>,
    limits: &AcceptedPublicationLimits,
) -> AcceptedPublicationStoreResult<AcceptedPublicationPointerV1> {
    ensure_matching_guard(paths, guard)?;
    limits.validate()?;
    let pointer_bytes = read_pointer_locked(paths, project_id, limits.max_pointer_bytes)?;
    let verified =
        verify_selected_from_pointer_locked(paths, project_id, pointer_bytes.clone(), limits)?;
    if verified.selection != VerifiedAcceptedPublicationSelectionV1::Current {
        return Err(invalid_generation(
            "rebinding requires the current accepted generation to verify",
        ));
    }
    let mut pointer = decode_pointer_v1(&pointer_bytes, limits)?;
    if let Some(expected) = expected_scope
        && &pointer.accepted_scope != expected
    {
        // Refuse before any mutation: a scope-mismatched binding must
        // never be installed, and nothing here needs restoring.
        return Err(invalid_pointer(
            "the expected scope disagrees with the pointer's accepted scope",
        ));
    }
    if pointer.version == ACCEPTED_PUBLICATION_VERSION {
        pointer.attachment_id = Some(new_attachment.clone());
        pointer.source_binding = None;
    } else {
        pointer.attachment_id = None;
        pointer.source_binding = Some(AcceptedPublicationSourceBindingV2::Attachment {
            attachment_id: new_attachment.clone(),
        });
    }
    let encoded = encode_pointer_v1(&pointer, limits)?;
    let directory = NofollowDirectory::open_existing(paths.pointers())
        .map_err(accepted_io_error)?
        .ok_or_else(|| {
            AcceptedPublicationStoreError::new(
                "error.accepted_publication_missing",
                "accepted-publication pointer directory is missing",
            )
        })?;
    directory
        .atomic_replace(&format!("{project_id}.json"), &encoded)
        .map_err(accepted_io_error)?;
    Ok(pointer)
}

fn read_pointer_locked(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    max_bytes: usize,
) -> AcceptedPublicationStoreResult<Vec<u8>> {
    read_pointer_optional_locked(paths, project_id, max_bytes)?.ok_or_else(|| {
        AcceptedPublicationStoreError::new(
            "error.accepted_publication_missing",
            "accepted-publication pointer is missing",
        )
    })
}

/// Absence and damage are different runtime states: a project that never
/// published has no pointer, while a project whose pointer cannot be read
/// or decoded is corrupt. Only an absent directory or absent file is
/// reported as `None`; every other failure stays an error.
fn read_pointer_optional_locked(
    paths: &AcceptedPublicationStorePaths,
    project_id: &ProjectId,
    max_bytes: usize,
) -> AcceptedPublicationStoreResult<Option<Vec<u8>>> {
    let Some(directory) =
        NofollowDirectory::open_existing(paths.pointers()).map_err(accepted_io_error)?
    else {
        return Ok(None);
    };
    let filename = format!("{project_id}.json");
    let bytes = directory
        .read_regular(&filename, max_bytes, "accepted-publication pointer")
        .map_err(accepted_io_error)?;
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

/// Accepted-publication builders shared by the crate's own tests. The
/// runtime facade tests need installed pointers and generations, and the
/// only honest way to produce them is the real preparation path.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::collections::HashMap;
    use std::fs;

    use bbox_gaps::gaps::GapNote;
    use bbox_knowledge::knowledge::{KnowledgeEdge, KnowledgeEntry};

    use super::*;

    pub(crate) fn knowledge_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.to_string(),
            title: "Accepted publication is path-free".to_string(),
            content: content.to_string(),
            cluster: Some("runtime".to_string()),
            variants: HashMap::new(),
            category: Category::Convention,
            scope: Scope::Project,
            project: None,
            project_id: None,
            providers: vec!["provider-a".to_string()],
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::<KnowledgeEdge>::new(),
            rationale: None,
            expires_at: None,
            source: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    pub(crate) fn gap_note(id: &str) -> GapNote {
        GapNote {
            id: id.to_string(),
            title: "Need a public accepted runtime".to_string(),
            gap_kind: GapKind::Tooling,
            domain: "publication".to_string(),
            wanted_capability: "Read verified accepted content by project id".to_string(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact: GapImpact::Medium,
            blocking_level: BlockingLevel::WorkaroundAvailable,
            dedupe_key: "tooling/publication/runtime-facade".to_string(),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: None,
            project_id: None,
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            resolved_at: None,
            resolution_note: None,
        }
    }

    /// One complete dual-lane generation plus its pointer, built through
    /// the real preparation path so every hash and id is authentic.
    pub(crate) fn prepare(
        project_id: &ProjectId,
        attachment_id: &AttachmentId,
        scope: &PublishedScope,
        accepted_commit: &str,
        content: &str,
        prior_pointer: Option<AcceptedPublicationPriorPointerV1>,
    ) -> PreparedAcceptedPublicationV1 {
        let relative = |lane: &str, id: &str| {
            if scope.bbox_root_relpath() == "." {
                format!(".bbox/{lane}/{id}.json")
            } else {
                format!("{}/.bbox/{lane}/{id}.json", scope.bbox_root_relpath())
            }
        };
        let input = AcceptedPublicationBuildInputV1 {
            project_id: project_id.clone(),
            source_binding: AcceptedPublicationBuildSourceV1::Attachment(attachment_id.clone()),
            scope: scope.clone(),
            full_ref: FullPublisherRef::parse("refs/heads/main").unwrap(),
            accepted_commit: GitObjectId::parse(accepted_commit).unwrap(),
            knowledge: vec![AcceptedKnowledgeSourceV1 {
                repository_relative_filename: relative("knowledge", "knowledge-a"),
                source_bytes: serde_json::to_vec(&knowledge_entry("knowledge-a", content)).unwrap(),
            }],
            gaps: vec![AcceptedGapSourceV1 {
                repository_relative_filename: relative("gaps", "gap-1234abcd"),
                source_bytes: serde_json::to_vec(&gap_note("gap-1234abcd")).unwrap(),
            }],
            graphs: Vec::new(),
            evidence: Vec::new(),
            // The shared fixture stays grant-free. A test that wants the
            // auto-advance policy installs the grant explicitly, so no
            // fixture consumer silently inherits one.
            auto_advance: None,
            prior_pointer,
        };
        prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap()
    }

    pub(crate) fn prior_of(
        prepared: &PreparedAcceptedPublicationV1,
    ) -> AcceptedPublicationPriorPointerV1 {
        AcceptedPublicationPriorPointerV1 {
            attachment_id: prepared.pointer.attachment_id.clone(),
            source_binding: prepared.pointer.source_binding.clone(),
            full_ref: prepared.pointer.full_ref.clone(),
            accepted_commit: prepared.pointer.accepted_commit.clone(),
            accepted_scope: prepared.pointer.accepted_scope.clone(),
            accepted_generation: prepared.pointer.accepted_generation.clone(),
            generation_hash: prepared.pointer.generation_hash.clone(),
        }
    }

    /// Install the prepared pair exactly as the catalog transaction owner
    /// would: the generation under its content-derived id, then the
    /// pointer that names it.
    pub(crate) fn install(
        paths: &AcceptedPublicationStorePaths,
        project_id: &ProjectId,
        prepared: &PreparedAcceptedPublicationV1,
    ) {
        fs::create_dir_all(paths.pointers()).unwrap();
        fs::create_dir_all(paths.generations().join(project_id.as_str())).unwrap();
        fs::write(
            paths.generation(project_id, &prepared.generation_id),
            prepared.generation_bytes.as_slice(),
        )
        .unwrap();
        fs::write(paths.pointer(project_id), prepared.pointer_bytes.as_slice()).unwrap();
    }

    /// Overwrite one installed generation file so its bytes no longer
    /// verify against the pointer that names it.
    pub(crate) fn corrupt_generation(
        paths: &AcceptedPublicationStorePaths,
        project_id: &ProjectId,
        generation_id: &AcceptedPublicationGenerationId,
    ) {
        fs::write(paths.generation(project_id, generation_id), b"corrupt").unwrap();
    }
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
            project_id: None,
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
            project_id: None,
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
            source_binding: AcceptedPublicationBuildSourceV1::Attachment(attachment_id()),
            scope: scope("."),
            full_ref: FullPublisherRef::parse("refs/heads/main").unwrap(),
            accepted_commit: GitObjectId::parse("a".repeat(40)).unwrap(),
            knowledge: vec![knowledge_source(
                "knowledge-a",
                ".bbox/knowledge/knowledge-a.json",
            )],
            gaps: vec![gap_source("gap-1234abcd", ".bbox/gaps/gap-1234abcd.json")],
            graphs: Vec::new(),
            evidence: Vec::new(),
            auto_advance: None,
            prior_pointer: None,
        }
    }

    fn governance_graph_sources() -> Vec<AcceptedGraphSourceV1Input> {
        [
            (
                "graph.json",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/graph.json"
                )
                .as_slice(),
            ),
            (
                "schema.json",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/schema.json"
                )
                .as_slice(),
            ),
            (
                "vertices.jsonl",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/vertices.jsonl"
                )
                .as_slice(),
            ),
            (
                "edges.jsonl",
                include_bytes!(
                    "../../bbox-project-graph/tests/fixtures/governance-record/edges.jsonl"
                )
                .as_slice(),
            ),
        ]
        .into_iter()
        .map(|(filename, source_bytes)| AcceptedGraphSourceV1Input {
            repository_relative_filename: format!(".bbox/graphs/governance-record/{filename}"),
            source_bytes: source_bytes.to_vec(),
        })
        .collect()
    }

    /// The accepted store gained graph fields in the same change that broke
    /// the knowledge-source store on pre-graphs state. It survives that
    /// vintage because every graph commitment is absent-equivalent: the
    /// aggregate hash is None for an empty graph lane and the count is zero,
    /// so a record written without the fields recomputes to itself.
    #[test]
    fn generation_written_before_the_graph_lane_still_validates() {
        let prepared =
            prepare_accepted_publication_v1(build_input(), &AcceptedPublicationLimits::default())
                .unwrap();
        let encoded = serde_json::to_value(&prepared.generation).unwrap();
        let mut record = encoded.as_object().unwrap().clone();
        assert!(record.remove("graph_sources").is_some());
        record
            .get_mut("counts")
            .and_then(|counts| counts.as_object_mut())
            .map(|counts| counts.remove("graph_files"))
            .unwrap()
            .unwrap();
        record
            .get_mut("hashes")
            .and_then(|hashes| hashes.as_object_mut())
            .map(|hashes| hashes.remove("graph_sources_sha256"));

        let legacy: AcceptedPublicationGenerationV1 =
            serde_json::from_value(serde_json::Value::Object(record)).unwrap();
        assert_eq!(legacy.counts.graph_files, 0);
        assert!(legacy.hashes.graph_sources_sha256.is_none());
        validate_generation_v1(&legacy, &AcceptedPublicationLimits::default()).unwrap();
    }

    fn evidence_document_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "bindings": [{
                "binding_id": "record-to-source",
                "source": {
                    "kind": "graph_vertex",
                    "graph_id": "governance-record",
                    "vertex_id": "record/case@1"
                },
                "kind": "record:CORRESPONDS_TO",
                "target": {
                    "kind": "project_file",
                    "rel_path_hash": "pathhash",
                    "chunk_hash": "chunkhash"
                },
                "assertion_authority": "project",
                "mapping_version": "mapping-v1",
                "asserted_at": "2026-01-01T00:00:00Z"
            }]
        }))
        .unwrap()
    }

    fn evidence_sources() -> Vec<AcceptedEvidenceSourceV1Input> {
        vec![AcceptedEvidenceSourceV1Input {
            repository_relative_filename: ".bbox/evidence/bindings.json".to_string(),
            source_bytes: evidence_document_bytes(),
        }]
    }

    /// Same vintage argument the graph lane made, one rung later. Every
    /// evidence commitment is absent-equivalent: the aggregate hash is None
    /// for an empty lane and the count is zero, so a generation written
    /// before the lane recomputes to itself and still validates.
    #[test]
    fn generation_written_before_the_evidence_lane_still_validates() {
        let prepared =
            prepare_accepted_publication_v1(build_input(), &AcceptedPublicationLimits::default())
                .unwrap();
        let encoded = serde_json::to_value(&prepared.generation).unwrap();
        let mut record = encoded.as_object().unwrap().clone();
        assert!(record.remove("evidence_sources").is_some());
        record
            .get_mut("counts")
            .and_then(|counts| counts.as_object_mut())
            .map(|counts| counts.remove("evidence_files"))
            .unwrap()
            .unwrap();
        record
            .get_mut("hashes")
            .and_then(|hashes| hashes.as_object_mut())
            .map(|hashes| hashes.remove("evidence_sources_sha256"));

        let legacy: AcceptedPublicationGenerationV1 =
            serde_json::from_value(serde_json::Value::Object(record)).unwrap();
        assert_eq!(legacy.counts.evidence_files, 0);
        assert!(legacy.hashes.evidence_sources_sha256.is_none());
        assert!(legacy.evidence_sources.is_empty());
        validate_generation_v1(&legacy, &AcceptedPublicationLimits::default()).unwrap();
    }

    /// A record written before BOTH lanes still opens. The two tolerances are
    /// independent, so the oldest vintage on disk has to be exercised as its
    /// own case rather than inferred from the one-lane-back case.
    #[test]
    fn generation_written_before_both_lanes_still_validates() {
        let prepared =
            prepare_accepted_publication_v1(build_input(), &AcceptedPublicationLimits::default())
                .unwrap();
        let encoded = serde_json::to_value(&prepared.generation).unwrap();
        let mut record = encoded.as_object().unwrap().clone();
        record.remove("graph_sources");
        record.remove("evidence_sources");
        if let Some(counts) = record
            .get_mut("counts")
            .and_then(|value| value.as_object_mut())
        {
            counts.remove("graph_files");
            counts.remove("evidence_files");
        }
        if let Some(hashes) = record
            .get_mut("hashes")
            .and_then(|value| value.as_object_mut())
        {
            hashes.remove("graph_sources_sha256");
            hashes.remove("evidence_sources_sha256");
        }

        let legacy: AcceptedPublicationGenerationV1 =
            serde_json::from_value(serde_json::Value::Object(record)).unwrap();
        assert_eq!(legacy.counts.graph_files, 0);
        assert_eq!(legacy.counts.evidence_files, 0);
        validate_generation_v1(&legacy, &AcceptedPublicationLimits::default()).unwrap();
    }

    #[test]
    fn candidate_builder_accepts_the_evidence_bindings_document() {
        let mut input = build_input();
        input.graphs = governance_graph_sources();
        input.evidence = evidence_sources();
        let prepared =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        assert_eq!(prepared.generation.counts.evidence_files, 1);
        assert_eq!(prepared.generation.evidence_sources.len(), 1);
        assert!(prepared.generation.hashes.evidence_sources_sha256.is_some());
        validate_generation_v1(&prepared.generation, &AcceptedPublicationLimits::default())
            .unwrap();
    }

    /// An invalid candidate document is refused at prepare time, so it never
    /// displaces a prior accepted binding set: the whole publication fails
    /// rather than landing a half-readable lane.
    #[test]
    fn candidate_builder_refuses_an_invalid_evidence_document() {
        let mut input = build_input();
        input.evidence = vec![AcceptedEvidenceSourceV1Input {
            repository_relative_filename: ".bbox/evidence/bindings.json".to_string(),
            source_bytes: br#"{"version":1,"bindings":[{"binding_id":""}]}"#.to_vec(),
        }];
        assert!(
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).is_err()
        );
    }

    #[test]
    fn candidate_builder_refuses_an_evidence_file_outside_the_lane() {
        for filename in [
            ".bbox/evidence/other.json",
            ".bbox/evidence/nested/bindings.json",
            ".bbox/graphs/bindings.json",
        ] {
            let mut input = build_input();
            input.evidence = vec![AcceptedEvidenceSourceV1Input {
                repository_relative_filename: filename.to_string(),
                source_bytes: evidence_document_bytes(),
            }];
            assert!(
                prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default())
                    .is_err(),
                "{filename} must not be accepted on the evidence lane"
            );
        }
    }

    #[test]
    fn candidate_builder_accepts_the_governance_record_graph_fixture() {
        let mut input = build_input();
        input.graphs = governance_graph_sources();
        let prepared =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        assert_eq!(prepared.generation.counts.graph_files, 4);
        assert_eq!(prepared.generation.graph_sources.len(), 4);
        assert!(prepared.generation.hashes.graph_sources_sha256.is_some());
    }

    #[test]
    fn candidate_builder_accepts_a_graph_without_the_optional_descriptor() {
        let mut input = build_input();
        input.graphs = governance_graph_sources()
            .into_iter()
            .filter(|source| !source.repository_relative_filename.ends_with("/graph.json"))
            .collect();

        let prepared =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();

        assert_eq!(prepared.generation.counts.graph_files, 3);
        assert_eq!(prepared.generation.graph_sources.len(), 3);
    }

    #[test]
    fn candidate_builder_rejects_structurally_invalid_graphs_before_install() {
        let mut input = build_input();
        input.graphs = governance_graph_sources();
        let edges = input
            .graphs
            .iter_mut()
            .find(|source| source.repository_relative_filename.ends_with("edges.jsonl"))
            .unwrap();
        edges.source_bytes =
            br#"{"from":"missing","type":"gov:OWNED_BY","to":"team-platform"}"#.to_vec();
        let error = prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default())
            .unwrap_err();
        assert_eq!(
            error.code(),
            "error.accepted_publication_invalid_generation"
        );
        assert!(error.to_string().contains("failed validation"));
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
        let identical_input = input.clone();
        let expected_bytes =
            input.knowledge[0].source_bytes.len() + input.gaps[0].source_bytes.len();
        let expected_knowledge_hash = PublicationSha256::digest(&input.knowledge[0].source_bytes);
        let first =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        let second =
            prepare_accepted_publication_v1(identical_input, &AcceptedPublicationLimits::default())
                .unwrap();
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
    fn version_one_pointer_round_trips_without_v2_fields() {
        let prepared = prepared();
        let value = serde_json::to_value(&prepared.pointer).unwrap();
        assert_eq!(value["version"], 1);
        assert!(value.get("attachment_id").is_some());
        assert!(value.get("source_binding").is_none());
        assert_eq!(
            encode_pointer_v1(
                &decode_pointer_v1(
                    &prepared.pointer_bytes,
                    &AcceptedPublicationLimits::default(),
                )
                .unwrap(),
                &AcceptedPublicationLimits::default(),
            )
            .unwrap(),
            prepared.pointer_bytes
        );
    }

    /// The grant is additive: a pointer without one serializes exactly the
    /// bytes it serialized before the field existed, so every project that
    /// never enables the policy keeps its compare-and-swap digest.
    #[test]
    fn a_pointer_without_an_auto_advance_grant_omits_the_field_entirely() {
        let prepared = prepared();
        let value = serde_json::to_value(&prepared.pointer).unwrap();
        assert!(
            value.get("auto_advance").is_none(),
            "the absent grant must not appear in pointer bytes: {value}"
        );
    }

    #[test]
    fn an_auto_advance_grant_round_trips_through_pointer_bytes() {
        let mut input = build_input();
        input.auto_advance = Some(AcceptedPublicationAutoAdvanceV1 {
            enabled: true,
            granted_reason: "operator grant for the producer lane".to_string(),
        });
        let prepared =
            prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default()).unwrap();
        let decoded = decode_pointer_v1(
            &prepared.pointer_bytes,
            &AcceptedPublicationLimits::default(),
        )
        .unwrap();
        let grant = decoded
            .auto_advance
            .as_ref()
            .expect("the grant survives a round trip");
        assert!(grant.enabled);
        assert_eq!(grant.granted_reason, "operator grant for the producer lane");
        // Re-encoding the WHOLE decoded pointer is the point: the grant has
        // to survive the round trip in place, not merely be readable, so
        // `decoded` must still be intact here.
        assert_eq!(
            encode_pointer_v1(&decoded, &AcceptedPublicationLimits::default()).unwrap(),
            prepared.pointer_bytes
        );
    }

    /// The grant records WHY it exists. A blank reason would leave an
    /// audited acceptance pointing at nothing, so it is refused at the
    /// same layer that refuses every other malformed pointer field.
    #[test]
    fn an_auto_advance_grant_without_an_operator_reason_is_refused() {
        let mut input = build_input();
        input.auto_advance = Some(AcceptedPublicationAutoAdvanceV1 {
            enabled: true,
            granted_reason: "   ".to_string(),
        });
        let error = prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default())
            .expect_err("a reasonless grant is not a valid pointer");
        assert_eq!(error.code(), "error.accepted_publication_invalid_pointer");
    }

    #[test]
    fn an_oversized_auto_advance_reason_is_refused() {
        let mut input = build_input();
        input.auto_advance = Some(AcceptedPublicationAutoAdvanceV1 {
            enabled: true,
            granted_reason: "r".repeat(MAX_AUTO_ADVANCE_REASON_BYTES + 1),
        });
        let error = prepare_accepted_publication_v1(input, &AcceptedPublicationLimits::default())
            .expect_err("an unbounded reason is not a valid pointer");
        assert_eq!(error.code(), "error.accepted_publication_invalid_pointer");
    }

    #[test]
    fn producer_pointer_v2_preserves_v1_prior_and_attachment_advance_stays_v2() {
        let first = prepared();
        let mut producer_input = build_input();
        producer_input.accepted_commit = GitObjectId::parse("b".repeat(40)).unwrap();
        producer_input.prior_pointer = Some(fixtures::prior_of(&first));
        producer_input.source_binding = AcceptedPublicationBuildSourceV1::Producer {
            producer_id: "producer-a".to_string(),
            source_generation_id: format!("kps_{}", "1".repeat(64)),
            source_generation_sha256: PublicationSha256::parse("2".repeat(64)).unwrap(),
        };
        let producer =
            prepare_accepted_publication_v1(producer_input, &AcceptedPublicationLimits::default())
                .unwrap();
        assert_eq!(producer.pointer.version, ACCEPTED_PUBLICATION_POINTER_V2);
        assert!(producer.pointer.attachment_id.is_none());
        assert!(matches!(
            pointer_source_binding(&producer.pointer).unwrap(),
            AcceptedPublicationSourceBindingV2::Producer {
                ref producer_id,
                ref source_generation_id,
                ..
            } if producer_id == "producer-a" && source_generation_id.starts_with("kps_")
        ));
        let prior = producer.pointer.prior_pointer.as_ref().unwrap();
        assert_eq!(prior.attachment_id, first.pointer.attachment_id);
        assert!(prior.source_binding.is_none());
        assert_eq!(
            decode_pointer_v1(
                &producer.pointer_bytes,
                &AcceptedPublicationLimits::default(),
            )
            .unwrap(),
            producer.pointer
        );

        let mut attachment_input = build_input();
        attachment_input.accepted_commit = GitObjectId::parse("c".repeat(40)).unwrap();
        attachment_input.prior_pointer = Some(fixtures::prior_of(&producer));
        let attachment = prepare_accepted_publication_v1(
            attachment_input,
            &AcceptedPublicationLimits::default(),
        )
        .unwrap();
        assert_eq!(attachment.pointer.version, ACCEPTED_PUBLICATION_POINTER_V2);
        assert!(attachment.pointer.attachment_id.is_none());
        assert!(matches!(
            pointer_source_binding(&attachment.pointer).unwrap(),
            AcceptedPublicationSourceBindingV2::Attachment { .. }
        ));
        assert!(matches!(
            attachment
                .pointer
                .prior_pointer
                .as_ref()
                .and_then(|prior| prior.source_binding.as_ref()),
            Some(AcceptedPublicationSourceBindingV2::Producer { .. })
        ));
    }

    #[test]
    fn pointer_versions_refuse_missing_ambiguous_and_invalid_source_bindings() {
        let limits = AcceptedPublicationLimits::default();
        let mut pointer = prepared().pointer;
        pointer.version = ACCEPTED_PUBLICATION_POINTER_V2;
        assert!(encode_pointer_v1(&pointer, &limits).is_err());

        pointer.attachment_id = None;
        pointer.source_binding = Some(AcceptedPublicationSourceBindingV2::Producer {
            producer_id: "bad producer".to_string(),
            source_generation_id: format!("kps_{}", "1".repeat(64)),
            source_generation_sha256: PublicationSha256::parse("2".repeat(64)).unwrap(),
        });
        assert!(encode_pointer_v1(&pointer, &limits).is_err());

        pointer.attachment_id = Some(attachment_id());
        assert!(encode_pointer_v1(&pointer, &limits).is_err());
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
    fn rebind_changes_attachment_only_and_selected_generation_survives() {
        let directory = tempfile::tempdir().unwrap();
        let projects_path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("projects.json");
        let paths = AcceptedPublicationStorePaths::derive(&projects_path).unwrap();
        let prepared = prepared();
        install_prepared(&paths, &prepared);
        let guard = acquire_accepted_publication_lock(&paths).unwrap();
        let limits = AcceptedPublicationLimits::default();

        let before = verify_selected_locked(&paths, &guard, &project_id(), &limits).unwrap();
        let new_attachment = AttachmentId::parse("att_0000000000000000000000000000f001").unwrap();
        assert_ne!(prepared.pointer.attachment_id, Some(new_attachment.clone()));

        let rebound = rebind_pointer_attachment_locked(
            &paths,
            &guard,
            &project_id(),
            &new_attachment,
            Some(&prepared.pointer.accepted_scope),
            &limits,
        )
        .unwrap();
        let scope_mismatch = PublishedScope::try_new("wrongfamily", ".").unwrap();
        let error = rebind_pointer_attachment_locked(
            &paths,
            &guard,
            &project_id(),
            &new_attachment,
            Some(&scope_mismatch),
            &limits,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.accepted_publication_invalid_pointer");
        assert_eq!(rebound.attachment_id, Some(new_attachment));
        assert_eq!(rebound.full_ref, prepared.pointer.full_ref);
        assert_eq!(rebound.accepted_commit, prepared.pointer.accepted_commit);
        assert_eq!(
            rebound.accepted_generation,
            prepared.pointer.accepted_generation
        );
        assert_eq!(rebound.generation_hash, prepared.pointer.generation_hash);

        // The phase-2 regression requirement: after rebinding, strict
        // selected verification serves the exact same accepted generation.
        let after = verify_selected_locked(&paths, &guard, &project_id(), &limits).unwrap();
        assert_eq!(
            after.selection,
            VerifiedAcceptedPublicationSelectionV1::Current
        );
        assert_eq!(after.generation_id, before.generation_id);
        assert_eq!(after.generation_bytes, before.generation_bytes);
    }

    #[test]
    fn corrupt_current_generation_falls_back_only_to_verified_prior() {
        let limits = AcceptedPublicationLimits::default();
        let first = prepared();
        let prior = AcceptedPublicationPriorPointerV1 {
            attachment_id: first.pointer.attachment_id.clone(),
            source_binding: first.pointer.source_binding.clone(),
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
