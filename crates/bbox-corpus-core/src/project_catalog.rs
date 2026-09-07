//! Pure durable project-catalog and host-local attachment contracts.
//!
//! Catalog records contain logical identity only. Filesystem paths exist only
//! in the separately validated attachment snapshot. This module performs no
//! filesystem, Git, configuration, or daemon access.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use schemars::JsonSchema;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Number, Value};
use uuid::Uuid;

use crate::identity::PublishedScope;
use crate::language::Language;

pub const CATALOG_VERSION_V2: u32 = 2;
/// Catalog wire version that carries the connector scope family.
///
/// The version is DERIVED from content, never chosen: a catalog holding no
/// connector scope keeps writing [`CATALOG_VERSION_V2`] bytes, so a daemon
/// predating this family still opens it. See
/// [`CatalogSnapshotV2::required_version`].
pub const CATALOG_VERSION_V3: u32 = 3;
/// Highest catalog wire version this build can open.
pub const CATALOG_VERSION_MAX: u32 = CATALOG_VERSION_V3;
pub const ATTACHMENT_VERSION_V1: u32 = 1;
pub const LEGACY_PROJECT_STORE_VERSION_V1: u32 = 1;
pub const MAX_PROJECT_CATALOG_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_LEGACY_PROJECT_STORE_BYTES: usize = 512 * 1024 * 1024;
pub const MAX_PROJECT_CATALOG_ENTRIES: usize = 100_000;

const MAX_PROJECT_ID_BYTES: usize = 96;
const MAX_AUTHORITY_BYTES: usize = 256;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_TIMESTAMP_BYTES: usize = 128;
const MAX_AUDIT_SOURCE_BYTES: usize = 256;
const MAX_AUDIT_REASON_BYTES: usize = 1024;
const MAX_PATH_BYTES: usize = 4096;
const MIN_CONNECTOR_SOURCE_ID_BYTES: usize = 8;
const MAX_CONNECTOR_SOURCE_ID_BYTES: usize = 128;
const MAX_CONNECTOR_KIND_BYTES: usize = 64;
const MAX_CONNECTOR_OBSERVATION_BYTES: usize = 256;
const MINT_RETRIES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogError {
    code: &'static str,
    detail: String,
}

impl ProjectCatalogError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: bounded_detail(detail.into()),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ProjectCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProjectCatalogError {}

fn bounded_detail(detail: String) -> String {
    let sanitized = detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    sanitized.chars().take(384).collect()
}

fn deserialize_unique_btree_set<'de, D, T>(deserializer: D) -> Result<BTreeSet<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Ord,
{
    let values = Vec::<T>::deserialize(deserializer)?;
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value) {
            return Err(de::Error::custom("duplicate strict-set value"));
        }
    }
    Ok(unique)
}

fn invalid_id(kind: &'static str) -> ProjectCatalogError {
    ProjectCatalogError::new(
        "error.project_catalog_invalid_id",
        format!("invalid {kind}"),
    )
}

fn validate_project_id(value: &str) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > MAX_PROJECT_ID_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_id("project_id"));
    }
    Ok(())
}

fn validate_authority_token(value: &str, kind: &'static str) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > MAX_AUTHORITY_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_id(kind));
    }
    Ok(())
}

fn validate_commit_namespace(value: &str) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > MAX_AUTHORITY_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_id("commit_namespace"));
    }
    Ok(())
}

/// Shape rule for the operator-minted `connector_source_id`.
///
/// The id is IDENTITY for a connector-scoped project and the daemon can
/// never recompute it from anything: it is minted once by the operator and
/// written into both sides of the two-sided config (the daemon's producer
/// grant and the satellite's source config). The daemon therefore validates
/// shape only, and the shape exists to kill whole classes of transcription
/// and confusion error rather than to prove anything about the remote store:
///
/// - **opaque**: no structure is parsed out of it and none is implied. It is
///   not a path, a URL, a repo id, or a vendor coordinate;
/// - **length-bounded**: 8 to 128 bytes. The floor refuses ids too short to
///   survive a transcription slip between two config files; the ceiling
///   bounds the durable record;
/// - **non-path-shaped**: ASCII lowercase alphanumerics plus `_`, `-`, and
///   `.`, which excludes `/`, `\`, `:`, whitespace, and control bytes. It
///   must start and end with an alphanumeric (so `.`, `..`, and dotfile
///   shapes are refused) and may not contain `..` anywhere;
/// - **lowercase**: two ids differing only by case would be two distinct
///   durable scopes that no human review would separate, so case is not a
///   distinguishing dimension at all.
///
/// Nothing here mandates a mint algorithm. A UUID, a random token, or a
/// readable slug all pass; the recommended form is a short prefix plus a
/// UUID (`csrc_5f2c…`), which keeps a connector scope legible in logs.
fn validate_connector_source_id(value: &str) -> Result<(), ProjectCatalogError> {
    let bytes = value.as_bytes();
    if bytes.len() < MIN_CONNECTOR_SOURCE_ID_BYTES || bytes.len() > MAX_CONNECTOR_SOURCE_ID_BYTES {
        return Err(invalid_id("connector_source_id"));
    }
    if !bytes
        .iter()
        .all(|byte| is_connector_id_byte(*byte) || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_id("connector_source_id"));
    }
    if !is_connector_id_byte(bytes[0]) || !is_connector_id_byte(bytes[bytes.len() - 1]) {
        return Err(invalid_id("connector_source_id"));
    }
    if value.contains("..") {
        return Err(invalid_id("connector_source_id"));
    }
    Ok(())
}

fn is_connector_id_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

/// Shape rule for `connector_kind`, the operator's declared connector family
/// (`gdrive`, `graph`, `webdav`, `s3`, ...).
///
/// It is deliberately an opaque validated token and NOT a closed enum: the
/// connector program's first invariant is that Blackbox core grows no
/// vendor-specific variants, so the corpus never learns the catalog of
/// vendors. Lowercase letters and digits plus `_`, leading letter, bounded.
fn validate_connector_kind(value: &str) -> Result<(), ProjectCatalogError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_CONNECTOR_KIND_BYTES {
        return Err(invalid_id("connector_kind"));
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Err(invalid_id("connector_kind"));
    }
    if !bytes[0].is_ascii_lowercase() {
        return Err(invalid_id("connector_kind"));
    }
    Ok(())
}

fn validate_minted_id(
    value: &str,
    prefix: &'static str,
    kind: &'static str,
) -> Result<(), ProjectCatalogError> {
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(invalid_id(kind));
    };
    if hex.len() != 32
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_id(kind));
    }
    Ok(())
}

/// Validate a content-addressed id shape: `prefix` plus 64 lowercase hex
/// characters (a SHA-256 digest). Distinct from [`validate_minted_id`]'s
/// 32-character random-mint shape: these ids are derived elsewhere from
/// generation content, never randomly minted here (Phase 3 plan section 5).
fn validate_content_addressed_id(
    value: &str,
    prefix: &'static str,
    kind: &'static str,
) -> Result<(), ProjectCatalogError> {
    let Some(hex) = value.strip_prefix(prefix) else {
        return Err(invalid_id(kind));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(invalid_id(kind));
    }
    Ok(())
}

macro_rules! parsed_string_type {
    ($name:ident, $validator:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, ProjectCatalogError> {
                let value = value.into();
                ($validator)(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

parsed_string_type!(ProjectId, validate_project_id);
parsed_string_type!(RecordedRepoAuthority, |value: &str| {
    validate_authority_token(value, "recorded_repo_authority")
});
parsed_string_type!(RepoBootstrapHint, |value: &str| {
    validate_authority_token(value, "repo_bootstrap_hint")
});
parsed_string_type!(CommitNamespace, validate_commit_namespace);
parsed_string_type!(RepoHistoryId, |value: &str| {
    validate_minted_id(value, "rh_", "repo_history_id")
});
parsed_string_type!(AttachmentId, |value: &str| {
    validate_minted_id(value, "att_", "attachment_id")
});
parsed_string_type!(ScopeMigrationId, |value: &str| {
    validate_minted_id(value, "sm_", "scope_migration_id")
});
parsed_string_type!(LegacyPathBindingId, |value: &str| {
    validate_minted_id(value, "lpb_", "legacy_path_binding_id")
});
parsed_string_type!(ProjectCatalogTransactionId, |value: &str| {
    validate_minted_id(value, "pct_", "project_catalog_transaction_id")
});
parsed_string_type!(ConnectorSourceId, validate_connector_source_id);
parsed_string_type!(ConnectorKind, validate_connector_kind);
parsed_string_type!(RepoHistoryGenerationId, |value: &str| {
    validate_content_addressed_id(value, "rhg_", "repo_history_generation_id")
});
parsed_string_type!(RepoHistoryQuarantineGenerationId, |value: &str| {
    validate_content_addressed_id(value, "rhq_", "repo_history_quarantine_generation_id")
});

fn random_id(prefix: &str) -> String {
    format!("{prefix}{}", Uuid::new_v4().simple())
}

impl ProjectId {
    pub fn mint(catalog: &CatalogSnapshotV2) -> Result<Self, ProjectCatalogError> {
        Self::mint_with(|| random_id("p_"), |id| catalog.projects.contains_key(id))
    }

    fn mint_with(
        mut candidate: impl FnMut() -> String,
        mut contains: impl FnMut(&ProjectId) -> bool,
    ) -> Result<Self, ProjectCatalogError> {
        for _ in 0..MINT_RETRIES {
            let id = Self::parse(candidate())?;
            if !contains(&id) {
                return Ok(id);
            }
        }
        Err(ProjectCatalogError::new(
            "error.project_catalog_id_collision",
            "project_id mint retry limit exhausted",
        ))
    }
}

macro_rules! impl_random_mint {
    ($name:ident, $prefix:literal) => {
        impl $name {
            pub fn mint() -> Self {
                Self::parse(random_id($prefix)).expect("code-owned random id must validate")
            }
        }
    };
}

impl_random_mint!(RepoHistoryId, "rh_");
impl_random_mint!(AttachmentId, "att_");
impl_random_mint!(ScopeMigrationId, "sm_");
impl_random_mint!(LegacyPathBindingId, "lpb_");
impl_random_mint!(ProjectCatalogTransactionId, "pct_");

impl CommitNamespace {
    /// Mint a code-owned namespace for one `LocalProject` history record.
    ///
    /// The history authority binds the random namespace to its project. The
    /// namespace itself contains no project, path, alias, repository, or ref
    /// bytes.
    pub fn mint_local(catalog: &CatalogSnapshotV2) -> Result<Self, ProjectCatalogError> {
        for _ in 0..MINT_RETRIES {
            let namespace = Self::parse(random_id("local_"))?;
            let owned = catalog.repo_histories.values().any(|history| {
                history.primary_namespace == namespace
                    || history.compatibility_namespaces.contains(&namespace)
            });
            if !owned && !catalog.ambiguous_namespaces.contains_key(&namespace) {
                return Ok(namespace);
            }
        }
        Err(ProjectCatalogError::new(
            "error.project_catalog_id_collision",
            "local commit namespace mint retry limit exhausted",
        ))
    }
}

fn is_local_commit_namespace(namespace: &CommitNamespace) -> bool {
    validate_minted_id(namespace.as_str(), "local_", "local_commit_namespace").is_ok()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema)]
#[serde(
    tag = "kind",
    content = "scope",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProjectScope {
    Published(PublishedScope),
    LegacyLocal,
    /// A remote source with no git repository and no committed config: a
    /// document store folder, a workspace, an API tenant.
    ///
    /// Identity is the operator's declaration, not a vendor fact. See
    /// [`ConnectorScope`].
    Connector(ConnectorScope),
}

impl ProjectScope {
    fn validate(&self) -> Result<(), ProjectCatalogError> {
        match self {
            Self::Published(scope) => scope.validate().map_err(|error| {
                ProjectCatalogError::new(
                    "error.project_catalog_invalid_scope",
                    format!("invalid published scope {}", error.field()),
                )
            })?,
            Self::LegacyLocal => {}
            Self::Connector(scope) => scope.validate()?,
        }
        Ok(())
    }

    /// Stable wire label for this scope family, shared by every read surface
    /// so `kind` reads identically from the catalog, the CLI, and the tools.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Published(_) => "published",
            Self::LegacyLocal => "legacy_local",
            Self::Connector(_) => "connector",
        }
    }

    /// The published scope, or `None` for every family that has none. A
    /// connector scope is never a published scope: it names no repository,
    /// no commit, and no checkout.
    pub fn published(&self) -> Option<&PublishedScope> {
        match self {
            Self::Published(scope) => Some(scope),
            Self::LegacyLocal | Self::Connector(_) => None,
        }
    }

    pub fn connector(&self) -> Option<&ConnectorScope> {
        match self {
            Self::Connector(scope) => Some(scope),
            Self::Published(_) | Self::LegacyLocal => None,
        }
    }
}

/// Durable identity for a connector source.
///
/// Resolved by operator decision (2026-08-12): identity is a grant-time,
/// operator-minted, opaque `connector_source_id` written into both sides of
/// the two-sided config. Provider coordinates (folder ids, drive ids,
/// workspace ids, tenant ids) are NOT identity and never enter this type;
/// they travel as replaceable observations
/// ([`ConnectorObservationsV1`]) exactly as absolute paths travel as
/// attachment observations for a git source.
///
/// Why not vendor coordinates: "stable" is per-vendor and softer than a
/// commit hash (folder moves survive, tenant migrations and account
/// transfers do not), WebDAV and S3 have no id concept at all, and the
/// daemon can independently recompute none of it. The accepted cost is that
/// two operators onboarding the same folder mint two scopes with no
/// mechanical convergence; that is what `LegacyLocal` already pays and is
/// closable later by an operator-declared alias, which is additive.
///
/// `connector_kind` rides along as the operator's grant-time declaration of
/// which connector family serves the source. It is durable because every
/// read surface must report the family honestly and Phase 0 has no other
/// durable home for it, but it is NOT part of identity: catalog uniqueness
/// and every grant lookup key on `connector_source_id` alone, so one id can
/// never name two projects even under a mistyped kind.
#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ConnectorScope {
    connector_source_id: ConnectorSourceId,
    connector_kind: ConnectorKind,
}

impl ConnectorScope {
    pub fn new(connector_source_id: ConnectorSourceId, connector_kind: ConnectorKind) -> Self {
        Self {
            connector_source_id,
            connector_kind,
        }
    }

    /// Parse and validate both halves from operator-supplied strings.
    pub fn try_new(
        connector_source_id: impl Into<String>,
        connector_kind: impl Into<String>,
    ) -> Result<Self, ProjectCatalogError> {
        Ok(Self {
            connector_source_id: ConnectorSourceId::parse(connector_source_id)?,
            connector_kind: ConnectorKind::parse(connector_kind)?,
        })
    }

    pub fn connector_source_id(&self) -> &ConnectorSourceId {
        &self.connector_source_id
    }

    pub fn connector_kind(&self) -> &ConnectorKind {
        &self.connector_kind
    }

    pub fn validate(&self) -> Result<(), ProjectCatalogError> {
        validate_connector_source_id(self.connector_source_id.as_str())?;
        validate_connector_kind(self.connector_kind.as_str())?;
        Ok(())
    }
}

impl fmt::Display for ConnectorScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.connector_kind, self.connector_source_id)
    }
}

/// Vendor coordinates a producer OBSERVED for a connector-scoped project.
///
/// Every field here is an observation: replaceable on the next onboarding
/// report, never compared for identity, never a lookup key, and never a
/// reason to accept or refuse anything. The daemon cannot recompute any of
/// it and does not pretend to verify it. Recording it is what lets an
/// operator see which folder or tenant a durable scope is pointed at without
/// letting a vendor coordinate become the scope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectorObservationsV1 {
    /// When the producer reported these coordinates.
    pub observed_at: String,
    /// The producer that reported them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    /// Vendor tenant or account the source was observed under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_authority: Option<String>,
    /// The store's own id for the scope root (a folder file id, a drive plus
    /// item id, a bucket plus prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_root_id: Option<String>,
    /// Human-facing name the vendor shows for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_display_name: Option<String>,
}

impl ConnectorObservationsV1 {
    pub fn validate(&self) -> Result<(), ProjectCatalogError> {
        validate_timestamp(&self.observed_at, "connector observation observed_at")?;
        for (value, field) in [
            (&self.producer_id, "connector observation producer_id"),
            (
                &self.remote_authority,
                "connector observation remote_authority",
            ),
            (&self.remote_root_id, "connector observation remote_root_id"),
            (
                &self.remote_display_name,
                "connector observation remote_display_name",
            ),
        ] {
            if let Some(value) = value {
                validate_observation_text(value, field)?;
            }
        }
        Ok(())
    }
}

/// Observations are operator-visible vendor text, so they are bounded and
/// control-free, and nothing else. They are never parsed.
fn validate_observation_text(value: &str, field: &'static str) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > MAX_CONNECTOR_OBSERVATION_BYTES
        || value.chars().any(|ch| ch.is_control())
    {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_invalid_connector_observation",
            format!("invalid {field}"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CorpusProject {
    pub project_id: ProjectId,
    pub scope: ProjectScope,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub operator_aliases: BTreeSet<String>,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub nominated_aliases: BTreeSet<String>,
    pub display_name: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at_compat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_history: Option<RepoHistoryId>,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub languages: BTreeSet<Language>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(
    tag = "kind",
    content = "authority",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RepoHistoryAuthority {
    Recorded(RecordedRepoAuthority),
    LocalProject(ProjectId),
    LegacyNamespace(CommitNamespace),
}

/// Whether a repo-history record's immutable commit/vector generation has
/// been built (Phase 3 plan section 4.1). Ships `#[serde(default)]` so v2
/// catalog bytes written before this field decode unchanged as `NotBuilt`.
/// The v1 importer (Phase 3) writes this field explicitly rather than
/// relying on the default; `NotBuilt` here still means "the importer has not
/// materialized history," identical in meaning to a defaulted value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepoHistoryMaterialization {
    #[default]
    NotBuilt,
    Ready {
        generation_id: RepoHistoryGenerationId,
    },
}

/// Quarantine-side counterpart of [`RepoHistoryMaterialization`], scoped to
/// [`AmbiguousNamespaceRecord`] and keyed by
/// [`RepoHistoryQuarantineGenerationId`] instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepoHistoryQuarantineMaterialization {
    #[default]
    NotBuilt,
    Ready {
        generation_id: RepoHistoryQuarantineGenerationId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRecord {
    pub repo_history_id: RepoHistoryId,
    /// Monotonic watermark for the ordered set of catalog projects that
    /// reference this repository history, including each project's scope.
    /// Older v2 catalog bytes decode as the pre-watermark generation zero.
    #[serde(default)]
    pub membership_generation: u64,
    pub authority: RepoHistoryAuthority,
    pub primary_namespace: CommitNamespace,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub compatibility_namespaces: BTreeSet<CommitNamespace>,
    #[serde(default)]
    pub materialization: RepoHistoryMaterialization,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AmbiguousNamespaceStatus {
    Quarantined,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AmbiguousNamespaceRecord {
    pub namespace: CommitNamespace,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub candidate_repo_history_ids: BTreeSet<RepoHistoryId>,
    pub status: AmbiguousNamespaceStatus,
    #[serde(default)]
    pub materialization: RepoHistoryQuarantineMaterialization,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMigrationAuthorityProvenance {
    AttachmentProved,
    OperatorAttested,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMigrationKind {
    Promotion,
    RelpathMove,
    RepoAuthorityChange,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentCapability {
    LocalCodeSource,
    GitHistory,
    Blame,
    RepoKnowledge,
    RepoMutation,
    RenderOutput,
    ProvenanceNoteIo,
    ArtifactWatching,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeMigrationRecord {
    pub scope_migration_id: ScopeMigrationId,
    pub project_id: ProjectId,
    pub catalog_epoch: u64,
    pub authority_provenance: ScopeMigrationAuthorityProvenance,
    pub operator_invocation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_reason: Option<String>,
    pub old_scope: ProjectScope,
    pub new_scope: ProjectScope,
    pub kind: ScopeMigrationKind,
    pub migrated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_bridge_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_bridge_generation: Option<String>,
    #[serde(deserialize_with = "deserialize_unique_btree_set")]
    pub pending_capabilities: BTreeSet<AttachmentCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogOriginV2 {
    FreshV2 {},
    MigratedV1 {
        transaction_id: ProjectCatalogTransactionId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshotV2 {
    pub version: u32,
    pub epoch: u64,
    pub origin: CatalogOriginV2,
    pub projects: BTreeMap<ProjectId, CorpusProject>,
    pub repo_histories: BTreeMap<RepoHistoryId, RepoHistoryRecord>,
    pub ambiguous_namespaces: BTreeMap<CommitNamespace, AmbiguousNamespaceRecord>,
    pub scope_migrations: BTreeMap<ScopeMigrationId, ScopeMigrationRecord>,
    /// Observed vendor coordinates for connector-scoped projects, keyed by
    /// project id. Absent from the bytes entirely when empty, which is what
    /// keeps a connector-free catalog byte-identical to a pre-connector
    /// write. An entry for a project that is not connector-scoped is
    /// refused: only a connector project can carry connector observations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub connector_observations: BTreeMap<ProjectId, ConnectorObservationsV1>,
}

impl CatalogSnapshotV2 {
    pub fn empty(epoch: u64) -> Result<Self, ProjectCatalogError> {
        let snapshot = Self {
            version: CATALOG_VERSION_V2,
            epoch,
            origin: CatalogOriginV2::FreshV2 {},
            projects: BTreeMap::new(),
            repo_histories: BTreeMap::new(),
            ambiguous_namespaces: BTreeMap::new(),
            scope_migrations: BTreeMap::new(),
            connector_observations: BTreeMap::new(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), ProjectCatalogError> {
        validate_catalog(self)
    }

    /// The lowest catalog wire version that can represent this content.
    ///
    /// This is the whole downgrade story. The version field is DERIVED, not
    /// chosen: a catalog holding no connector scope reports
    /// [`CATALOG_VERSION_V2`] and is written as v2 bytes, so a daemon
    /// predating the connector family opens it unchanged. The moment one
    /// connector scope exists the catalog reports [`CATALOG_VERSION_V3`] and
    /// an older daemon FAILS CLOSED rather than silently dropping projects
    /// it cannot represent. Losing a connector project to a downgrade would
    /// orphan its content and free its scope for reuse, so refusing to open
    /// is the only honest outcome.
    ///
    /// Where that refusal comes from, because the two layers produce
    /// different-looking errors and an operator mid-rollback should
    /// recognize either:
    ///
    /// - **The startup version probe** (`probe_project_store_mode`, run by
    ///   `src/server/open.rs` before any project-scoped subsystem starts)
    ///   deserializes only the `version` field and refuses an unknown one
    ///   with `error.project_catalog_unsupported_version`. This is what a
    ///   real rolled-back daemon hits, and it never parses a project row.
    ///   The version number is therefore load-bearing, not forensic: it is
    ///   the entire mechanism by which an older build knows to stop.
    /// - **A strict row decode** reached some other way refuses later and
    ///   differently, with serde's `unknown variant 'connector'`, because
    ///   the scope enum denies unknown variants. Still fail-closed, just not
    ///   the typed refusal.
    pub fn required_version(&self) -> u32 {
        if self
            .projects
            .values()
            .any(|project| matches!(project.scope, ProjectScope::Connector(_)))
        {
            CATALOG_VERSION_V3
        } else {
            CATALOG_VERSION_V2
        }
    }

    /// Set `version` to exactly what the content requires. The durable store
    /// calls this on the write path; a transaction closure never chooses the
    /// version itself.
    pub fn sync_version(&mut self) {
        self.version = self.required_version();
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Base,
    Worktree,
    ManagedClone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentStatus {
    Attached,
    Detached,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentCapabilities {
    pub local_code_source: bool,
    pub git_history: bool,
    pub blame: bool,
    pub repo_knowledge: bool,
    pub repo_mutation: bool,
    pub render_output: bool,
    pub provenance_note_io: bool,
    pub artifact_watching: bool,
}

impl AttachmentCapabilities {
    pub fn any(self) -> bool {
        self.local_code_source
            || self.git_history
            || self.blame
            || self.repo_knowledge
            || self.repo_mutation
            || self.render_output
            || self.provenance_note_io
            || self.artifact_watching
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutAttachment {
    pub attachment_id: AttachmentId,
    pub project_id: ProjectId,
    pub checkout_id: String,
    pub checkout_dir: String,
    pub checkout_project_dir: String,
    pub project_root_relpath: String,
    pub kind: AttachmentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_scope: Option<PublishedScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_repo_hint: Option<RepoBootstrapHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    pub capabilities: AttachmentCapabilities,
    pub status: AttachmentStatus,
    pub attached_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPathRelationship {
    Root,
    ContainedSubdirectory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyPathBindingStatus {
    Mapped {
        project_id: ProjectId,
        relationship: LegacyPathRelationship,
    },
    Unscoped {},
    Quarantined {},
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, JsonSchema)]
pub struct LegacyPathLedgerEntry {
    pub legacy_path_binding_id: LegacyPathBindingId,
    pub historical_path: String,
    pub source_store: String,
    pub source_row_id: String,
    /// How many owner rows the obligation stood for when it was reviewed, and a
    /// canonical ordered commitment over their ids.
    ///
    /// Persisted, not merely inventoried, because the ONLY thing that makes the
    /// evidence worth recording is something rederiving it at the moment of
    /// writing and again at verification. Without it here, a member removed,
    /// duplicated, or substituted after review leaves the survivors uniformly
    /// stamped, and both halves report success over a set nobody approved.
    ///
    /// Every owner carries both: the small stores are singletons (count 1, a
    /// commitment over their one row id), so the shape is uniform across the
    /// owner set rather than special-cased for the line-oriented one.
    pub member_row_count: u64,
    pub member_commitment_sha256: String,
    pub inventory_epoch: u64,
    pub status: LegacyPathBindingStatus,
}

/// Ledger sources whose PRE-EVIDENCE bindings can have their member evidence
/// reconstructed at decode, because in that schema one binding was one row.
///
/// An ALLOW-LIST, not a deny-list, and that direction is the whole safety
/// property. Reconstruction invents evidence, so it may only be applied where
/// the answer is knowable without asking the owner. Anything absent from this
/// list - a group-shaped owner, or a token this version does not recognize -
/// refuses rather than being guessed at, so a source added later cannot be
/// silently mis-reconstructed by an older rule.
///
/// The tokens mirror `bbox_indexing::project_catalog_backfill::legacy_store_token`
/// across a crate boundary that cannot be imported the other way; a test there
/// walks the owner set and pins the two lists together.
const RECONSTRUCTABLE_LEDGER_SOURCES: &[&str] = &[
    "knowledge",
    "gap",
    "thread",
    "note",
    "pin",
    // Inert legacy ledger evidence must not make surviving catalogs unreadable.
    "roadmap",
    "packet",
    "task",
    "proposal",
    "slack",
    "whiteboard",
    "artifact",
    // Cannot produce a legacy-selector binding at all (exempt by construction);
    // listed so that if one somehow exists it refuses at the backfill's own
    // exemption check, with that diagnostic, rather than here with a shape one.
    "provenance",
    // Host-local relocation records name exactly the attachment they carry.
    "attachment-relocation",
];

/// Whether a pre-evidence binding of this source can have its member evidence
/// derived from its `source_row_id` alone.
///
/// `false` for the transcript-edge owner, whose binding names a SELECTOR GROUP
/// standing for an unknown number of physical lane rows. Deriving a singleton
/// there would embed evidence that no refold can ever reproduce, so the stamp
/// and the verify would refuse that binding forever and no fresh preflight
/// could repair it: a durable dead end dressed up as a compatibility path.
pub fn legacy_ledger_evidence_is_reconstructable(source_store: &str) -> bool {
    RECONSTRUCTABLE_LEDGER_SOURCES.contains(&source_store)
}

/// Decode a ledger entry written by ANY version of this schema.
///
/// The member evidence is required in everything this binary writes, but it did
/// not exist before, and a host that has ever relocated an attachment already
/// has a nonempty ledger. A strict derive would refuse to decode that host's
/// attachment snapshot outright, before a migration or backfill could run at
/// all, which turns an additive field into an unopenable store.
///
/// Absence is therefore read as the SINGLETON evidence the entry always
/// implied: one row, committed over its own `source_row_id` - but ONLY for the
/// sources where that is true (see
/// [`legacy_ledger_evidence_is_reconstructable`]). A group-shaped source keeps
/// its absent evidence and is refused by validation, because inventing a count
/// there is worse than refusing: it cannot be detected later and it cannot be
/// repaired.
///
/// Absence is distinguished from a written zero on purpose. Normalization
/// applies ONLY when both halves are missing; a partially written or
/// explicitly zeroed pair is left as read and refused by
/// [`AttachmentSnapshotV1::validate`], so a corrupt or truncated entry cannot
/// be laundered into valid-looking evidence. Serialization always emits both
/// fields, so nothing this binary writes is ever ambiguous.
impl<'de> Deserialize<'de> for LegacyPathLedgerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnDisk {
            legacy_path_binding_id: LegacyPathBindingId,
            historical_path: String,
            source_store: String,
            source_row_id: String,
            #[serde(default)]
            member_row_count: Option<u64>,
            #[serde(default)]
            member_commitment_sha256: Option<String>,
            inventory_epoch: u64,
            status: LegacyPathBindingStatus,
        }

        let entry = OnDisk::deserialize(deserializer)?;
        let (member_row_count, member_commitment_sha256) =
            match (entry.member_row_count, entry.member_commitment_sha256) {
                (None, None) if legacy_ledger_evidence_is_reconstructable(&entry.source_store) => {
                    let members = crate::project_catalog_snapshot::singleton_selector_members(
                        &entry.source_row_id,
                    );
                    (members.row_count, members.commitment_sha256)
                }
                // Anything else is taken as written. A missing half becomes the
                // invalid value it already is, and validation names it.
                (count, commitment) => (count.unwrap_or_default(), commitment.unwrap_or_default()),
            };
        Ok(Self {
            legacy_path_binding_id: entry.legacy_path_binding_id,
            historical_path: entry.historical_path,
            source_store: entry.source_store,
            source_row_id: entry.source_row_id,
            member_row_count,
            member_commitment_sha256,
            inventory_epoch: entry.inventory_epoch,
            status: entry.status,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeMigrationAttachmentProof {
    pub scope_migration_id: ScopeMigrationId,
    pub attachment_id: AttachmentId,
    pub checkout_id: String,
    pub old_scope: ProjectScope,
    pub new_scope: ProjectScope,
    pub proved_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSnapshotV1 {
    pub version: u32,
    pub epoch: u64,
    pub attachments: BTreeMap<AttachmentId, CheckoutAttachment>,
    pub scope_migration_proofs: BTreeMap<ScopeMigrationId, ScopeMigrationAttachmentProof>,
    pub legacy_path_bindings: BTreeMap<LegacyPathBindingId, LegacyPathLedgerEntry>,
    /// Operator-selected default local-source attachment per project
    /// (phase-2 §7.3): consulted by path operations when no session pin or
    /// explicit attachment selector is present. Host-local attachment data,
    /// never catalog data. Additive with a serde default so Phase 1
    /// snapshots decode unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_attachments: BTreeMap<ProjectId, AttachmentId>,
}

impl AttachmentSnapshotV1 {
    pub fn empty(epoch: u64) -> Result<Self, ProjectCatalogError> {
        let snapshot = Self {
            version: ATTACHMENT_VERSION_V1,
            epoch,
            attachments: BTreeMap::new(),
            scope_migration_proofs: BTreeMap::new(),
            legacy_path_bindings: BTreeMap::new(),
            default_attachments: BTreeMap::new(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), ProjectCatalogError> {
        validate_attachments(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct LegacyProjectRecordV1 {
    pub project_id: String,
    #[serde(default)]
    pub repo_id: Option<String>,
    pub canonical_path: String,
    pub registered_at: String,
    pub is_git_repo: bool,
    #[serde(default)]
    pub languages: BTreeSet<Language>,
    #[serde(default)]
    pub aliases: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
pub struct LegacyProjectStoreV1 {
    pub version: u32,
    pub projects: Vec<LegacyProjectRecordV1>,
}

impl Default for LegacyProjectStoreV1 {
    fn default() -> Self {
        Self {
            version: LEGACY_PROJECT_STORE_VERSION_V1,
            projects: Vec::new(),
        }
    }
}

impl LegacyProjectStoreV1 {
    pub fn validate(&self) -> Result<(), ProjectCatalogError> {
        if self.version != LEGACY_PROJECT_STORE_VERSION_V1 {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_unsupported_legacy_version",
                "legacy project store version is unsupported",
            ));
        }
        if self.projects.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit("legacy projects"));
        }
        Ok(())
    }
}

fn collection_limit(kind: &'static str) -> ProjectCatalogError {
    ProjectCatalogError::new(
        "error.project_catalog_collection_limit",
        format!("{kind} exceeds the collection limit"),
    )
}

fn validate_catalog(snapshot: &CatalogSnapshotV2) -> Result<(), ProjectCatalogError> {
    // The version is a function of content, checked in both directions: an
    // unknown version cannot be opened, and a version too LOW for what the
    // bytes carry is refused rather than laundered into a valid-looking
    // catalog. A v2-declared file holding connector scopes is exactly what a
    // hand-edit or a partial downgrade produces, and accepting it would mean
    // writing v2 bytes an older daemon then reads as a truncated catalog.
    if !(CATALOG_VERSION_V2..=CATALOG_VERSION_MAX).contains(&snapshot.version) {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_unsupported_version",
            "catalog version is unsupported",
        ));
    }
    if snapshot.version < snapshot.required_version() {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_unsupported_version",
            "catalog version is lower than its content requires",
        ));
    }
    if snapshot.epoch == 0 {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_invalid_epoch",
            "catalog epoch must be nonzero",
        ));
    }
    for (kind, len) in [
        ("projects", snapshot.projects.len()),
        ("repo histories", snapshot.repo_histories.len()),
        ("ambiguous namespaces", snapshot.ambiguous_namespaces.len()),
        ("scope migrations", snapshot.scope_migrations.len()),
    ] {
        if len > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit(kind));
        }
    }

    let mut published_scopes = BTreeSet::new();
    let mut connector_source_ids = BTreeSet::new();
    let mut accepted_aliases = BTreeMap::<&str, &ProjectId>::new();
    let project_ids = snapshot
        .projects
        .keys()
        .map(|id| id.as_str())
        .collect::<BTreeSet<_>>();
    for (key, project) in &snapshot.projects {
        if key != &project.project_id {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_key_mismatch",
                format!("project key disagrees with project {}", key),
            ));
        }
        project.scope.validate()?;
        validate_display(&project.display_name, "project display_name")?;
        validate_timestamp(&project.created_at, "project created_at")?;
        if let Some(timestamp) = &project.registered_at_compat {
            validate_timestamp(timestamp, "project registered_at_compat")?;
        }
        for (kind, len) in [
            ("project operator aliases", project.operator_aliases.len()),
            ("project nominated aliases", project.nominated_aliases.len()),
            ("project languages", project.languages.len()),
        ] {
            if len > MAX_PROJECT_CATALOG_ENTRIES {
                return Err(collection_limit(kind));
            }
        }
        if let ProjectScope::Published(scope) = &project.scope
            && !published_scopes.insert(scope)
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_duplicate_scope",
                format!("project {} has a duplicate published scope", key),
            ));
        }
        // Connector uniqueness keys on the minted id ALONE, never on the
        // (id, kind) pair: one operator-minted id names exactly one durable
        // project, so a mistyped kind can never mint a second project under
        // an id that is already owned.
        if let ProjectScope::Connector(scope) = &project.scope
            && !connector_source_ids.insert(scope.connector_source_id())
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_duplicate_scope",
                format!("project {} has a duplicate connector_source_id", key),
            ));
        }
        for alias in &project.operator_aliases {
            validate_alias(alias)?;
            if project_ids.contains(alias.as_str()) {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_alias_collision",
                    format!("project {} has an alias colliding with a project id", key),
                ));
            }
            if let Some(owner) = accepted_aliases.insert(alias, key) {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_alias_collision",
                    format!("projects {} and {} claim one accepted alias", owner, key),
                ));
            }
        }
        for alias in &project.nominated_aliases {
            validate_alias(alias)?;
        }
        if let Some(history_id) = &project.repo_history
            && !snapshot.repo_histories.contains_key(history_id)
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_dangling_history",
                format!("project {} references a missing repo history", key),
            ));
        }
    }

    // Observations are strictly subordinate to a connector-scoped project:
    // they name one, they never outlive one, and they never appear beside a
    // published or legacy-local project. That last rule is what keeps a
    // catalog with no connector scopes byte-identical to a pre-connector
    // write, which is the downgrade proof.
    if snapshot.connector_observations.len() > MAX_PROJECT_CATALOG_ENTRIES {
        return Err(collection_limit("connector observations"));
    }
    for (project_id, observations) in &snapshot.connector_observations {
        let Some(project) = snapshot.projects.get(project_id) else {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_dangling_connector_observation",
                format!("connector observations name missing project {project_id}"),
            ));
        };
        if !matches!(project.scope, ProjectScope::Connector(_)) {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_connector_observation",
                format!("project {project_id} is not connector-scoped"),
            ));
        }
        observations.validate()?;
    }

    let mut recorded_authorities = BTreeSet::new();
    let mut owned_namespaces = BTreeMap::<&CommitNamespace, &RepoHistoryId>::new();
    for (key, history) in &snapshot.repo_histories {
        if key != &history.repo_history_id {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_key_mismatch",
                format!("repo history key disagrees with history {}", key),
            ));
        }
        if let RepoHistoryAuthority::Recorded(authority) = &history.authority
            && !recorded_authorities.insert(authority)
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_duplicate_repo_authority",
                format!("repo history {} duplicates recorded authority", key),
            ));
        }
        if let Some(owner) = owned_namespaces.insert(&history.primary_namespace, key) {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_duplicate_namespace",
                format!(
                    "repo histories {} and {} share a commit namespace",
                    owner, key
                ),
            ));
        }
        for namespace in &history.compatibility_namespaces {
            if namespace == &history.primary_namespace {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_duplicate_namespace",
                    format!("repo history {} repeats its primary namespace", key),
                ));
            }
            if let Some(owner) = owned_namespaces.insert(namespace, key) {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_duplicate_namespace",
                    format!(
                        "repo histories {} and {} share a commit namespace",
                        owner, key
                    ),
                ));
            }
        }
        if history.compatibility_namespaces.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit("repo history compatibility namespaces"));
        }
        if let RepoHistoryAuthority::LocalProject(owner) = &history.authority {
            let owner_project = snapshot.projects.get(owner).ok_or_else(|| {
                ProjectCatalogError::new(
                    "error.project_catalog_dangling_history_authority",
                    format!("repo history {} has a missing local project owner", key),
                )
            })?;
            if owner_project.repo_history.as_ref() != Some(key)
                || snapshot.projects.values().any(|project| {
                    project.project_id != *owner && project.repo_history.as_ref() == Some(key)
                })
            {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_local_history_not_isolated",
                    format!("repo history {} is not isolated to its local project", key),
                ));
            }
            if owner_project.scope != ProjectScope::LegacyLocal {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_local_history_not_isolated",
                    format!("repo history {} local owner is not legacy-local", key),
                ));
            }
            if !is_local_commit_namespace(&history.primary_namespace) {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_local_history_namespace",
                    format!("repo history {} lacks a code-owned local namespace", key),
                ));
            }
        }
        if let RepoHistoryAuthority::LegacyNamespace(namespace) = &history.authority
            && namespace != &history.primary_namespace
            && !history.compatibility_namespaces.contains(namespace)
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_legacy_history_namespace",
                format!(
                    "repo history {} legacy authority is not one of its namespaces",
                    key
                ),
            ));
        }
        if let RepoHistoryMaterialization::Ready { generation_id } = &history.materialization
            && RepoHistoryGenerationId::parse(generation_id.as_str()).is_err()
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_history_generation",
                format!("repo history {} has an invalid generation id", key),
            ));
        }
    }

    for project in snapshot.projects.values() {
        let Some(history_id) = &project.repo_history else {
            continue;
        };
        let history = &snapshot.repo_histories[history_id];
        if let ProjectScope::Published(scope) = &project.scope {
            match &history.authority {
                RepoHistoryAuthority::Recorded(authority)
                    if authority.as_str() == scope.repo_id() => {}
                RepoHistoryAuthority::Recorded(_) => {
                    return Err(ProjectCatalogError::new(
                        "error.project_catalog_repo_authority_mismatch",
                        format!(
                            "project {} published scope disagrees with repo history authority",
                            project.project_id
                        ),
                    ));
                }
                RepoHistoryAuthority::LocalProject(_)
                | RepoHistoryAuthority::LegacyNamespace(_) => {
                    return Err(ProjectCatalogError::new(
                        "error.project_catalog_repo_authority_mismatch",
                        format!(
                            "project {} published scope lacks recorded repo authority",
                            project.project_id
                        ),
                    ));
                }
            }
        }
    }

    for (key, ambiguous) in &snapshot.ambiguous_namespaces {
        if key != &ambiguous.namespace {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_key_mismatch",
                format!("ambiguous namespace key disagrees with namespace {}", key),
            ));
        }
        if owned_namespaces.contains_key(key) {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_ambiguous_namespace_owned",
                format!("ambiguous namespace {} is also active", key),
            ));
        }
        if ambiguous.candidate_repo_history_ids.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit("ambiguous namespace candidates"));
        }
        if ambiguous.candidate_repo_history_ids.len() < 2
            || ambiguous
                .candidate_repo_history_ids
                .iter()
                .any(|id| !snapshot.repo_histories.contains_key(id))
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_ambiguity",
                format!("ambiguous namespace {} has invalid candidates", key),
            ));
        }
        if let RepoHistoryQuarantineMaterialization::Ready { generation_id } =
            &ambiguous.materialization
            && RepoHistoryQuarantineGenerationId::parse(generation_id.as_str()).is_err()
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_history_generation",
                format!("ambiguous namespace {} has an invalid generation id", key),
            ));
        }
    }

    validate_scope_migrations(snapshot)?;
    Ok(())
}

fn validate_scope_migrations(snapshot: &CatalogSnapshotV2) -> Result<(), ProjectCatalogError> {
    let mut by_project = BTreeMap::<&ProjectId, Vec<&ScopeMigrationRecord>>::new();
    for (key, migration) in &snapshot.scope_migrations {
        if key != &migration.scope_migration_id {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_key_mismatch",
                format!("scope migration key disagrees with migration {}", key),
            ));
        }
        if !snapshot.projects.contains_key(&migration.project_id) {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_dangling_migration",
                format!("scope migration {} references a missing project", key),
            ));
        }
        if migration.catalog_epoch == 0 || migration.catalog_epoch > snapshot.epoch {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_migration_epoch",
                format!("scope migration {} has an invalid catalog epoch", key),
            ));
        }
        migration.old_scope.validate()?;
        migration.new_scope.validate()?;
        if migration.old_scope == migration.new_scope {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_migration_shape",
                format!("scope migration {} has equal old and new scope", key),
            ));
        }
        let valid_shape = match (&migration.kind, &migration.old_scope, &migration.new_scope) {
            (
                ScopeMigrationKind::Promotion,
                ProjectScope::LegacyLocal,
                ProjectScope::Published(_),
            ) => {
                migration.authority_provenance
                    == ScopeMigrationAuthorityProvenance::AttachmentProved
            }
            (
                ScopeMigrationKind::RelpathMove,
                ProjectScope::Published(old),
                ProjectScope::Published(new),
            ) => {
                old.repo_id() == new.repo_id() && old.bbox_root_relpath() != new.bbox_root_relpath()
            }
            (
                ScopeMigrationKind::RepoAuthorityChange,
                ProjectScope::Published(old),
                ProjectScope::Published(new),
            ) => {
                old.repo_id() != new.repo_id() && old.bbox_root_relpath() == new.bbox_root_relpath()
            }
            _ => false,
        };
        if !valid_shape {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_migration_shape",
                format!("scope migration {} kind disagrees with its transition", key),
            ));
        }
        validate_bounded_text(
            &migration.operator_invocation,
            MAX_AUDIT_SOURCE_BYTES,
            "scope migration operator_invocation",
        )?;
        if let Some(reason) = &migration.operator_reason {
            validate_bounded_text(
                reason,
                MAX_AUDIT_REASON_BYTES,
                "scope migration operator_reason",
            )?;
        }
        if migration.authority_provenance == ScopeMigrationAuthorityProvenance::OperatorAttested
            && migration.operator_reason.is_none()
        {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_missing_operator_reason",
                format!("operator-attested scope migration {} lacks a reason", key),
            ));
        }
        validate_timestamp(&migration.migrated_at, "scope migration migrated_at")?;
        if let Some(generation) = &migration.code_bridge_generation {
            validate_bounded_text(generation, MAX_AUTHORITY_BYTES, "code bridge generation")?;
        }
        if let Some(generation) = &migration.publication_bridge_generation {
            validate_bounded_text(
                generation,
                MAX_AUTHORITY_BYTES,
                "publication bridge generation",
            )?;
        }
        if migration.pending_capabilities.len() > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit("scope migration pending capabilities"));
        }
        by_project
            .entry(&migration.project_id)
            .or_default()
            .push(migration);
    }

    for (project_id, mut migrations) in by_project {
        migrations.sort_by(|left, right| {
            (left.catalog_epoch, &left.scope_migration_id)
                .cmp(&(right.catalog_epoch, &right.scope_migration_id))
        });
        for pair in migrations.windows(2) {
            if pair[0].catalog_epoch == pair[1].catalog_epoch
                || pair[0].new_scope != pair[1].old_scope
            {
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_migration_chain",
                    format!(
                        "project {} has a branching or discontinuous migration chain",
                        project_id
                    ),
                ));
            }
        }
        let final_scope = &migrations
            .last()
            .expect("nonempty migration group")
            .new_scope;
        if &snapshot.projects[project_id].scope != final_scope {
            return Err(ProjectCatalogError::new(
                "error.project_catalog_migration_chain",
                format!(
                    "project {} current scope disagrees with its migration chain",
                    project_id
                ),
            ));
        }
    }
    Ok(())
}

fn validate_attachments(snapshot: &AttachmentSnapshotV1) -> Result<(), ProjectCatalogError> {
    if snapshot.version != ATTACHMENT_VERSION_V1 {
        return Err(ProjectCatalogError::new(
            "error.project_attachments_unsupported_version",
            "attachment snapshot version is unsupported",
        ));
    }
    for (project_id, attachment_id) in &snapshot.default_attachments {
        let Some(attachment) = snapshot.attachments.get(attachment_id) else {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_dangling_default",
                format!("default attachment {attachment_id} is not in the store"),
            ));
        };
        if &attachment.project_id != project_id {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_default_project_mismatch",
                format!("default attachment {attachment_id} belongs to another project"),
            ));
        }
        if attachment.status != AttachmentStatus::Attached {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_default_detached",
                format!("default attachment {attachment_id} is not active"),
            ));
        }
    }
    if snapshot.epoch == 0 {
        return Err(ProjectCatalogError::new(
            "error.project_attachments_invalid_epoch",
            "attachment epoch must be nonzero",
        ));
    }
    for (kind, len) in [
        ("attachments", snapshot.attachments.len()),
        (
            "scope migration proofs",
            snapshot.scope_migration_proofs.len(),
        ),
        ("legacy path bindings", snapshot.legacy_path_bindings.len()),
    ] {
        if len > MAX_PROJECT_CATALOG_ENTRIES {
            return Err(collection_limit(kind));
        }
    }

    let mut active_keys = BTreeSet::new();
    let mut active_checkout_scopes = BTreeSet::new();
    for (key, attachment) in &snapshot.attachments {
        if key != &attachment.attachment_id {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_key_mismatch",
                format!("attachment key disagrees with attachment {}", key),
            ));
        }
        validate_checkout_id(&attachment.checkout_id)?;
        validate_normalized_absolute_path(
            &attachment.checkout_dir,
            &format!("attachment {} checkout_dir", key),
        )?;
        validate_normalized_absolute_path(
            &attachment.checkout_project_dir,
            &format!("attachment {} checkout_project_dir", key),
        )?;
        validate_project_relpath(&attachment.project_root_relpath)?;
        if !projected_path_matches(attachment) {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_projection_mismatch",
                format!("attachment {} project path disagrees with its relpath", key),
            ));
        }
        if let Some(scope) = &attachment.validated_scope {
            scope.validate().map_err(|error| {
                ProjectCatalogError::new(
                    "error.project_attachments_invalid_scope",
                    format!("attachment {} has invalid scope {}", key, error.field()),
                )
            })?;
        }
        if let Some(branch_ref) = &attachment.branch_ref {
            validate_bounded_text(branch_ref, 1024, "attachment branch_ref")?;
        }
        validate_timestamp(&attachment.attached_at, "attachment attached_at")?;
        if let Some(detached_at) = &attachment.detached_at {
            validate_timestamp(detached_at, "attachment detached_at")?;
        }
        match attachment.status {
            AttachmentStatus::Attached => {
                let active_key = (
                    &attachment.project_id,
                    attachment.checkout_id.as_str(),
                    attachment.project_root_relpath.as_str(),
                );
                if !active_keys.insert(active_key) {
                    return Err(ProjectCatalogError::new(
                        "error.project_attachments_duplicate_active",
                        format!("attachment {} duplicates an active checkout binding", key),
                    ));
                }
                let checkout_scope = (
                    attachment.checkout_id.as_str(),
                    attachment.project_root_relpath.as_str(),
                );
                if !active_checkout_scopes.insert(checkout_scope) {
                    return Err(ProjectCatalogError::new(
                        "error.project_attachments_duplicate_active",
                        format!(
                            "attachment {} reuses an active checkout scope across projects",
                            key
                        ),
                    ));
                }
                if attachment.detached_at.is_some() {
                    return Err(ProjectCatalogError::new(
                        "error.project_attachments_invalid_status",
                        format!("attached attachment {} has detached_at", key),
                    ));
                }
            }
            AttachmentStatus::Detached => {
                if attachment.capabilities.any() || attachment.detached_at.is_none() {
                    return Err(ProjectCatalogError::new(
                        "error.project_attachments_detached_capability",
                        format!("detached attachment {} claims active state", key),
                    ));
                }
            }
        }
    }

    for (key, proof) in &snapshot.scope_migration_proofs {
        if key != &proof.scope_migration_id {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_key_mismatch",
                format!("migration proof key disagrees with proof {}", key),
            ));
        }
        validate_checkout_id(&proof.checkout_id)?;
        proof.old_scope.validate()?;
        proof.new_scope.validate()?;
        if proof.old_scope == proof.new_scope {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_invalid_proof",
                format!("migration proof {} has equal old and new scope", key),
            ));
        }
        validate_timestamp(&proof.proved_at, "migration proof proved_at")?;
    }

    let mut legacy_source_rows = BTreeSet::new();
    for (key, binding) in &snapshot.legacy_path_bindings {
        if key != &binding.legacy_path_binding_id {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_key_mismatch",
                format!("legacy path key disagrees with binding {}", key),
            ));
        }
        // Only a MAPPED binding's historical path is consumed as a path:
        // dual-read resolves through it, so it must be normalized absolute.
        // Unscoped and quarantined bindings carry whatever literal the v1
        // store actually held, including bare resolver NAMES from early note
        // rows; demanding path shape there would refuse the migration for
        // rows that deliberately keep no path binding at all. The literal
        // stays bounded and control-free, and the supersession key still
        // works over it byte-for-byte.
        match &binding.status {
            LegacyPathBindingStatus::Mapped { .. } => {
                validate_normalized_absolute_path(
                    &binding.historical_path,
                    &format!("legacy path binding {} historical_path", key),
                )?;
            }
            LegacyPathBindingStatus::Unscoped {} | LegacyPathBindingStatus::Quarantined {} => {
                validate_bounded_text(
                    &binding.historical_path,
                    MAX_PATH_BYTES,
                    "legacy path binding historical_path literal",
                )?;
            }
        }
        validate_bounded_text(&binding.source_store, 128, "legacy path source_store")?;
        validate_bounded_text(&binding.source_row_id, 256, "legacy path source_row_id")?;
        // A binding standing for no rows is not evidence of anything, and a
        // commitment that is not a sha256 could not have come from a capture.
        if binding.member_row_count == 0
            || binding.member_commitment_sha256.len() != 64
            || !binding
                .member_commitment_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            // Two different failures wear the same shape here, and they need
            // opposite remediations, so they get different codes. A
            // reconstructable source can only reach this by carrying a broken
            // pair, which is a defect in the record. An unreconstructable one
            // reaches it by predating the evidence entirely, and NOTHING short
            // of the owner's own walk can supply what it is missing.
            if !legacy_ledger_evidence_is_reconstructable(&binding.source_store) {
                // The detail is capped by `bounded_detail`; the remedy
                // sentence leads so no truncation can eat it.
                return Err(ProjectCatalogError::new(
                    "error.project_catalog_legacy_evidence_unreconstructable",
                    format!(
                        "re-run the project-catalog migration from the v1 predecessor after \
                         discarding this migrated pair: binding {key} names group source {} \
                         with no member evidence, which only the owner's own walk can supply; \
                         inventing it would refuse every later stamp forever.",
                        binding.source_store
                    ),
                ));
            }
            return Err(ProjectCatalogError::new(
                "error.project_catalog_invalid_field",
                format!(
                    "legacy path binding {key} member evidence is invalid: an entry written \
                     before this evidence existed must omit BOTH fields, which decode fills \
                     with singleton evidence; a partial or zeroed pair cannot be reconstructed \
                     and must be repaired at the source"
                ),
            ));
        }
        if binding.inventory_epoch == 0 {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_invalid_inventory_epoch",
                format!("legacy path binding {} has a zero inventory epoch", key),
            ));
        }
        if !legacy_source_rows.insert((
            binding.inventory_epoch,
            binding.source_store.as_str(),
            binding.source_row_id.as_str(),
        )) {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_duplicate_legacy_source",
                format!(
                    "legacy path binding {} duplicates an inventory source row",
                    key
                ),
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct ValidatedCatalogAttachments<'a> {
    catalog: &'a CatalogSnapshotV2,
    attachments: &'a AttachmentSnapshotV1,
}

#[derive(Debug, Clone, Copy)]
pub struct ValidatedCheckoutAttachment<'a> {
    project: &'a CorpusProject,
    repo_history: Option<&'a RepoHistoryRecord>,
    attachment: &'a CheckoutAttachment,
}

impl<'a> ValidatedCheckoutAttachment<'a> {
    pub fn project(&self) -> &'a CorpusProject {
        self.project
    }

    pub fn repo_history(&self) -> Option<&'a RepoHistoryRecord> {
        self.repo_history
    }

    pub fn attachment(&self) -> &'a CheckoutAttachment {
        self.attachment
    }
}

impl<'a> ValidatedCatalogAttachments<'a> {
    pub fn catalog(&self) -> &'a CatalogSnapshotV2 {
        self.catalog
    }

    pub fn attachments(&self) -> &'a AttachmentSnapshotV1 {
        self.attachments
    }

    pub fn attachment(&self, id: &AttachmentId) -> Option<ValidatedCheckoutAttachment<'a>> {
        self.attachments.attachments.get(id).and_then(|attachment| {
            let project = self.catalog.projects.get(&attachment.project_id)?;
            let repo_history = project
                .repo_history
                .as_ref()
                .and_then(|history_id| self.catalog.repo_histories.get(history_id));
            Some(ValidatedCheckoutAttachment {
                project,
                repo_history,
                attachment,
            })
        })
    }
}

pub fn validate_catalog_attachments<'a>(
    catalog: &'a CatalogSnapshotV2,
    attachments: &'a AttachmentSnapshotV1,
) -> Result<ValidatedCatalogAttachments<'a>, ProjectCatalogError> {
    catalog.validate()?;
    attachments.validate()?;
    if catalog.epoch != attachments.epoch {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_epoch_mismatch",
            "catalog and attachment epochs disagree",
        ));
    }

    for attachment in attachments.attachments.values() {
        let project = catalog
            .projects
            .get(&attachment.project_id)
            .ok_or_else(|| {
                ProjectCatalogError::new(
                    "error.project_attachments_dangling_project",
                    format!(
                        "attachment {} references a missing project",
                        attachment.attachment_id
                    ),
                )
            })?;
        if attachment.status == AttachmentStatus::Attached {
            let scope_matches = match (&project.scope, &attachment.validated_scope) {
                (ProjectScope::Published(project_scope), Some(attachment_scope)) => {
                    project_scope == attachment_scope
                        && project_scope.bbox_root_relpath() == attachment.project_root_relpath
                }
                (ProjectScope::LegacyLocal, None) => true,
                _ => false,
            };
            if !scope_matches {
                return Err(ProjectCatalogError::new(
                    "error.project_attachments_scope_mismatch",
                    format!(
                        "attachment {} scope disagrees with project {}",
                        attachment.attachment_id, project.project_id
                    ),
                ));
            }
        }
    }

    for binding in attachments.legacy_path_bindings.values() {
        if let LegacyPathBindingStatus::Mapped { project_id, .. } = &binding.status
            && !catalog.projects.contains_key(project_id)
        {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_dangling_legacy_binding",
                format!(
                    "legacy path binding {} references a missing project",
                    binding.legacy_path_binding_id
                ),
            ));
        }
    }

    for migration in catalog.scope_migrations.values() {
        let proof = attachments
            .scope_migration_proofs
            .get(&migration.scope_migration_id);
        match migration.authority_provenance {
            ScopeMigrationAuthorityProvenance::AttachmentProved => {
                let proof = proof.ok_or_else(|| {
                    ProjectCatalogError::new(
                        "error.project_attachments_missing_migration_proof",
                        format!(
                            "scope migration {} lacks its attachment proof",
                            migration.scope_migration_id
                        ),
                    )
                })?;
                let attachment = attachments
                    .attachments
                    .get(&proof.attachment_id)
                    .ok_or_else(|| {
                        ProjectCatalogError::new(
                            "error.project_attachments_dangling_migration_proof",
                            format!(
                                "scope migration proof {} references a missing attachment",
                                proof.scope_migration_id
                            ),
                        )
                    })?;
                if attachment.project_id != migration.project_id
                    || attachment.checkout_id != proof.checkout_id
                    || proof.old_scope != migration.old_scope
                    || proof.new_scope != migration.new_scope
                {
                    return Err(ProjectCatalogError::new(
                        "error.project_attachments_migration_proof_mismatch",
                        format!(
                            "scope migration proof {} disagrees with catalog or attachment state",
                            proof.scope_migration_id
                        ),
                    ));
                }
            }
            ScopeMigrationAuthorityProvenance::OperatorAttested if proof.is_some() => {
                return Err(ProjectCatalogError::new(
                    "error.project_attachments_unexpected_migration_proof",
                    format!(
                        "operator-attested scope migration {} has an attachment proof",
                        migration.scope_migration_id
                    ),
                ));
            }
            ScopeMigrationAuthorityProvenance::OperatorAttested => {}
        }
    }
    for proof_id in attachments.scope_migration_proofs.keys() {
        let Some(migration) = catalog.scope_migrations.get(proof_id) else {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_dangling_migration_proof",
                format!("migration proof {} has no catalog record", proof_id),
            ));
        };
        if migration.authority_provenance != ScopeMigrationAuthorityProvenance::AttachmentProved {
            return Err(ProjectCatalogError::new(
                "error.project_attachments_unexpected_migration_proof",
                format!("migration proof {} has incompatible provenance", proof_id),
            ));
        }
    }

    Ok(ValidatedCatalogAttachments {
        catalog,
        attachments,
    })
}

fn validate_alias(alias: &str) -> Result<(), ProjectCatalogError> {
    if alias.is_empty()
        || alias.len() > MAX_PROJECT_ID_BYTES
        || alias.trim() != alias
        || matches!(alias, "." | "..")
        || alias.contains(['/', '\\', '%'])
        || alias.chars().any(char::is_whitespace)
        || alias.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_invalid_alias",
            "project alias is invalid",
        ));
    }
    Ok(())
}

fn validate_display(value: &str, kind: &'static str) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > MAX_DISPLAY_NAME_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_invalid_display",
            format!("{kind} is invalid"),
        ));
    }
    Ok(())
}

fn validate_timestamp(value: &str, kind: &'static str) -> Result<(), ProjectCatalogError> {
    validate_bounded_text(value, MAX_TIMESTAMP_BYTES, kind)
}

fn validate_bounded_text(
    value: &str,
    max_bytes: usize,
    kind: &'static str,
) -> Result<(), ProjectCatalogError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_invalid_field",
            format!("{kind} is invalid"),
        ));
    }
    Ok(())
}

fn validate_checkout_id(value: &str) -> Result<(), ProjectCatalogError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ProjectCatalogError::new(
            "error.project_attachments_invalid_checkout_id",
            "checkout_id is not a strong-random marker id",
        ));
    }
    Ok(())
}

fn validate_normalized_absolute_path(value: &str, kind: &str) -> Result<(), ProjectCatalogError> {
    let valid = value.starts_with('/')
        && value.len() <= MAX_PATH_BYTES
        && !value.contains('\\')
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        && (value == "/"
            || value.strip_prefix('/').is_some_and(|tail| {
                tail.split('/').all(|component| {
                    !component.is_empty()
                        && !matches!(component, "." | "..")
                        && component.len() <= 255
                })
            }));
    if !valid {
        return Err(ProjectCatalogError::new(
            "error.project_attachments_invalid_path",
            format!("{kind} is not a normalized absolute path"),
        ));
    }
    Ok(())
}

fn validate_project_relpath(value: &str) -> Result<(), ProjectCatalogError> {
    let valid = value == "."
        || (!value.is_empty()
            && value.len() <= MAX_PATH_BYTES
            && !value.starts_with('/')
            && !value.contains('\\')
            && value.split('/').all(|component| {
                !component.is_empty()
                    && !matches!(component, "." | "..")
                    && component.len() <= 255
                    && !component.bytes().any(|byte| byte.is_ascii_control())
            }));
    if !valid {
        return Err(ProjectCatalogError::new(
            "error.project_attachments_invalid_relpath",
            "project_root_relpath is invalid",
        ));
    }
    Ok(())
}

fn projected_path_matches(attachment: &CheckoutAttachment) -> bool {
    let checkout = Path::new(&attachment.checkout_dir);
    let project = Path::new(&attachment.checkout_project_dir);
    let expected = if attachment.project_root_relpath == "." {
        checkout.to_path_buf()
    } else {
        checkout.join(&attachment.project_root_relpath)
    };
    project == expected
}

pub fn decode_catalog_snapshot(raw: &[u8]) -> Result<CatalogSnapshotV2, ProjectCatalogError> {
    let snapshot: CatalogSnapshotV2 = decode_strict(raw)?;
    snapshot.validate()?;
    Ok(snapshot)
}

pub fn decode_attachment_snapshot(raw: &[u8]) -> Result<AttachmentSnapshotV1, ProjectCatalogError> {
    let snapshot: AttachmentSnapshotV1 = decode_strict(raw)?;
    snapshot.validate()?;
    Ok(snapshot)
}

pub fn decode_legacy_project_store(
    raw: &[u8],
) -> Result<LegacyProjectStoreV1, ProjectCatalogError> {
    let store: LegacyProjectStoreV1 = decode_strict_bounded(raw, MAX_LEGACY_PROJECT_STORE_BYTES)?;
    store.validate()?;
    Ok(store)
}

pub fn encode_catalog_snapshot(
    snapshot: &CatalogSnapshotV2,
) -> Result<Vec<u8>, ProjectCatalogError> {
    snapshot.validate()?;
    encode_stable(snapshot)
}

pub fn encode_attachment_snapshot(
    snapshot: &AttachmentSnapshotV1,
) -> Result<Vec<u8>, ProjectCatalogError> {
    snapshot.validate()?;
    encode_stable(snapshot)
}

fn encode_stable<T: Serialize>(value: &T) -> Result<Vec<u8>, ProjectCatalogError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| {
        ProjectCatalogError::new(
            "error.project_catalog_encode",
            "snapshot serialization failed",
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > MAX_PROJECT_CATALOG_BYTES {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_byte_limit",
            "encoded snapshot exceeds the byte limit",
        ));
    }
    Ok(bytes)
}

fn decode_strict<T>(raw: &[u8]) -> Result<T, ProjectCatalogError>
where
    T: for<'de> Deserialize<'de>,
{
    decode_strict_bounded(raw, MAX_PROJECT_CATALOG_BYTES)
}

fn decode_strict_bounded<T>(raw: &[u8], max_bytes: usize) -> Result<T, ProjectCatalogError>
where
    T: for<'de> Deserialize<'de>,
{
    if raw.len() > max_bytes {
        return Err(ProjectCatalogError::new(
            "error.project_catalog_byte_limit",
            "snapshot exceeds the byte limit",
        ));
    }
    let value = serde_json::from_slice::<StrictJsonValue>(raw).map_err(|error| {
        if error.to_string().contains("duplicate object key") {
            ProjectCatalogError::new(
                "error.project_catalog_duplicate_json_key",
                "snapshot JSON contains a duplicate object key",
            )
        } else {
            ProjectCatalogError::new(
                "error.project_catalog_invalid_json",
                "snapshot JSON is invalid",
            )
        }
    })?;
    serde_json::from_value(value.0).map_err(|_| {
        ProjectCatalogError::new(
            "error.project_catalog_invalid_schema",
            "snapshot JSON does not match the strict schema",
        )
    })
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(StrictJsonValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJsonValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictJsonValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
            values.push(value.0);
        }
        Ok(StrictJsonValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = object.next_value::<StrictJsonValue>()?;
            values.insert(key, value.0);
        }
        Ok(StrictJsonValue(Value::Object(values)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> ProjectId {
        ProjectId::parse(value).unwrap()
    }

    fn scope(repo: &str, relpath: &str) -> PublishedScope {
        PublishedScope::try_new(repo, relpath).unwrap()
    }

    fn project(project_id: &str, scope: ProjectScope) -> CorpusProject {
        CorpusProject {
            project_id: id(project_id),
            scope,
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: "Example project".into(),
            created_at: "2026-07-22T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: BTreeSet::new(),
        }
    }

    fn catalog_with(project: CorpusProject) -> CatalogSnapshotV2 {
        let mut snapshot = CatalogSnapshotV2 {
            version: CATALOG_VERSION_V2,
            epoch: 1,
            origin: CatalogOriginV2::FreshV2 {},
            projects: BTreeMap::from([(project.project_id.clone(), project)]),
            repo_histories: BTreeMap::new(),
            ambiguous_namespaces: BTreeMap::new(),
            scope_migrations: BTreeMap::new(),
            connector_observations: BTreeMap::new(),
        };
        // Fixtures derive the version exactly as the store does, so a
        // connector fixture is a v3 catalog without every caller saying so.
        snapshot.sync_version();
        snapshot
    }

    fn connector_scope(id: &str) -> ConnectorScope {
        ConnectorScope::try_new(id, "gdrive").unwrap()
    }

    fn attachment(
        project_id: &ProjectId,
        validated_scope: Option<PublishedScope>,
    ) -> CheckoutAttachment {
        CheckoutAttachment {
            attachment_id: AttachmentId::parse("att_11111111111111111111111111111111").unwrap(),
            project_id: project_id.clone(),
            checkout_id: "22222222222222222222222222222222".into(),
            checkout_dir: "/tmp/example".into(),
            checkout_project_dir: "/tmp/example".into(),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Base,
            validated_scope,
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities {
                local_code_source: true,
                ..AttachmentCapabilities::default()
            },
            status: AttachmentStatus::Attached,
            attached_at: "2026-07-22T00:00:00Z".into(),
            detached_at: None,
        }
    }

    fn promoted_fixture() -> (
        CatalogSnapshotV2,
        AttachmentSnapshotV1,
        ScopeMigrationId,
        AttachmentId,
    ) {
        let migration_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let published_scope = scope("repo-1", ".");
        let project = project("one", ProjectScope::Published(published_scope.clone()));
        let migration = ScopeMigrationRecord {
            scope_migration_id: migration_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "bbox_project_promote".into(),
            operator_reason: None,
            old_scope: ProjectScope::LegacyLocal,
            new_scope: ProjectScope::Published(published_scope.clone()),
            kind: ScopeMigrationKind::Promotion,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(2).unwrap();
        catalog
            .projects
            .insert(project.project_id.clone(), project.clone());
        catalog
            .scope_migrations
            .insert(migration_id.clone(), migration);

        let row = attachment(&project.project_id, Some(published_scope.clone()));
        let attachment_id = row.attachment_id.clone();
        let proof = ScopeMigrationAttachmentProof {
            scope_migration_id: migration_id.clone(),
            attachment_id: attachment_id.clone(),
            checkout_id: row.checkout_id.clone(),
            old_scope: ProjectScope::LegacyLocal,
            new_scope: ProjectScope::Published(published_scope),
            proved_at: "2026-07-22T00:00:00Z".into(),
        };
        let mut attachments = AttachmentSnapshotV1::empty(2).unwrap();
        attachments.attachments.insert(attachment_id.clone(), row);
        attachments
            .scope_migration_proofs
            .insert(migration_id.clone(), proof);
        (catalog, attachments, migration_id, attachment_id)
    }

    #[test]
    fn project_id_accepts_legacy_and_new_contract() {
        for accepted in [
            "a",
            "deadbeef",
            "p_0123456789abcdef0123456789abcdef",
            "a.b-c_d",
        ] {
            assert!(ProjectId::parse(accepted).is_ok(), "{accepted}");
        }
        for rejected in ["", ".", "..", "a/b", "a\\b", "a:b", "a b", "a%2fb", "\na"] {
            assert!(ProjectId::parse(rejected).is_err(), "{rejected:?}");
        }
        assert!(ProjectId::parse("a".repeat(MAX_PROJECT_ID_BYTES)).is_ok());
        assert!(ProjectId::parse("a".repeat(MAX_PROJECT_ID_BYTES + 1)).is_err());
    }

    #[test]
    fn bounded_mint_fails_after_repeated_catalog_collisions() {
        let occupied = id("p_11111111111111111111111111111111");
        let error = ProjectId::mint_with(
            || occupied.as_str().to_string(),
            |candidate| candidate == &occupied,
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_id_collision");
    }

    #[test]
    fn newtype_deserialization_cannot_bypass_validation() {
        assert!(serde_json::from_str::<ProjectId>(r#""../escape""#).is_err());
        assert!(
            serde_json::from_str::<AttachmentId>(r#""p_11111111111111111111111111111111""#)
                .is_err()
        );
        assert!(serde_json::from_str::<CommitNamespace>(r#""repo:namespace""#).is_err());
    }

    #[test]
    fn published_scope_validation_is_strict_and_portable() {
        assert!(PublishedScope::try_new("repo-1", ".").is_ok());
        assert!(PublishedScope::try_new("repo-1", "services/api").is_ok());
        assert!(PublishedScope::try_new("repo 1", ".").is_err());
        assert!(PublishedScope::try_new("repo-1", "../api").is_err());
        assert!(PublishedScope::try_new("repo-1", r"services\api").is_err());
        assert!(PublishedScope::try_new("repo-1", "C:/api").is_err());
        assert!(
            serde_json::from_value::<PublishedScope>(
                serde_json::json!({"repo_id":"repo 1","bbox_root_relpath":"."})
            )
            .is_err()
        );
    }

    #[test]
    fn strict_codec_rejects_unknown_and_duplicate_keys() {
        let raw = br#"{
            "version": 2,
            "version": 2,
            "epoch": 1,
            "origin": {"kind":"fresh_v2"},
            "projects": {},
            "repo_histories": {},
            "ambiguous_namespaces": {},
            "scope_migrations": {}
        }"#;
        assert_eq!(
            decode_catalog_snapshot(raw).unwrap_err().code(),
            "error.project_catalog_duplicate_json_key"
        );

        let raw = br#"{
            "version": 2,
            "epoch": 1,
            "origin": {"kind":"fresh_v2"},
            "projects": {},
            "repo_histories": {},
            "ambiguous_namespaces": {},
            "scope_migrations": {},
            "unexpected": true
        }"#;
        assert_eq!(
            decode_catalog_snapshot(raw).unwrap_err().code(),
            "error.project_catalog_invalid_schema"
        );

        let raw = br#"{
            "version": 2,
            "epoch": 1,
            "origin": {"kind":"fresh_v2", "unexpected":true},
            "projects": {},
            "repo_histories": {},
            "ambiguous_namespaces": {},
            "scope_migrations": {}
        }"#;
        assert_eq!(
            decode_catalog_snapshot(raw).unwrap_err().code(),
            "error.project_catalog_invalid_schema"
        );
    }

    #[test]
    fn stable_codec_orders_maps_and_appends_newline() {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        for project_id in ["z", "a"] {
            let project = project(project_id, ProjectScope::LegacyLocal);
            catalog.projects.insert(project.project_id.clone(), project);
        }
        let first = encode_catalog_snapshot(&catalog).unwrap();
        let second = encode_catalog_snapshot(&decode_catalog_snapshot(&first).unwrap()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.last(), Some(&b'\n'));
        let text = String::from_utf8(first).unwrap();
        assert!(text.find(r#""a""#).unwrap() < text.find(r#""z""#).unwrap());
        assert!(!text.contains("canonical_path"));
        assert!(!text.contains("checkout_dir"));
        assert!(!text.contains("attachment_id"));
    }

    #[test]
    fn stable_encoder_refuses_bytes_its_decoder_cannot_accept() {
        let oversized = "x".repeat(MAX_PROJECT_CATALOG_BYTES);
        assert_eq!(
            encode_stable(&oversized).unwrap_err().code(),
            "error.project_catalog_byte_limit"
        );
    }

    #[test]
    fn strict_codec_rejects_nested_scope_fields_and_duplicate_set_values() {
        let mut project = project("one", ProjectScope::Published(scope("repo-1", ".")));
        project.operator_aliases.insert("one-alias".into());
        let catalog = catalog_with(project);
        let encoded = encode_catalog_snapshot(&catalog).unwrap();
        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["projects"]["one"]["scope"]["scope"]["unexpected"] = Value::Bool(true);
        assert_eq!(
            decode_catalog_snapshot(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .code(),
            "error.project_catalog_invalid_schema"
        );

        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["projects"]["one"]["operator_aliases"] =
            serde_json::json!(["one-alias", "one-alias"]);
        assert_eq!(
            decode_catalog_snapshot(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .code(),
            "error.project_catalog_invalid_schema"
        );
    }

    #[test]
    fn strict_variant_envelopes_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<ProjectScope>(
                serde_json::json!({"kind":"legacy_local","unexpected":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProjectScope>(serde_json::json!({
                "kind":"published",
                "scope":{"repo_authority":"repo-1","project_root_relpath":"."},
                "unexpected":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<RepoHistoryAuthority>(serde_json::json!({
                "kind":"recorded",
                "authority":"repo-1",
                "unexpected":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<LegacyPathBindingStatus>(
                serde_json::json!({"kind":"unscoped","unexpected":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<LegacyPathBindingStatus>(serde_json::json!({
                "kind":"mapped",
                "project_id":"project-one",
                "relationship":"root",
                "unexpected":true
            }))
            .is_err()
        );
    }

    #[test]
    fn catalog_rejects_duplicate_published_scope() {
        let shared_scope = scope("repo-1", ".");
        let first = project("one", ProjectScope::Published(shared_scope.clone()));
        let second = project("two", ProjectScope::Published(shared_scope));
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(first.project_id.clone(), first);
        catalog.projects.insert(second.project_id.clone(), second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_duplicate_scope"
        );
    }

    #[test]
    fn catalog_rejects_duplicate_accepted_alias() {
        let mut first = project("one", ProjectScope::Published(scope("repo-1", ".")));
        first.operator_aliases.insert("shared".into());
        let mut second = project("two", ProjectScope::Published(scope("repo-2", ".")));
        second.operator_aliases.insert("shared".into());
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(first.project_id.clone(), first);
        catalog.projects.insert(second.project_id.clone(), second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_alias_collision"
        );
    }

    #[test]
    fn catalog_rejects_accepted_alias_colliding_with_project_id() {
        let mut first = project("one", ProjectScope::Published(scope("repo-1", ".")));
        first.operator_aliases.insert("two".into());
        let second = project("two", ProjectScope::Published(scope("repo-2", ".")));
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(first.project_id.clone(), first);
        catalog.projects.insert(second.project_id.clone(), second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_alias_collision"
        );
    }

    #[test]
    fn catalog_and_attachment_maps_reject_embedded_id_mismatches() {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(
            id("wrong-project-key"),
            project("one", ProjectScope::LegacyLocal),
        );
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_key_mismatch"
        );

        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let wrong_history_key =
            RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let history = RepoHistoryRecord {
            repo_history_id: history_id,
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(wrong_history_key, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_key_mismatch"
        );

        let ambiguity_key = CommitNamespace::parse("wrong-namespace").unwrap();
        let ambiguity = AmbiguousNamespaceRecord {
            namespace: CommitNamespace::parse("ambiguous-namespace").unwrap(),
            candidate_repo_history_ids: BTreeSet::new(),
            status: AmbiguousNamespaceStatus::Quarantined,
            materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .ambiguous_namespaces
            .insert(ambiguity_key, ambiguity);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_key_mismatch"
        );

        let (mut catalog, _, migration_id, _) = promoted_fixture();
        let migration = catalog.scope_migrations.remove(&migration_id).unwrap();
        catalog.scope_migrations.insert(
            ScopeMigrationId::parse("sm_33333333333333333333333333333333").unwrap(),
            migration,
        );
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_key_mismatch"
        );

        let row = attachment(&id("one"), None);
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot.attachments.insert(
            AttachmentId::parse("att_33333333333333333333333333333333").unwrap(),
            row,
        );
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_key_mismatch"
        );

        let (_, mut snapshot, migration_id, _) = promoted_fixture();
        let proof = snapshot
            .scope_migration_proofs
            .remove(&migration_id)
            .unwrap();
        snapshot.scope_migration_proofs.insert(
            ScopeMigrationId::parse("sm_33333333333333333333333333333333").unwrap(),
            proof,
        );
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_key_mismatch"
        );

        let binding_id =
            LegacyPathBindingId::parse("lpb_11111111111111111111111111111111").unwrap();
        let binding = LegacyPathLedgerEntry {
            legacy_path_binding_id: binding_id,
            historical_path: "/tmp/old".into(),
            source_store: "knowledge".into(),
            source_row_id: "row-1".into(),
            member_row_count: 1,
            member_commitment_sha256: "a".repeat(64),
            inventory_epoch: 1,
            status: LegacyPathBindingStatus::Unscoped {},
        };
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot.legacy_path_bindings.insert(
            LegacyPathBindingId::parse("lpb_22222222222222222222222222222222").unwrap(),
            binding,
        );
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_key_mismatch"
        );
    }

    #[test]
    fn catalog_rejects_dangling_history() {
        let mut project = project("one", ProjectScope::LegacyLocal);
        project.repo_history =
            Some(RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap());
        let catalog = catalog_with(project);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_dangling_history"
        );

        let history_id = RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LocalProject(id("missing")),
            primary_namespace: CommitNamespace::parse("local_33333333333333333333333333333333")
                .unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_dangling_history_authority"
        );
    }

    #[test]
    fn catalog_rejects_duplicate_recorded_authority() {
        let first_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let second_id = RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let authority = RecordedRepoAuthority::parse("repo-1").unwrap();
        let first = RepoHistoryRecord {
            repo_history_id: first_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(authority.clone()),
            primary_namespace: CommitNamespace::parse("namespace-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let second = RepoHistoryRecord {
            repo_history_id: second_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(authority),
            primary_namespace: CommitNamespace::parse("namespace-two").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(first_id, first);
        catalog.repo_histories.insert(second_id, second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_duplicate_repo_authority"
        );
    }

    #[test]
    fn catalog_rejects_primary_compatibility_namespace_collisions() {
        let first_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let second_id = RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let shared = CommitNamespace::parse("shared-namespace").unwrap();
        let first = RepoHistoryRecord {
            repo_history_id: first_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: shared.clone(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let second = RepoHistoryRecord {
            repo_history_id: second_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-2").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-two").unwrap(),
            compatibility_namespaces: BTreeSet::from([shared]),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(first_id, first);
        catalog.repo_histories.insert(second_id, second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_duplicate_namespace"
        );

        let history_id = RepoHistoryId::parse("rh_33333333333333333333333333333333").unwrap();
        let namespace = CommitNamespace::parse("repeated-namespace").unwrap();
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-3").unwrap(),
            ),
            primary_namespace: namespace.clone(),
            compatibility_namespaces: BTreeSet::from([namespace]),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_duplicate_namespace"
        );
    }

    #[test]
    fn catalog_rejects_dangling_ambiguity_candidate() {
        let present_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let missing_id = RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let history = RepoHistoryRecord {
            repo_history_id: present_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let namespace = CommitNamespace::parse("ambiguous-namespace").unwrap();
        let ambiguous = AmbiguousNamespaceRecord {
            namespace: namespace.clone(),
            candidate_repo_history_ids: BTreeSet::from([present_id.clone(), missing_id]),
            status: AmbiguousNamespaceStatus::Quarantined,
            materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(present_id, history);
        catalog.ambiguous_namespaces.insert(namespace, ambiguous);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_ambiguity"
        );
    }

    #[test]
    fn ambiguous_namespace_cannot_also_be_active() {
        let first_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let second_id = RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap();
        let namespace = CommitNamespace::parse("shared-namespace").unwrap();
        let first = RepoHistoryRecord {
            repo_history_id: first_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: namespace.clone(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let second = RepoHistoryRecord {
            repo_history_id: second_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-2").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-two").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let ambiguous = AmbiguousNamespaceRecord {
            namespace: namespace.clone(),
            candidate_repo_history_ids: BTreeSet::from([first_id.clone(), second_id.clone()]),
            status: AmbiguousNamespaceStatus::Quarantined,
            materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(first_id, first);
        catalog.repo_histories.insert(second_id, second);
        catalog.ambiguous_namespaces.insert(namespace, ambiguous);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_ambiguous_namespace_owned"
        );
    }

    #[test]
    fn local_history_authority_is_isolated_to_its_project() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut owner = project("one", ProjectScope::LegacyLocal);
        owner.repo_history = Some(history_id.clone());
        let mut sibling = project("two", ProjectScope::LegacyLocal);
        sibling.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LocalProject(owner.project_id.clone()),
            primary_namespace: CommitNamespace::parse("local_33333333333333333333333333333333")
                .unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(owner.project_id.clone(), owner);
        catalog.projects.insert(sibling.project_id.clone(), sibling);
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_local_history_not_isolated"
        );
    }

    #[test]
    fn published_history_requires_matching_recorded_authority() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut published = project("one", ProjectScope::Published(scope("repo-1", ".")));
        published.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LegacyNamespace(
                CommitNamespace::parse("legacy-one").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("legacy-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .projects
            .insert(published.project_id.clone(), published);
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_repo_authority_mismatch"
        );
    }

    #[test]
    fn legacy_local_sibling_may_read_shared_recorded_history() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut published = project("published", ProjectScope::Published(scope("repo-1", ".")));
        published.repo_history = Some(history_id.clone());
        let mut legacy = project("legacy", ProjectScope::LegacyLocal);
        legacy.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("legacy-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .projects
            .insert(published.project_id.clone(), published);
        catalog.projects.insert(legacy.project_id.clone(), legacy);
        catalog.repo_histories.insert(history_id, history);
        catalog.validate().unwrap();
    }

    #[test]
    fn local_history_requires_code_owned_namespace_and_legacy_local_owner() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut owner = project("one", ProjectScope::LegacyLocal);
        owner.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LocalProject(owner.project_id.clone()),
            primary_namespace: CommitNamespace::parse("legacy-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(owner.project_id.clone(), owner);
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_local_history_namespace"
        );

        let namespace = CommitNamespace::mint_local(&CatalogSnapshotV2::empty(1).unwrap()).unwrap();
        assert!(is_local_commit_namespace(&namespace));

        let history_id = RepoHistoryId::parse("rh_44444444444444444444444444444444").unwrap();
        let mut published = project("published", ProjectScope::Published(scope("repo-1", ".")));
        published.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LocalProject(published.project_id.clone()),
            primary_namespace: CommitNamespace::parse("local_55555555555555555555555555555555")
                .unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .projects
            .insert(published.project_id.clone(), published);
        catalog.repo_histories.insert(history_id, history);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_local_history_not_isolated"
        );
    }

    #[test]
    fn migration_chain_preserves_namespace_and_ends_at_current_scope() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let migration_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let published = ProjectScope::Published(scope("repo-1", "."));
        let mut promoted = project("one", published.clone());
        promoted.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("legacy-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let migration = ScopeMigrationRecord {
            scope_migration_id: migration_id.clone(),
            project_id: promoted.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "blackbox project-catalog promote".into(),
            operator_reason: Some("approved promotion".into()),
            old_scope: ProjectScope::LegacyLocal,
            new_scope: published,
            kind: ScopeMigrationKind::Promotion,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(2).unwrap();
        catalog
            .projects
            .insert(promoted.project_id.clone(), promoted);
        catalog.repo_histories.insert(history_id, history);
        catalog.scope_migrations.insert(migration_id, migration);
        catalog.validate().unwrap();
    }

    #[test]
    fn migration_kind_requires_the_exact_typed_scope_change() {
        let migration_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let old = ProjectScope::Published(scope("repo-1", "."));
        let new = ProjectScope::Published(scope("repo-1", "api"));
        let mut moved = project("one", new.clone());
        moved.repo_history = None;
        let migration = ScopeMigrationRecord {
            scope_migration_id: migration_id.clone(),
            project_id: moved.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::OperatorAttested,
            operator_invocation: "blackbox project-catalog scope-migrate".into(),
            operator_reason: Some("approved relpath move".into()),
            old_scope: old,
            new_scope: new,
            kind: ScopeMigrationKind::RepoAuthorityChange,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(2).unwrap();
        catalog.projects.insert(moved.project_id.clone(), moved);
        catalog.scope_migrations.insert(migration_id, migration);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_migration_shape"
        );
    }

    #[test]
    fn migration_chain_rejects_discontinuous_transitions() {
        let first_id = ScopeMigrationId::parse("sm_11111111111111111111111111111111").unwrap();
        let second_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let first_scope = ProjectScope::Published(scope("repo-1", "api"));
        let current_scope = ProjectScope::Published(scope("repo-1", "web"));
        let project = project("one", current_scope.clone());
        let first = ScopeMigrationRecord {
            scope_migration_id: first_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "bbox_project_promote".into(),
            operator_reason: None,
            old_scope: ProjectScope::LegacyLocal,
            new_scope: first_scope,
            kind: ScopeMigrationKind::Promotion,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let second = ScopeMigrationRecord {
            scope_migration_id: second_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 3,
            authority_provenance: ScopeMigrationAuthorityProvenance::OperatorAttested,
            operator_invocation: "blackbox project-catalog scope-migrate".into(),
            operator_reason: Some("approved relpath move".into()),
            old_scope: ProjectScope::Published(scope("repo-1", "other")),
            new_scope: current_scope,
            kind: ScopeMigrationKind::RelpathMove,
            migrated_at: "2026-07-22T01:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(3).unwrap();
        catalog.projects.insert(project.project_id.clone(), project);
        catalog.scope_migrations.insert(first_id, first);
        catalog.scope_migrations.insert(second_id, second);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_migration_chain"
        );
    }

    #[test]
    fn catalog_rejects_migration_epoch_branch_and_reference_errors() {
        let (mut catalog, _, migration_id, _) = promoted_fixture();
        catalog
            .scope_migrations
            .get_mut(&migration_id)
            .unwrap()
            .catalog_epoch = 3;
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_migration_epoch"
        );

        let (mut catalog, _, migration_id, _) = promoted_fixture();
        catalog
            .scope_migrations
            .get_mut(&migration_id)
            .unwrap()
            .project_id = id("missing");
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_dangling_migration"
        );

        let (mut catalog, _, _, _) = promoted_fixture();
        catalog.projects.get_mut(&id("one")).unwrap().scope =
            ProjectScope::Published(scope("repo-1", "services/api"));
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_migration_chain"
        );

        let (mut catalog, _, _, _) = promoted_fixture();
        let second_id = ScopeMigrationId::parse("sm_33333333333333333333333333333333").unwrap();
        let old_scope = ProjectScope::Published(scope("repo-1", "."));
        let new_scope = ProjectScope::Published(scope("repo-1", "services/api"));
        catalog.projects.get_mut(&id("one")).unwrap().scope = new_scope.clone();
        catalog.scope_migrations.insert(
            second_id.clone(),
            ScopeMigrationRecord {
                scope_migration_id: second_id,
                project_id: id("one"),
                catalog_epoch: 2,
                authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
                operator_invocation: "blackbox project-catalog scope-migrate".into(),
                operator_reason: None,
                old_scope,
                new_scope,
                kind: ScopeMigrationKind::RelpathMove,
                migrated_at: "2026-07-22T01:00:00Z".into(),
                code_bridge_generation: None,
                publication_bridge_generation: None,
                pending_capabilities: BTreeSet::new(),
            },
        );
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_migration_chain"
        );
    }

    #[test]
    fn attachments_reject_bad_projection_duplicate_active_and_detached_capabilities() {
        let project_id = id("one");
        let first = attachment(&project_id, None);
        let mut bad_projection = first.clone();
        bad_projection.checkout_project_dir = "/tmp/other".into();
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot
            .attachments
            .insert(bad_projection.attachment_id.clone(), bad_projection);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_projection_mismatch"
        );

        let mut duplicate = first.clone();
        duplicate.attachment_id =
            AttachmentId::parse("att_33333333333333333333333333333333").unwrap();
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot
            .attachments
            .insert(first.attachment_id.clone(), first.clone());
        snapshot
            .attachments
            .insert(duplicate.attachment_id.clone(), duplicate);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_duplicate_active"
        );

        let mut cross_project = first.clone();
        cross_project.attachment_id =
            AttachmentId::parse("att_44444444444444444444444444444444").unwrap();
        cross_project.project_id = id("two");
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot
            .attachments
            .insert(first.attachment_id.clone(), first.clone());
        snapshot
            .attachments
            .insert(cross_project.attachment_id.clone(), cross_project);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_duplicate_active"
        );

        let mut detached = first;
        detached.status = AttachmentStatus::Detached;
        detached.detached_at = Some("2026-07-22T01:00:00Z".into());
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot
            .attachments
            .insert(detached.attachment_id.clone(), detached);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_detached_capability"
        );
    }

    #[test]
    fn legacy_path_ledger_rejects_duplicate_inventory_source_rows() {
        let first_id = LegacyPathBindingId::parse("lpb_11111111111111111111111111111111").unwrap();
        let second_id = LegacyPathBindingId::parse("lpb_22222222222222222222222222222222").unwrap();
        let first = LegacyPathLedgerEntry {
            legacy_path_binding_id: first_id.clone(),
            historical_path: "/tmp/old".into(),
            source_store: "knowledge".into(),
            source_row_id: "row-1".into(),
            member_row_count: 1,
            member_commitment_sha256: "a".repeat(64),
            inventory_epoch: 1,
            status: LegacyPathBindingStatus::Unscoped {},
        };
        let mut second = first.clone();
        second.legacy_path_binding_id = second_id.clone();
        second.historical_path = "/tmp/other".into();
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot.legacy_path_bindings.insert(first_id, first);
        snapshot.legacy_path_bindings.insert(second_id, second);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_duplicate_legacy_source"
        );
    }

    /// Only MAPPED bindings' historical paths are consumed as paths by
    /// dual-read; unscoped and quarantined bindings carry the v1 store's
    /// actual literal, including bare resolver names from early note rows.
    /// A path-shape demand on those refused the migration of every host
    /// carrying name-keyed rows.
    #[test]
    fn unscoped_bindings_accept_name_literals_and_mapped_bindings_do_not() {
        let binding_id =
            LegacyPathBindingId::parse("lpb_33333333333333333333333333333333").unwrap();
        let named = LegacyPathLedgerEntry {
            legacy_path_binding_id: binding_id.clone(),
            historical_path: "transcript-search".into(),
            source_store: "note".into(),
            source_row_id: "note-1".into(),
            member_row_count: 1,
            member_commitment_sha256: "a".repeat(64),
            inventory_epoch: 1,
            status: LegacyPathBindingStatus::Unscoped {},
        };
        let mut snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        snapshot
            .legacy_path_bindings
            .insert(binding_id.clone(), named.clone());
        snapshot.validate().unwrap();

        let mut mapped = named;
        mapped.status = LegacyPathBindingStatus::Mapped {
            project_id: ProjectId::parse("aaaaaaaa").unwrap(),
            relationship: LegacyPathRelationship::ContainedSubdirectory,
        };
        snapshot.legacy_path_bindings.insert(binding_id, mapped);
        assert_eq!(
            snapshot.validate().unwrap_err().code(),
            "error.project_attachments_invalid_path"
        );
    }

    #[test]
    fn cross_validation_rejects_scope_and_relpath_disagreement() {
        let published_project = project("one", ProjectScope::Published(scope("repo-1", ".")));
        let project_id = published_project.project_id.clone();
        let catalog = catalog_with(published_project);
        let row = attachment(&project_id, Some(scope("repo-2", ".")));
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        attachments
            .attachments
            .insert(row.attachment_id.clone(), row);
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_scope_mismatch"
        );

        let nested_scope = scope("repo-1", "services/api");
        let nested_project = project("nested", ProjectScope::Published(nested_scope.clone()));
        let mut wrong_projection = attachment(&nested_project.project_id, Some(nested_scope));
        wrong_projection.attachment_id =
            AttachmentId::parse("att_33333333333333333333333333333333").unwrap();
        let nested_catalog = catalog_with(nested_project);
        let mut nested_attachments = AttachmentSnapshotV1::empty(1).unwrap();
        nested_attachments
            .attachments
            .insert(wrong_projection.attachment_id.clone(), wrong_projection);
        assert_eq!(
            validate_catalog_attachments(&nested_catalog, &nested_attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_scope_mismatch"
        );

        let mut historical = attachment(&project_id, Some(scope("repo-2", ".")));
        historical.status = AttachmentStatus::Detached;
        historical.capabilities = AttachmentCapabilities::default();
        historical.detached_at = Some("2026-07-22T01:00:00Z".into());
        let mut detached_attachments = AttachmentSnapshotV1::empty(1).unwrap();
        detached_attachments
            .attachments
            .insert(historical.attachment_id.clone(), historical);
        validate_catalog_attachments(&catalog, &detached_attachments).unwrap();
    }

    #[test]
    fn cross_validation_rejects_dangling_ledger_and_proof_references() {
        let project = project("one", ProjectScope::LegacyLocal);
        let catalog = catalog_with(project);
        let dangling = attachment(&id("missing"), None);
        let mut dangling_attachments = AttachmentSnapshotV1::empty(1).unwrap();
        dangling_attachments
            .attachments
            .insert(dangling.attachment_id.clone(), dangling);
        assert_eq!(
            validate_catalog_attachments(&catalog, &dangling_attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_dangling_project"
        );

        let binding_id =
            LegacyPathBindingId::parse("lpb_11111111111111111111111111111111").unwrap();
        let binding = LegacyPathLedgerEntry {
            legacy_path_binding_id: binding_id.clone(),
            historical_path: "/tmp/old".into(),
            source_store: "knowledge".into(),
            source_row_id: "row-1".into(),
            member_row_count: 1,
            member_commitment_sha256: "a".repeat(64),
            inventory_epoch: 1,
            status: LegacyPathBindingStatus::Mapped {
                project_id: id("missing"),
                relationship: LegacyPathRelationship::Root,
            },
        };
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        attachments.legacy_path_bindings.insert(binding_id, binding);
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_dangling_legacy_binding"
        );

        let (mut catalog, attachments, migration_id, _) = promoted_fixture();
        catalog.scope_migrations.remove(&migration_id);
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_dangling_migration_proof"
        );

        let (catalog, mut attachments, _, attachment_id) = promoted_fixture();
        attachments.attachments.remove(&attachment_id);
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_dangling_migration_proof"
        );
    }

    #[test]
    fn cross_validation_rejects_operator_proof_and_every_proof_mismatch() {
        let (mut catalog, attachments, migration_id, _) = promoted_fixture();
        let migration = catalog.scope_migrations.get_mut(&migration_id).unwrap();
        migration.authority_provenance = ScopeMigrationAuthorityProvenance::OperatorAttested;
        migration.operator_reason = Some("operator attested relpath move".into());
        migration.old_scope = ProjectScope::Published(scope("repo-1", "services/api"));
        migration.new_scope = ProjectScope::Published(scope("repo-1", "."));
        migration.kind = ScopeMigrationKind::RelpathMove;
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_unexpected_migration_proof"
        );

        enum Mismatch {
            Project,
            Checkout,
            OldScope,
            NewScope,
        }

        for mismatch in [
            Mismatch::Project,
            Mismatch::Checkout,
            Mismatch::OldScope,
            Mismatch::NewScope,
        ] {
            let (mut catalog, mut attachments, migration_id, attachment_id) = promoted_fixture();
            match mismatch {
                Mismatch::Project => {
                    let sibling = project("two", ProjectScope::LegacyLocal);
                    catalog
                        .projects
                        .insert(sibling.project_id.clone(), sibling.clone());
                    let row = attachments.attachments.get_mut(&attachment_id).unwrap();
                    row.project_id = sibling.project_id;
                    row.status = AttachmentStatus::Detached;
                    row.capabilities = AttachmentCapabilities::default();
                    row.detached_at = Some("2026-07-22T01:00:00Z".into());
                }
                Mismatch::Checkout => {
                    attachments
                        .scope_migration_proofs
                        .get_mut(&migration_id)
                        .unwrap()
                        .checkout_id = "33333333333333333333333333333333".into();
                }
                Mismatch::OldScope => {
                    attachments
                        .scope_migration_proofs
                        .get_mut(&migration_id)
                        .unwrap()
                        .old_scope = ProjectScope::Published(scope("repo-2", "."));
                }
                Mismatch::NewScope => {
                    attachments
                        .scope_migration_proofs
                        .get_mut(&migration_id)
                        .unwrap()
                        .new_scope = ProjectScope::Published(scope("repo-1", "services/api"));
                }
            }
            assert_eq!(
                validate_catalog_attachments(&catalog, &attachments)
                    .unwrap_err()
                    .code(),
                "error.project_attachments_migration_proof_mismatch"
            );
        }
    }

    #[test]
    fn cross_validation_requires_and_checks_attachment_migration_proof() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let migration_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let published_scope = scope("repo-1", ".");
        let mut project = project("one", ProjectScope::Published(published_scope.clone()));
        project.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("deadbeef").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let migration = ScopeMigrationRecord {
            scope_migration_id: migration_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "bbox_project_promote".into(),
            operator_reason: None,
            old_scope: ProjectScope::LegacyLocal,
            new_scope: ProjectScope::Published(published_scope.clone()),
            kind: ScopeMigrationKind::Promotion,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(2).unwrap();
        catalog
            .projects
            .insert(project.project_id.clone(), project.clone());
        catalog.repo_histories.insert(history_id, history);
        catalog
            .scope_migrations
            .insert(migration_id.clone(), migration);

        let row = attachment(&project.project_id, Some(published_scope.clone()));
        let attachment_id = row.attachment_id.clone();
        let checkout_id = row.checkout_id.clone();
        let mut attachments = AttachmentSnapshotV1::empty(2).unwrap();
        attachments.attachments.insert(attachment_id.clone(), row);
        assert_eq!(
            validate_catalog_attachments(&catalog, &attachments)
                .unwrap_err()
                .code(),
            "error.project_attachments_missing_migration_proof"
        );

        let proof = ScopeMigrationAttachmentProof {
            scope_migration_id: migration_id.clone(),
            attachment_id,
            checkout_id,
            old_scope: ProjectScope::LegacyLocal,
            new_scope: ProjectScope::Published(published_scope),
            proved_at: "2026-07-22T00:00:00Z".into(),
        };
        attachments
            .scope_migration_proofs
            .insert(migration_id, proof);
        validate_catalog_attachments(&catalog, &attachments).unwrap();
    }

    #[test]
    fn historical_migration_proofs_survive_later_scope_changes_and_detach() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let promotion_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222").unwrap();
        let relpath_move_id =
            ScopeMigrationId::parse("sm_33333333333333333333333333333333").unwrap();
        let initial_scope = ProjectScope::Published(scope("repo-1", "."));
        let final_scope = ProjectScope::Published(scope("repo-1", "services/api"));
        let mut project = project("one", final_scope.clone());
        project.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("deadbeef").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let promotion = ScopeMigrationRecord {
            scope_migration_id: promotion_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 2,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "bbox_project_promote".into(),
            operator_reason: None,
            old_scope: ProjectScope::LegacyLocal,
            new_scope: initial_scope.clone(),
            kind: ScopeMigrationKind::Promotion,
            migrated_at: "2026-07-22T00:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let relpath_move = ScopeMigrationRecord {
            scope_migration_id: relpath_move_id.clone(),
            project_id: project.project_id.clone(),
            catalog_epoch: 3,
            authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "blackbox project-catalog scope-migrate".into(),
            operator_reason: None,
            old_scope: initial_scope.clone(),
            new_scope: final_scope.clone(),
            kind: ScopeMigrationKind::RelpathMove,
            migrated_at: "2026-07-22T01:00:00Z".into(),
            code_bridge_generation: None,
            publication_bridge_generation: None,
            pending_capabilities: BTreeSet::new(),
        };
        let mut catalog = CatalogSnapshotV2::empty(3).unwrap();
        catalog
            .projects
            .insert(project.project_id.clone(), project.clone());
        catalog.repo_histories.insert(history_id, history);
        catalog
            .scope_migrations
            .insert(promotion_id.clone(), promotion);
        catalog
            .scope_migrations
            .insert(relpath_move_id.clone(), relpath_move);

        let mut row = attachment(&project.project_id, Some(scope("repo-1", "services/api")));
        row.project_root_relpath = "services/api".into();
        row.checkout_project_dir = "/tmp/example/services/api".into();
        let attachment_id = row.attachment_id.clone();
        let checkout_id = row.checkout_id.clone();
        let mut attachments = AttachmentSnapshotV1::empty(3).unwrap();
        attachments.attachments.insert(attachment_id.clone(), row);
        attachments.scope_migration_proofs.insert(
            promotion_id.clone(),
            ScopeMigrationAttachmentProof {
                scope_migration_id: promotion_id,
                attachment_id: attachment_id.clone(),
                checkout_id: checkout_id.clone(),
                old_scope: ProjectScope::LegacyLocal,
                new_scope: initial_scope.clone(),
                proved_at: "2026-07-22T00:00:00Z".into(),
            },
        );
        attachments.scope_migration_proofs.insert(
            relpath_move_id.clone(),
            ScopeMigrationAttachmentProof {
                scope_migration_id: relpath_move_id,
                attachment_id: attachment_id.clone(),
                checkout_id,
                old_scope: initial_scope,
                new_scope: final_scope,
                proved_at: "2026-07-22T01:00:00Z".into(),
            },
        );
        validate_catalog_attachments(&catalog, &attachments).unwrap();

        let detached = attachments.attachments.get_mut(&attachment_id).unwrap();
        detached.status = AttachmentStatus::Detached;
        detached.capabilities = AttachmentCapabilities::default();
        detached.detached_at = Some("2026-07-22T02:00:00Z".into());
        validate_catalog_attachments(&catalog, &attachments).unwrap();
    }

    #[test]
    fn strict_attachment_codec_requires_every_capability_field() {
        let project_id = id("one");
        let row = attachment(&project_id, None);
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        attachments
            .attachments
            .insert(row.attachment_id.clone(), row);
        let encoded = encode_attachment_snapshot(&attachments).unwrap();
        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["attachments"]["att_11111111111111111111111111111111"]["capabilities"]
            .as_object_mut()
            .unwrap()
            .remove("git_history");
        let raw = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            decode_attachment_snapshot(&raw).unwrap_err().code(),
            "error.project_catalog_invalid_schema"
        );

        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        value["attachments"]["att_11111111111111111111111111111111"]["validated_scope"] = serde_json::json!({
            "repo_id": "repo-1",
            "bbox_root_relpath": ".",
            "unexpected": true
        });
        let raw = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            decode_attachment_snapshot(&raw).unwrap_err().code(),
            "error.project_catalog_invalid_schema"
        );
    }

    #[test]
    fn compatibility_join_requires_a_cross_validated_active_attachment() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut project = project("one", ProjectScope::Published(scope("repo-1", ".")));
        project.repo_history = Some(history_id.clone());
        let project_id = project.project_id.clone();
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-1").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("legacy-one").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = catalog_with(project.clone());
        catalog.repo_histories.insert(history_id, history);
        let row = attachment(&project_id, Some(scope("repo-1", ".")));
        let attachment_id = row.attachment_id.clone();
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        attachments.attachments.insert(attachment_id.clone(), row);
        let validated = validate_catalog_attachments(&catalog, &attachments).unwrap();
        let record = crate::project_record::ProjectRecord::from_catalog_attachment(
            &project,
            validated.attachment(&attachment_id).unwrap(),
        )
        .unwrap();
        assert_eq!(record.project_id, "one");
        assert_eq!(record.repo_id.as_deref(), Some("legacy-one"));
        assert_eq!(record.canonical_path, "/tmp/example");
        assert!(record.is_git_repo);

        let mut fabricated = project.clone();
        fabricated.display_name = "Fabricated metadata".into();
        assert!(
            crate::project_record::ProjectRecord::from_catalog_attachment(
                &fabricated,
                validated.attachment(&attachment_id).unwrap(),
            )
            .is_err()
        );

        let mut detached = attachment(&project_id, Some(scope("repo-1", ".")));
        detached.status = AttachmentStatus::Detached;
        detached.capabilities = AttachmentCapabilities::default();
        detached.detached_at = Some("2026-07-22T01:00:00Z".into());
        let detached_id = detached.attachment_id.clone();
        let mut detached_snapshot = AttachmentSnapshotV1::empty(1).unwrap();
        detached_snapshot
            .attachments
            .insert(detached_id.clone(), detached);
        let validated = validate_catalog_attachments(&catalog, &detached_snapshot).unwrap();
        assert!(
            crate::project_record::ProjectRecord::from_catalog_attachment(
                &project,
                validated.attachment(&detached_id).unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn compatibility_join_preserves_legacy_local_git_namespace_without_capability() {
        let history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut project = project("one", ProjectScope::LegacyLocal);
        project.repo_history = Some(history_id.clone());
        let history = RepoHistoryRecord {
            repo_history_id: history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::LegacyNamespace(
                CommitNamespace::parse("deadbeef").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("deadbeef").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut catalog = catalog_with(project.clone());
        catalog.repo_histories.insert(history_id, history);
        let mut row = attachment(&project.project_id, None);
        row.capabilities = AttachmentCapabilities::default();
        let attachment_id = row.attachment_id.clone();
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        attachments.attachments.insert(attachment_id.clone(), row);
        let validated = validate_catalog_attachments(&catalog, &attachments).unwrap();
        let record = crate::project_record::ProjectRecord::from_catalog_attachment(
            &project,
            validated.attachment(&attachment_id).unwrap(),
        )
        .unwrap();
        assert_eq!(record.repo_id.as_deref(), Some("deadbeef"));
        assert!(record.is_git_repo);
    }

    #[test]
    fn legacy_fixture_preserves_missing_defaulted_fields_and_unknown_fields() {
        let raw = br#"{
            "version": 1,
            "projects": [{
                "project_id": "deadbeef",
                "canonical_path": "/tmp/example",
                "registered_at": "2026-07-22T00:00:00Z",
                "is_git_repo": false,
                "legacy_extra": true
            }],
            "legacy_store_extra": true
        }"#;
        let store = decode_legacy_project_store(raw).unwrap();
        assert_eq!(store.projects.len(), 1);
        assert_eq!(store.projects[0].repo_id, None);
        assert!(store.projects[0].languages.is_empty());
        assert!(store.projects[0].aliases.is_empty());
    }

    #[test]
    fn legacy_project_store_budget_exceeds_the_strict_v2_snapshot_budget() {
        let mut raw = br#"{"version":1,"projects":[]}"#.to_vec();
        raw.resize(MAX_PROJECT_CATALOG_BYTES + 1, b' ');

        let decoded = decode_legacy_project_store(&raw).unwrap();
        assert!(decoded.projects.is_empty());
        assert_eq!(
            decode_catalog_snapshot(&raw).unwrap_err().code,
            "error.project_catalog_byte_limit"
        );
    }

    #[test]
    fn complete_v1_record_round_trips_through_the_compatibility_dto() {
        let raw = br#"{
            "version": 1,
            "projects": [{
                "project_id": "deadbeef",
                "repo_id": "cafebabe",
                "canonical_path": "/tmp/example",
                "registered_at": "2026-07-22T00:00:00Z",
                "is_git_repo": true,
                "languages": ["rust"],
                "aliases": ["example"]
            }]
        }"#;
        let store = decode_legacy_project_store(raw).unwrap();
        let legacy = store.projects[0].clone();
        let compatibility = crate::project_record::ProjectRecord::from(legacy.clone());
        let round_trip = LegacyProjectRecordV1::from(compatibility);
        assert_eq!(round_trip, legacy);

        let encoded = serde_json::to_value(&store).unwrap();
        let expected: Value = serde_json::from_slice(raw).unwrap();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn generation_id_shapes_accept_only_their_own_prefix_and_64_lowercase_hex() {
        assert!(RepoHistoryGenerationId::parse(format!("rhg_{}", "a".repeat(64))).is_ok());
        assert!(
            RepoHistoryQuarantineGenerationId::parse(format!("rhq_{}", "a".repeat(64))).is_ok()
        );
        for invalid in [
            format!("rhg_{}", "a".repeat(63)),
            format!("rhg_{}", "a".repeat(65)),
            format!("rhg_{}", "A".repeat(64)),
            format!("rhq_{}", "a".repeat(64)),
            "rhg_".to_string(),
            String::new(),
        ] {
            assert!(RepoHistoryGenerationId::parse(invalid).is_err());
        }
        for invalid in [
            format!("rhq_{}", "a".repeat(63)),
            format!("rhg_{}", "a".repeat(64)),
            String::new(),
        ] {
            assert!(RepoHistoryQuarantineGenerationId::parse(invalid).is_err());
        }
    }

    #[test]
    fn repo_history_additive_fields_default_from_legacy_bytes() {
        let history_a = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-a").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-a").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let history_b = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-b").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-b").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let ambiguous = AmbiguousNamespaceRecord {
            namespace: CommitNamespace::parse("shared-namespace").unwrap(),
            candidate_repo_history_ids: BTreeSet::from([
                history_a.repo_history_id.clone(),
                history_b.repo_history_id.clone(),
            ]),
            status: AmbiguousNamespaceStatus::Quarantined,
            materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .repo_histories
            .insert(history_a.repo_history_id.clone(), history_a);
        catalog
            .repo_histories
            .insert(history_b.repo_history_id.clone(), history_b);
        catalog
            .ambiguous_namespaces
            .insert(ambiguous.namespace.clone(), ambiguous);
        catalog.validate().unwrap();
        let encoded = encode_catalog_snapshot(&catalog).unwrap();

        // Simulate bytes from before the additive history fields: strict
        // decode still succeeds while every missing field takes its defined
        // compatibility default.
        let mut value: Value = serde_json::from_slice(&encoded).unwrap();
        for history in value["repo_histories"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            let history = history.as_object_mut().unwrap();
            history.remove("materialization");
            history.remove("membership_generation");
        }
        for ambiguous in value["ambiguous_namespaces"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            ambiguous.as_object_mut().unwrap().remove("materialization");
        }
        let stripped = serde_json::to_vec(&value).unwrap();
        let decoded = decode_catalog_snapshot(&stripped).unwrap();
        for history in decoded.repo_histories.values() {
            assert_eq!(history.membership_generation, 0);
            assert_eq!(
                history.materialization,
                RepoHistoryMaterialization::NotBuilt
            );
        }
        for ambiguous in decoded.ambiguous_namespaces.values() {
            assert_eq!(
                ambiguous.materialization,
                RepoHistoryQuarantineMaterialization::NotBuilt
            );
        }
    }

    #[test]
    fn ready_repo_history_generation_id_must_satisfy_validate_catalog() {
        let repo_history_id = RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap();
        let mut history = RepoHistoryRecord {
            repo_history_id: repo_history_id.clone(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-a").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-a").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            // Constructed directly (not through `parse`) to exercise the
            // validate_catalog defense-in-depth clause itself, independent
            // of the type-level guarantee `parse` already provides.
            materialization: RepoHistoryMaterialization::Ready {
                generation_id: RepoHistoryGenerationId(String::from("not-a-valid-shape")),
            },
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .repo_histories
            .insert(repo_history_id.clone(), history.clone());
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_history_generation"
        );

        history.materialization = RepoHistoryMaterialization::Ready {
            generation_id: RepoHistoryGenerationId::parse(format!("rhg_{}", "a".repeat(64)))
                .unwrap(),
        };
        catalog.repo_histories.insert(repo_history_id, history);
        catalog.validate().unwrap();
    }

    #[test]
    fn ready_ambiguous_generation_id_must_satisfy_validate_catalog_and_candidate_rules() {
        let history_a = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::parse("rh_11111111111111111111111111111111").unwrap(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-a").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-a").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let history_b = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::parse("rh_22222222222222222222222222222222").unwrap(),
            membership_generation: 0,
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-b").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("namespace-b").unwrap(),
            compatibility_namespaces: BTreeSet::new(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        let mut ambiguous = AmbiguousNamespaceRecord {
            namespace: CommitNamespace::parse("shared-namespace").unwrap(),
            candidate_repo_history_ids: BTreeSet::from([
                history_a.repo_history_id.clone(),
                history_b.repo_history_id.clone(),
            ]),
            status: AmbiguousNamespaceStatus::Quarantined,
            materialization: RepoHistoryQuarantineMaterialization::Ready {
                generation_id: RepoHistoryQuarantineGenerationId(String::from("bad")),
            },
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog
            .repo_histories
            .insert(history_a.repo_history_id.clone(), history_a);
        catalog
            .repo_histories
            .insert(history_b.repo_history_id.clone(), history_b);
        catalog
            .ambiguous_namespaces
            .insert(ambiguous.namespace.clone(), ambiguous.clone());
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_history_generation"
        );

        // A Ready ambiguous record still has to satisfy the ordinary
        // candidate rules (at least two existing candidates): dropping to
        // one candidate fails the pre-existing check, not a new one.
        ambiguous.materialization = RepoHistoryQuarantineMaterialization::Ready {
            generation_id: RepoHistoryQuarantineGenerationId::parse(format!(
                "rhq_{}",
                "a".repeat(64)
            ))
            .unwrap(),
        };
        ambiguous.candidate_repo_history_ids = BTreeSet::from([ambiguous
            .candidate_repo_history_ids
            .iter()
            .next()
            .unwrap()
            .clone()]);
        catalog
            .ambiguous_namespaces
            .insert(ambiguous.namespace.clone(), ambiguous);
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_ambiguity"
        );
    }

    /// One attachment snapshot exactly as a host wrote it BEFORE the member
    /// evidence existed: a relocation binding an ordinary checkout move minted,
    /// and an owner-row binding from the Phase 5 era.
    fn pre_evidence_attachment_snapshot() -> String {
        r#"{
  "version": 1,
  "epoch": 4,
  "attachments": {},
  "scope_migration_proofs": {},
  "legacy_path_bindings": {
    "lpb_11111111111111111111111111111111": {
      "legacy_path_binding_id": "lpb_11111111111111111111111111111111",
      "historical_path": "/host/checkouts/alpha",
      "source_store": "attachment-relocation",
      "source_row_id": "att_44444444444444444444444444444444",
      "inventory_epoch": 3,
      "status": {
        "kind": "mapped",
        "project_id": "a1b2c3d4",
        "relationship": "root"
      }
    },
    "lpb_22222222222222222222222222222222": {
      "legacy_path_binding_id": "lpb_22222222222222222222222222222222",
      "historical_path": "/host/checkouts/beta",
      "source_store": "knowledge",
      "source_row_id": "kb1",
      "inventory_epoch": 3,
      "status": {
        "kind": "unscoped"
      }
    }
  }
}
"#
        .to_string()
    }

    /// R2-1. A snapshot written before the member evidence existed must still
    /// OPEN.
    ///
    /// The evidence is required in everything this binary writes, but any host
    /// that has ever relocated an attachment already has a nonempty ledger, and
    /// a strict decode would refuse that host's attachment store outright,
    /// before a migration or a backfill could run at all. Absence is read as
    /// the singleton evidence the entry always implied: one row, committed over
    /// its own `source_row_id`.
    #[test]
    fn a_pre_evidence_snapshot_decodes_with_derived_singleton_evidence() {
        let snapshot =
            decode_attachment_snapshot(pre_evidence_attachment_snapshot().as_bytes()).unwrap();
        assert_eq!(snapshot.legacy_path_bindings.len(), 2);

        for (source_row_id, binding_id) in [
            (
                "att_44444444444444444444444444444444",
                "lpb_11111111111111111111111111111111",
            ),
            ("kb1", "lpb_22222222222222222222222222222222"),
        ] {
            let binding = snapshot
                .legacy_path_bindings
                .get(&LegacyPathBindingId::parse(binding_id).unwrap())
                .expect("both legacy bindings survive the decode");
            let derived =
                crate::project_catalog_snapshot::singleton_selector_members(source_row_id);
            assert_eq!(binding.member_row_count, 1);
            assert_eq!(binding.member_commitment_sha256, derived.commitment_sha256);
        }

        // And what we write back is unambiguous: both fields are emitted, so a
        // re-read never has to guess again.
        let encoded = String::from_utf8(encode_attachment_snapshot(&snapshot).unwrap()).unwrap();
        assert_eq!(encoded.matches("\"member_row_count\"").count(), 2);
        assert_eq!(encoded.matches("\"member_commitment_sha256\"").count(), 2);
        assert_eq!(
            decode_attachment_snapshot(encoded.as_bytes()).unwrap(),
            snapshot,
            "the normalized snapshot round-trips unchanged"
        );
    }

    /// R3-1. Reconstruction is only legal where the answer is KNOWABLE without
    /// asking the owner, and for a group-shaped source it is not.
    ///
    /// A transcript-edge binding names a selector group standing for an unknown
    /// number of physical lane rows - three, in the fixture this stands in for.
    /// Deriving a singleton would write "one row" into the migrated ledger, the
    /// backfill's refold would then disagree with it on every future run, and no
    /// fresh preflight could repair a record that is already durable. That is a
    /// dead end, so decode refuses instead, with its own code and a remedy that
    /// actually works: remigrate from the v1 predecessor, where every owner's
    /// evidence is captured rather than guessed.
    #[test]
    fn a_pre_evidence_group_shaped_binding_refuses_instead_of_being_invented() {
        let raw = pre_evidence_attachment_snapshot().replace(
            r#"      "source_store": "knowledge",
      "source_row_id": "kb1","#,
            r#"      "source_store": "transcript-edge",
      "source_row_id": "transcript_edge:1111111111111111111111111111111111111111111111111111111111111111:selector:2222222222222222222222222222222222222222222222222222222222222222","#,
        );
        assert!(
            raw.contains("transcript-edge"),
            "the fixture substitution must have applied"
        );

        let error = decode_attachment_snapshot(raw.as_bytes()).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_legacy_evidence_unreconstructable"
        );
        assert!(
            error
                .to_string()
                .contains("re-run the project-catalog migration"),
            "the refusal must name a repair that works: {error}"
        );
        // And it is NOT the generic invalid-field refusal, which would send the
        // operator to repair a record that is not broken, only old.
        assert_ne!(error.code(), "error.project_catalog_invalid_field");

        // The same binding WITH captured evidence opens normally: the refusal is
        // about absent evidence for a group, not about the owner.
        let repaired = raw.replace(
            r#"      "inventory_epoch": 3,
      "status": {
        "kind": "unscoped"
      }"#,
            &format!(
                r#"      "member_row_count": 3,
      "member_commitment_sha256": "{}",
      "inventory_epoch": 3,
      "status": {{
        "kind": "unscoped"
      }}"#,
                "b".repeat(64)
            ),
        );
        let snapshot = decode_attachment_snapshot(repaired.as_bytes()).unwrap();
        let binding = snapshot
            .legacy_path_bindings
            .get(&LegacyPathBindingId::parse("lpb_22222222222222222222222222222222").unwrap())
            .unwrap();
        assert_eq!(binding.member_row_count, 3);
    }

    /// The reconstructable set is an ALLOW-LIST: a token this version does not
    /// know is refused rather than guessed at, so a source added later cannot be
    /// silently mis-reconstructed by an older rule.
    #[test]
    fn an_unrecognized_pre_evidence_source_is_not_reconstructed() {
        assert!(legacy_ledger_evidence_is_reconstructable("knowledge"));
        assert!(legacy_ledger_evidence_is_reconstructable(
            "attachment-relocation"
        ));
        assert!(!legacy_ledger_evidence_is_reconstructable(
            "transcript-edge"
        ));
        assert!(!legacy_ledger_evidence_is_reconstructable(
            "a-source-this-version-never-minted"
        ));

        let raw = pre_evidence_attachment_snapshot().replace(
            r#""source_store": "knowledge","#,
            r#""source_store": "a-source-this-version-never-minted","#,
        );
        assert_eq!(
            decode_attachment_snapshot(raw.as_bytes())
                .unwrap_err()
                .code(),
            "error.project_catalog_legacy_evidence_unreconstructable"
        );
    }

    /// The normalization is for ABSENCE, never for a written value. A zeroed or
    /// half-written pair cannot be reconstructed, so it is refused rather than
    /// laundered into valid-looking evidence.
    #[test]
    fn written_member_evidence_is_never_normalized_away() {
        for injected in [
            r#""member_row_count": 0,"#,
            r#""member_row_count": 0,
      "member_commitment_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","#,
            r#""member_commitment_sha256": "not-a-digest","#,
            // A count without its commitment is exactly as unreconstructable as
            // a commitment without its count.
            r#""member_row_count": 2,"#,
        ] {
            let raw = pre_evidence_attachment_snapshot().replace(
                r#"      "source_row_id": "kb1","#,
                &format!("      \"source_row_id\": \"kb1\",\n      {injected}"),
            );
            let error = decode_attachment_snapshot(raw.as_bytes()).unwrap_err();
            assert_eq!(
                error.code(),
                "error.project_catalog_invalid_field",
                "unexpected refusal for {injected}: {error}"
            );
            assert!(
                error.to_string().contains("member evidence is invalid"),
                "the refusal must name the remediation: {error}"
            );
        }
    }

    // ── Connector scope family (Phase 0) ────────────────────────────────

    #[test]
    fn connector_source_id_shape_accepts_opaque_bounded_ids() {
        for accepted in [
            "csrc_5f2c1d9a4b6e470e",
            "drive-ops-2026",
            "a1b2c3d4",
            "team.ops.drive-01",
            &"x".repeat(MAX_CONNECTOR_SOURCE_ID_BYTES),
        ] {
            ConnectorSourceId::parse(accepted)
                .unwrap_or_else(|error| panic!("{accepted:?} must parse: {error}"));
        }
    }

    #[test]
    fn connector_source_id_shape_refuses_path_case_and_length_hazards() {
        for refused in [
            // too short to survive a transcription slip between the two
            // sides of the operator's config
            "short7",
            "",
            // over the durable bound
            &"x".repeat(MAX_CONNECTOR_SOURCE_ID_BYTES + 1),
            // path-shaped
            "drive/ops-2026",
            "drive\\ops-2026",
            "../drive-ops",
            ".drive-ops",
            "drive-ops.",
            "drive..ops",
            ".",
            "..",
            "/absolute/path",
            "c:\\drive\\ops",
            // authority-shaped
            "tenant@example.com",
            "https://drive.example",
            // case and whitespace confusion
            "Drive-Ops-2026",
            "drive ops 2026",
            "drive\tops",
            "drive\nops",
        ] {
            let error =
                ConnectorSourceId::parse(refused).expect_err(&format!("{refused:?} is refused"));
            assert_eq!(error.code(), "error.project_catalog_invalid_id");
        }
    }

    #[test]
    fn connector_kind_is_an_opaque_token_not_a_closed_vendor_enum() {
        for accepted in ["gdrive", "graph", "webdav", "s3", "slack", "local_mirror"] {
            ConnectorKind::parse(accepted).unwrap();
        }
        for refused in [
            "", "GDrive", "g drive", "g-drive", "3s", "_gdrive", "drive/x",
        ] {
            let error =
                ConnectorKind::parse(refused).expect_err(&format!("{refused:?} is refused"));
            assert_eq!(error.code(), "error.project_catalog_invalid_id");
        }
    }

    #[test]
    fn connector_scope_round_trips_through_the_tagged_envelope() {
        let scope = ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e"));
        let encoded = serde_json::to_value(&scope).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "connector",
                "scope": {
                    "connector_source_id": "csrc_5f2c1d9a4b6e470e",
                    "connector_kind": "gdrive",
                },
            }),
            "the connector envelope is part of the durable wire contract"
        );
        let decoded: ProjectScope = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, scope);
        assert_eq!(scope.kind_label(), "connector");
        assert!(
            scope.published().is_none(),
            "a connector scope names no repository"
        );
        assert_eq!(
            scope.connector().unwrap().connector_kind().as_str(),
            "gdrive"
        );
    }

    #[test]
    fn connector_scope_envelope_refuses_unknown_fields_and_vendor_coordinates() {
        // A provider coordinate smuggled into the durable scope is refused:
        // coordinates are observations, never identity.
        for injected in [
            serde_json::json!({
                "kind": "connector",
                "scope": {
                    "connector_source_id": "csrc_5f2c1d9a4b6e470e",
                    "connector_kind": "gdrive",
                    "remote_root_id": "0ABcDeFgHiJkLmN",
                },
            }),
            serde_json::json!({
                "kind": "connector",
                "scope": {
                    "connector_source_id": "csrc_5f2c1d9a4b6e470e",
                    "connector_kind": "gdrive",
                    "remote_authority": "tenant.example",
                },
            }),
        ] {
            serde_json::from_value::<ProjectScope>(injected)
                .expect_err("the connector scope envelope is strict");
        }
        serde_json::from_value::<ProjectScope>(serde_json::json!({
            "kind": "connector",
            "scope": { "connector_source_id": "../x", "connector_kind": "gdrive" },
        }))
        .expect_err("a path-shaped connector_source_id is refused at the wire");
    }

    #[test]
    fn duplicate_connector_source_id_refuses_even_under_a_different_kind() {
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        let second = project(
            "p_2222",
            ProjectScope::Connector(
                ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "graph").unwrap(),
            ),
        );
        catalog.projects.insert(second.project_id.clone(), second);
        let error = catalog.validate().unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_duplicate_scope");
    }

    #[test]
    fn distinct_connector_source_ids_coexist() {
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        let second = project(
            "p_2222",
            ProjectScope::Connector(connector_scope("csrc_00000000deadbeef")),
        );
        catalog.projects.insert(second.project_id.clone(), second);
        catalog.validate().unwrap();
    }

    #[test]
    fn catalog_version_is_derived_from_content() {
        let published = catalog_with(project(
            "p_1111",
            ProjectScope::Published(scope("repo-a", ".")),
        ));
        assert_eq!(
            published.required_version(),
            CATALOG_VERSION_V2,
            "a catalog with no connector scope still writes v2 bytes"
        );
        assert_eq!(published.version, CATALOG_VERSION_V2);

        let connector = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        assert_eq!(connector.required_version(), CATALOG_VERSION_V3);
        assert_eq!(connector.version, CATALOG_VERSION_V3);
    }

    #[test]
    fn a_catalog_understating_its_version_is_refused() {
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        catalog.version = CATALOG_VERSION_V2;
        let error = catalog.validate().unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_unsupported_version");

        catalog.version = CATALOG_VERSION_MAX + 1;
        let error = catalog.validate().unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_unsupported_version");
    }

    #[test]
    fn connector_catalog_survives_an_encode_decode_reopen() {
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        catalog.connector_observations.insert(
            id("p_1111"),
            ConnectorObservationsV1 {
                observed_at: "2026-08-13T00:00:00Z".into(),
                producer_id: Some("producer-a".into()),
                remote_authority: Some("tenant.example".into()),
                remote_root_id: Some("0ABcDeFgHiJkLmN".into()),
                remote_display_name: Some("Ops shared folder".into()),
            },
        );
        let raw = encode_catalog_snapshot(&catalog).unwrap();
        let reopened = decode_catalog_snapshot(&raw).unwrap();
        assert_eq!(reopened, catalog, "a connector catalog reopens unchanged");
        assert_eq!(reopened.version, CATALOG_VERSION_V3);
        let observed = &reopened.connector_observations[&id("p_1111")];
        assert_eq!(observed.remote_root_id.as_deref(), Some("0ABcDeFgHiJkLmN"));
    }

    #[test]
    fn connector_observations_are_subordinate_to_a_connector_project() {
        // Dangling: names no project at all.
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        catalog
            .connector_observations
            .insert(id("p_9999"), bare_observations());
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_dangling_connector_observation"
        );

        // Attached to a project that is not connector-scoped.
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Published(scope("repo-a", ".")),
        ));
        catalog
            .connector_observations
            .insert(id("p_1111"), bare_observations());
        assert_eq!(
            catalog.validate().unwrap_err().code(),
            "error.project_catalog_invalid_connector_observation"
        );
    }

    #[test]
    fn connector_observation_text_is_bounded_and_control_free() {
        let mut catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        for injected in [
            "x".repeat(MAX_CONNECTOR_OBSERVATION_BYTES + 1),
            "a\u{0}b".to_string(),
            String::new(),
        ] {
            let mut observations = bare_observations();
            observations.remote_authority = Some(injected.clone());
            catalog
                .connector_observations
                .insert(id("p_1111"), observations);
            assert_eq!(
                catalog.validate().unwrap_err().code(),
                "error.project_catalog_invalid_connector_observation",
                "unexpected acceptance for {injected:?}"
            );
        }
    }

    fn bare_observations() -> ConnectorObservationsV1 {
        ConnectorObservationsV1 {
            observed_at: "2026-08-13T00:00:00Z".into(),
            producer_id: None,
            remote_authority: None,
            remote_root_id: None,
            remote_display_name: None,
        }
    }

    // ── The downgrade story ─────────────────────────────────────────────
    //
    // These mirror types ARE the pre-connector reader, reproduced field for
    // field: the strict two-variant scope enum, the strict project envelope,
    // and the snapshot envelope whose version gate accepted only 2. Decoding
    // this build's own bytes through them is the proof, and it is the only
    // way to exercise an old reader without shipping an old binary into the
    // test.

    #[derive(Debug, Deserialize)]
    #[serde(
        tag = "kind",
        content = "scope",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    #[allow(dead_code)]
    enum PreConnectorProjectScope {
        Published(PublishedScope),
        LegacyLocal,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct PreConnectorCorpusProject {
        project_id: ProjectId,
        scope: PreConnectorProjectScope,
        operator_aliases: BTreeSet<String>,
        nominated_aliases: BTreeSet<String>,
        display_name: String,
        created_at: String,
        #[serde(default)]
        registered_at_compat: Option<String>,
        #[serde(default)]
        repo_history: Option<RepoHistoryId>,
        languages: BTreeSet<Language>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct PreConnectorCatalogSnapshot {
        version: u32,
        epoch: u64,
        origin: CatalogOriginV2,
        projects: BTreeMap<ProjectId, PreConnectorCorpusProject>,
        repo_histories: BTreeMap<RepoHistoryId, RepoHistoryRecord>,
        ambiguous_namespaces: BTreeMap<CommitNamespace, AmbiguousNamespaceRecord>,
        scope_migrations: BTreeMap<ScopeMigrationId, ScopeMigrationRecord>,
    }

    /// The pre-connector reader's REAL two-stage sequence.
    ///
    /// Stage order is the whole point and an earlier version of this mirror
    /// got it wrong by modelling only `decode_catalog_snapshot`. That
    /// function decodes rows and then validates, but nothing reaches it
    /// until the probe has already approved the version: daemon startup runs
    /// `probe_project_store_mode` first, described in `src/server/open.rs` as
    /// "one strict probe decides the runtime authority for the process
    /// lifetime, before any project-scoped subsystem starts". The probe
    /// deserializes ONLY `{ version }` and refuses an unknown one, so a
    /// rolled-back daemon never parses a single project row.
    fn decode_as_pre_connector_reader(raw: &[u8]) -> Result<PreConnectorCatalogSnapshot, String> {
        // Stage 1: the startup version probe, row-blind by construction.
        #[derive(Deserialize)]
        struct VersionProbe {
            version: u64,
        }
        let probe: VersionProbe = serde_json::from_slice(raw).map_err(|error| error.to_string())?;
        match probe.version {
            // 1 selects the version-1 bridge store, a different document
            // shape entirely; 2 is the catalog this reader understands.
            1 | 2 => {}
            _ => return Err("error.project_catalog_unsupported_version".to_string()),
        }

        // Stage 2: only a probe-approved store is opened and strictly
        // decoded, rows and all.
        let snapshot: PreConnectorCatalogSnapshot =
            serde_json::from_slice(raw).map_err(|error| error.to_string())?;
        if snapshot.version != CATALOG_VERSION_V2 {
            return Err("error.project_catalog_unsupported_version".to_string());
        }
        Ok(snapshot)
    }

    #[test]
    fn downgrade_opens_a_connector_free_catalog_written_by_this_build() {
        let catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Published(scope("repo-a", ".")),
        ));
        let raw = encode_catalog_snapshot(&catalog).unwrap();
        assert!(
            !String::from_utf8(raw.clone())
                .unwrap()
                .contains("connector_observations"),
            "an empty observation map must not reach the bytes at all, or every \
             pre-connector reader breaks on a catalog that has no connectors"
        );
        let reopened = decode_as_pre_connector_reader(&raw)
            .expect("a connector-free catalog written here opens under v2 expectations");
        assert_eq!(reopened.version, CATALOG_VERSION_V2);
        assert_eq!(reopened.projects.len(), 1);
    }

    #[test]
    fn downgrade_fails_closed_on_a_catalog_that_holds_connector_scopes() {
        let catalog = catalog_with(project(
            "p_1111",
            ProjectScope::Connector(connector_scope("csrc_5f2c1d9a4b6e470e")),
        ));
        let raw = encode_catalog_snapshot(&catalog).unwrap();
        // Fail-closed, named explicitly: the old reader refuses the WHOLE
        // catalog rather than opening it minus the projects it cannot
        // represent. Silently dropping them would orphan their content and
        // free a durable scope for reuse; refusing is the only honest
        // outcome, and the remedy is to roll forward, never to hand-edit
        // the version field.
        //
        // The refusal an operator actually SEES during a rollback comes from
        // the startup version probe, so it is the typed one. The version
        // number is therefore load-bearing, not forensic decoration: it is
        // the entire mechanism by which an older daemon knows to stop.
        let error = decode_as_pre_connector_reader(&raw)
            .expect_err("a v3 catalog must refuse to open under v2 expectations");
        assert_eq!(error, "error.project_catalog_unsupported_version");

        // Second line, different shape. Anything that skips the probe and
        // hands these bytes straight to a strict row decode still fails
        // closed, but with serde's unknown-variant parse error rather than
        // the typed refusal. Both shapes are pinned here so an operator
        // staring at either one during a rollback recognizes it, and so a
        // future change cannot quietly turn one into a silent success.
        let strict_row_decode = serde_json::from_slice::<PreConnectorCatalogSnapshot>(&raw)
            .expect_err("strict row decode must refuse an unknown scope variant");
        let rendered = strict_row_decode.to_string();
        assert!(
            rendered.contains("unknown variant") && rendered.contains("connector"),
            "the raw-decode refusal must name the variant it could not represent: {rendered}"
        );
    }
}
