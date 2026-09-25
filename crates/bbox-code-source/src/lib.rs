//! Dependency-clean wire and filesystem policy for distributed code sources.

pub mod cutback_state;

pub use cutback_state::{CutbackErrorClass, CutbackReason, CutbackStateV2};

use std::path::{Component, Path};

use bbox_corpus_core::identity::PublishedScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;
pub const WALKER_POLICY_VERSION: &str = "code-source-walker-v1";
pub const MAX_RELATIVE_PATH_BYTES: usize = 4096;
pub const MAX_PATH_COMPONENT_BYTES: usize = 255;
pub const MAX_MANIFEST_PAGE_ENTRIES: usize = 2_000;
pub const MAX_MANIFEST_PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_DOCUMENT_FILE_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_IMAGE_FILE_BYTES: u64 = 20 * 1024 * 1024;
pub const DEFAULT_MAX_MANIFEST_FILES: u64 = 250_000;
pub const DEFAULT_MAX_MANIFEST_LOGICAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;

const SKIP_DIRS: &[&str] = &["target", "node_modules", "_build", ".worktrees"];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ContractError {
    #[error("unsupported code-source schema version {0}")]
    UnsupportedSchema(u32),
    #[error("walker policy mismatch: received {received}, expected {expected}")]
    WalkerPolicyMismatch {
        received: String,
        expected: &'static str,
    },
    #[error("invalid published scope: {0}")]
    InvalidScope(String),
    #[error("invalid catalog onboard request field: {0}")]
    InvalidOnboardField(&'static str),
    #[error("invalid checkout mutation field: {0}")]
    InvalidCheckoutMutationField(&'static str),
    #[error("invalid producer command field: {0}")]
    InvalidProducerCommandField(&'static str),
    #[error("invalid producer id")]
    InvalidProducerId,
    #[error("invalid relative path: {0}")]
    InvalidRelativePath(String),
    #[error("invalid sha256 digest")]
    InvalidDigest,
    #[error("invalid collected materialization selector")]
    InvalidCollectedMaterializationSelector,
    #[error("unsupported source path: {0}")]
    UnsupportedPath(String),
    #[error("source file {path} exceeds its {max_bytes}-byte cap")]
    FileTooLarge { path: String, max_bytes: u64 },
    #[error("manifest entries are not strictly sorted")]
    ManifestNotSorted,
    #[error("manifest contains duplicate path {0}")]
    DuplicatePath(String),
    #[error("manifest declares {actual} files, limit is {limit}")]
    TooManyFiles { actual: u64, limit: u64 },
    #[error("manifest declares {actual} logical bytes, limit is {limit}")]
    TooManyBytes { actual: u64, limit: u64 },
    #[error("invalid source uri")]
    InvalidSourceUri,
    #[error("manifest file count does not match descriptor")]
    FileCountMismatch,
    #[error("manifest logical byte count does not match descriptor")]
    LogicalBytesMismatch,
    #[error("manifest digest does not match descriptor")]
    ManifestDigestMismatch,
    #[error("dirty fingerprint does not match descriptor")]
    DirtyFingerprintMismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub relative_path: String,
    pub content_sha256: String,
    pub size: u64,
}

impl ManifestEntry {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_relative_path(&self.relative_path)?;
        if self.relative_path.split('/').any(is_skipped_component) {
            return Err(ContractError::UnsupportedPath(self.relative_path.clone()));
        }
        validate_sha256(&self.content_sha256)?;
        let max_bytes = max_bytes_for_path(Path::new(&self.relative_path))
            .ok_or_else(|| ContractError::UnsupportedPath(self.relative_path.clone()))?;
        if self.size > max_bytes {
            return Err(ContractError::FileTooLarge {
                path: self.relative_path.clone(),
                max_bytes,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationDescriptor {
    pub schema_version: u32,
    pub walker_policy_version: String,
    pub scope: PublishedScope,
    pub head_commit: String,
    pub dirty_fingerprint: String,
    pub manifest_sha256: String,
    pub file_count: u64,
    pub logical_bytes: u64,
}

impl GenerationDescriptor {
    pub fn validate_header(&self) -> Result<(), ContractError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        if self.walker_policy_version != WALKER_POLICY_VERSION {
            return Err(ContractError::WalkerPolicyMismatch {
                received: self.walker_policy_version.clone(),
                expected: WALKER_POLICY_VERSION,
            });
        }
        validate_scope(&self.scope)?;
        validate_sha256(&self.dirty_fingerprint)?;
        validate_sha256(&self.manifest_sha256)?;
        validate_git_commit(&self.head_commit)?;
        Ok(())
    }

    pub fn validate_manifest(
        &self,
        entries: &[ManifestEntry],
        max_files: u64,
        max_logical_bytes: u64,
    ) -> Result<(), ContractError> {
        self.validate_header()?;
        validate_manifest(entries, max_files, max_logical_bytes)?;
        let file_count = entries.len() as u64;
        let logical_bytes = entries.iter().try_fold(0_u64, |sum, entry| {
            sum.checked_add(entry.size)
                .ok_or(ContractError::TooManyBytes {
                    actual: u64::MAX,
                    limit: max_logical_bytes,
                })
        })?;
        if file_count != self.file_count {
            return Err(ContractError::FileCountMismatch);
        }
        if logical_bytes != self.logical_bytes {
            return Err(ContractError::LogicalBytesMismatch);
        }
        if manifest_sha256(entries) != self.manifest_sha256 {
            return Err(ContractError::ManifestDigestMismatch);
        }
        if dirty_fingerprint(&self.head_commit, entries) != self.dirty_fingerprint {
            return Err(ContractError::DirtyFingerprintMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginUploadRequest {
    pub descriptor: GenerationDescriptor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BeginUploadResponse {
    pub upload_id: String,
    pub ordinal: u64,
    pub max_page_entries: usize,
    pub max_page_bytes: usize,
}

/// Pre-upload currency probe: the collector sends the descriptor it just
/// scanned and the server answers with the active generation when the scope's
/// activated content is identical, letting the collector skip the entire
/// upload protocol for an unchanged tree (mirrors the Git-history probe).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeSourceProbeRequestV1 {
    pub descriptor: GenerationDescriptor,
}

impl CodeSourceProbeRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.descriptor.validate_header()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeSourceProbeResponseV1 {
    pub current: Option<CodeSourceProbeCurrentV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeSourceProbeCurrentV1 {
    pub generation_id: String,
    pub file_count: u64,
    pub logical_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestPage {
    pub entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissingBlobsPage {
    pub generation_id: String,
    pub hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalizeResponse {
    pub generation_id: String,
    pub status_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    ReceivingManifest,
    MissingBlobs,
    Ready,
    StagingIndex,
    Active,
    Superseded,
    MissingBlobData,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationStatus {
    pub generation_id: String,
    pub state: GenerationState,
    pub file_count: u64,
    pub logical_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Catalog onboarding (remote project registration via the producer channel)
// ---------------------------------------------------------------------------

pub const CATALOG_ONBOARD_SCHEMA_VERSION: u32 = 1;
pub const MAX_ONBOARD_ALIASES: usize = 64;
pub const MAX_ONBOARD_PATH_BYTES: usize = 4096;

/// Checkout facts probed by the checkout-owner collector and presented over
/// the authenticated producer channel. The daemon revalidates every field it
/// can check independently (scope membership in the producer grant, repo_id
/// equality, relpath shape and agreement, catalog uniqueness); the residual
/// trust in the canonical path strings is exactly the trust the publication
/// lanes already place in the producer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogOnboardRequestV1 {
    pub schema_version: u32,
    /// The published scope the producer grant must already cover.
    pub scope: PublishedScope,
    /// Producer-canonical checkout top (absolute on the producer host).
    /// Carried as producer-side data; the daemon never opens it.
    pub producer_checkout_dir: String,
    /// Producer-canonical project dir inside the checkout. Carried as
    /// producer-side data; the daemon never opens it.
    pub producer_project_dir: String,
    /// Monorepo discriminator relative to the checkout top (`.` at root).
    pub project_root_relpath: String,
    /// Checkout shape: `base`, `worktree`, or `managed_clone`.
    pub checkout_kind: String,
    /// Durable checkout identity from `.bbox/local/checkout-id`.
    pub checkout_id: String,
    pub branch_ref: Option<String>,
    /// repo_id recorded by the committed config at HEAD, when present.
    pub committed_repo_id: Option<String>,
    #[serde(default)]
    pub declared_aliases: Vec<String>,
    /// Capabilities observed at onboard time; lease-time acquisition still
    /// revalidates each capability.
    pub capabilities: bbox_corpus_core::project_catalog::AttachmentCapabilities,
}

impl CatalogOnboardRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CATALOG_ONBOARD_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        validate_scope(&self.scope)?;
        for path in [&self.producer_checkout_dir, &self.producer_project_dir] {
            if path.is_empty()
                || path.len() > MAX_ONBOARD_PATH_BYTES
                || !path.starts_with('/')
                || path
                    .bytes()
                    .any(|byte| byte == 0 || byte.is_ascii_control())
            {
                return Err(ContractError::InvalidOnboardField("checkout path"));
            }
        }
        if self.project_root_relpath != self.scope.bbox_root_relpath() {
            return Err(ContractError::InvalidOnboardField(
                "project_root_relpath disagrees with scope",
            ));
        }
        if self.project_root_relpath != "."
            && validate_relative_path(&self.project_root_relpath).is_err()
        {
            return Err(ContractError::InvalidOnboardField("project_root_relpath"));
        }
        if !matches!(
            self.checkout_kind.as_str(),
            "base" | "worktree" | "managed_clone"
        ) {
            return Err(ContractError::InvalidOnboardField("checkout_kind"));
        }
        if self.checkout_id.len() != 32
            || !self
                .checkout_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ContractError::InvalidOnboardField("checkout_id"));
        }
        if let Some(repo_id) = &self.committed_repo_id {
            if repo_id != self.scope.repo_id() {
                return Err(ContractError::InvalidOnboardField(
                    "committed_repo_id disagrees with scope",
                ));
            }
        }
        if self.declared_aliases.len() > MAX_ONBOARD_ALIASES
            || self
                .declared_aliases
                .iter()
                .any(|alias| alias.is_empty() || alias.len() > 256)
        {
            return Err(ContractError::InvalidOnboardField("declared_aliases"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CatalogOnboardResponseV1 {
    pub project_id: String,
    pub attachment_id: String,
    pub created_project: bool,
    pub already_attached: bool,
    pub epoch: u64,
    #[serde(default)]
    pub nominated_aliases: Vec<String>,
}

// ---------------------------------------------------------------------------
// Producer command backchannel (v1)
// ---------------------------------------------------------------------------

pub const PRODUCER_COMMAND_SCHEMA_VERSION: u32 = 1;
pub const MAX_PRODUCER_ENROLL_ROOTS: usize = 64;
pub const MAX_PRODUCER_COMMANDS_PER_POLL: usize = 64;
pub const MAX_PRODUCER_COMMAND_PATH_BYTES: usize = 4096;
pub const MAX_PRODUCER_COMMAND_LABEL_BYTES: usize = 256;
pub const MAX_PRODUCER_COMMAND_VERSION_BYTES: usize = 128;
pub const MAX_PRODUCER_COMMAND_REF_BYTES: usize = 1024;
pub const MAX_PRODUCER_COMMAND_ERROR_BYTES: usize = 4096;
pub const MAX_ENROLL_RECEIPT_COMMIT_PATHS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerPresenceV1 {
    pub enroll_roots: Vec<String>,
    pub host_label: String,
    pub config_path: String,
    pub service_label: Option<String>,
    pub collector_version: String,
}

impl ProducerPresenceV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.enroll_roots.len() > MAX_PRODUCER_ENROLL_ROOTS {
            return Err(ContractError::InvalidProducerCommandField("enroll_roots"));
        }
        for root in &self.enroll_roots {
            validate_absolute_command_path(root, "enroll_roots")?;
        }
        validate_command_string(
            &self.host_label,
            MAX_PRODUCER_COMMAND_LABEL_BYTES,
            "host_label",
        )?;
        validate_absolute_command_path(&self.config_path, "config_path")?;
        if let Some(service_label) = &self.service_label {
            validate_command_string(
                service_label,
                MAX_PRODUCER_COMMAND_LABEL_BYTES,
                "service_label",
            )?;
        }
        validate_command_string(
            &self.collector_version,
            MAX_PRODUCER_COMMAND_VERSION_BYTES,
            "collector_version",
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerCommandPollRequestV1 {
    pub schema_version: u32,
    pub presence: ProducerPresenceV1,
}

impl ProducerCommandPollRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != PRODUCER_COMMAND_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        self.presence.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerCommandPollResponseV1 {
    pub commands: Vec<ProducerCommandV1>,
}

impl ProducerCommandPollResponseV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.commands.len() > MAX_PRODUCER_COMMANDS_PER_POLL {
            return Err(ContractError::InvalidProducerCommandField("commands"));
        }
        self.commands
            .iter()
            .try_for_each(ProducerCommandV1::validate)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerCommandV1 {
    pub command_id: String,
    pub kind: String,
    pub path: String,
    pub full_ref: Option<String>,
}

impl ProducerCommandV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_producer_command_id(&self.command_id)?;
        if self.kind != "enroll" {
            return Err(ContractError::InvalidProducerCommandField("kind"));
        }
        validate_absolute_command_path(&self.path, "path")?;
        if let Some(full_ref) = &self.full_ref {
            validate_command_string(full_ref, MAX_PRODUCER_COMMAND_REF_BYTES, "full_ref")?;
            if !full_ref.starts_with("refs/") {
                return Err(ContractError::InvalidProducerCommandField("full_ref"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerCommandErrorV1 {
    pub code: String,
    pub message: String,
}

impl ProducerCommandErrorV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_command_string(&self.code, MAX_PRODUCER_COMMAND_LABEL_BYTES, "error.code")?;
        validate_command_string(
            &self.message,
            MAX_PRODUCER_COMMAND_ERROR_BYTES,
            "error.message",
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollOnboardErrorV1 {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: String,
}

impl EnrollOnboardErrorV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(code) = &self.code {
            validate_command_string(code, MAX_PRODUCER_COMMAND_LABEL_BYTES, "onboard_error.code")?;
        }
        validate_command_string(
            &self.message,
            MAX_PRODUCER_COMMAND_ERROR_BYTES,
            "onboard_error.message",
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollReceiptV1 {
    pub project_id: Option<String>,
    pub attachment_id: Option<String>,
    pub created_project: bool,
    pub already_attached: bool,
    pub scope: PublishedScope,
    pub published_ref: String,
    pub identity_committed: bool,
    #[serde(default)]
    pub commit_paths: Vec<String>,
    pub onboard_error: Option<EnrollOnboardErrorV1>,
}

impl EnrollReceiptV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        for (value, field) in [
            (&self.project_id, "project_id"),
            (&self.attachment_id, "attachment_id"),
        ] {
            if let Some(value) = value {
                validate_command_string(value, MAX_PRODUCER_COMMAND_LABEL_BYTES, field)?;
            }
        }
        validate_scope(&self.scope)?;
        validate_command_string(
            &self.published_ref,
            MAX_PRODUCER_COMMAND_REF_BYTES,
            "published_ref",
        )?;
        if !self.published_ref.starts_with("refs/") {
            return Err(ContractError::InvalidProducerCommandField("published_ref"));
        }
        if self.commit_paths.len() > MAX_ENROLL_RECEIPT_COMMIT_PATHS {
            return Err(ContractError::InvalidProducerCommandField("commit_paths"));
        }
        for path in &self.commit_paths {
            if path.len() > MAX_PRODUCER_COMMAND_PATH_BYTES || validate_relative_path(path).is_err()
            {
                return Err(ContractError::InvalidProducerCommandField("commit_paths"));
            }
        }
        if self.identity_committed && !self.commit_paths.is_empty() {
            return Err(ContractError::InvalidProducerCommandField("commit_paths"));
        }
        if let Some(error) = &self.onboard_error {
            error.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProducerCommandAckRequestV1 {
    pub command_id: String,
    pub outcome: String,
    pub receipt: Option<EnrollReceiptV1>,
    pub error: Option<ProducerCommandErrorV1>,
}

impl ProducerCommandAckRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_producer_command_id(&self.command_id)?;
        match self.outcome.as_str() {
            "applied" if self.receipt.is_some() && self.error.is_none() => self
                .receipt
                .as_ref()
                .expect("checked receipt presence")
                .validate(),
            "failed" if self.receipt.is_none() && self.error.is_some() => self
                .error
                .as_ref()
                .expect("checked error presence")
                .validate(),
            "applied" | "failed" => Err(ContractError::InvalidProducerCommandField(
                "outcome payload",
            )),
            _ => Err(ContractError::InvalidProducerCommandField("outcome")),
        }
    }
}

fn validate_producer_command_id(value: &str) -> Result<(), ContractError> {
    if value.len() != 19
        || !value.starts_with("pc-")
        || !value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ContractError::InvalidProducerCommandField("command_id"));
    }
    Ok(())
}

fn validate_absolute_command_path(value: &str, field: &'static str) -> Result<(), ContractError> {
    if value.len() > MAX_PRODUCER_COMMAND_PATH_BYTES
        || !Path::new(value).is_absolute()
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ContractError::InvalidProducerCommandField(field));
    }
    Ok(())
}

fn validate_command_string(
    value: &str,
    max_bytes: usize,
    field: &'static str,
) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ContractError::InvalidProducerCommandField(field));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Checkout-mutation backchannel (v1)
//
// The daemon computes repo-owned file mutations it cannot apply itself (zero
// checkout authority): a validated gap record, a rewritten knowledge entry, a
// project bro configuration file, a deletion. The checkout-owner collector
// polls for pending mutations over the authenticated producer channel,
// applies them byte-for-byte, and acks. All schema intelligence stays
// daemon-side; the collector is a dumb writer. The path constraint below is
// the safety boundary: unguarded mutations only ever touch committed `.bbox/`
// state, and guarded mutations only ever touch the closed set of project
// configuration targets, never arbitrary checkout files.
// ---------------------------------------------------------------------------

pub const CHECKOUT_MUTATION_SCHEMA_VERSION: u32 = 1;
pub const MAX_CHECKOUT_MUTATIONS_PER_POLL: usize = 64;
pub const MAX_CHECKOUT_MUTATION_PATH_BYTES: usize = 1024;
pub const MAX_CHECKOUT_MUTATION_CONTENT_BYTES: usize = 256 * 1024;
pub const MAX_CHECKOUT_MUTATION_REASON_BYTES: usize = 2048;
/// Longest project configuration name (brofile or teamplate stem).
pub const MAX_PROJECT_CONFIG_NAME_BYTES: usize = 128;

/// Request header a collector sends on the mutation poll to declare which
/// mutation shapes it can apply. A header rather than a body field because
/// the poll body denies unknown fields: an older daemon ignores the header
/// and keeps serving legacy mutations, and an older collector never sends it,
/// so the daemon withholds guarded mutations instead of delivering a shape
/// the collector cannot decode.
pub const CHECKOUT_MUTATION_CAPABILITIES_HEADER: &str = "x-bbox-checkout-mutation-capabilities";
/// Capability token for [`CheckoutMutationGuardV1`] preconditions.
pub const CHECKOUT_MUTATION_CAPABILITY_GUARDED_V1: &str = "guarded-v1";

/// Whether a capabilities header value advertises guarded mutations.
pub fn capabilities_support_guarded(header_value: &str) -> bool {
    header_value
        .split(',')
        .any(|token| token.trim() == CHECKOUT_MUTATION_CAPABILITY_GUARDED_V1)
}

/// One repo-owned project configuration file the guarded mutation lane may
/// write or delete. Paths are relative to the published scope root, the same
/// root the checkout owner joins every mutation path onto.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectConfigTargetV1 {
    /// `.bro/brofiles/<name>.json`
    Brofile(String),
    /// `.bro/teamplates/<name>.json`
    Teamplate(String),
    /// `.bbox/mcp.json`
    McpStore,
}

pub const PROJECT_MCP_STORE_PATH: &str = ".bbox/mcp.json";
/// The committed project config. A read dependency of the configuration
/// view (MCP enablement), never a mutation target.
pub const PROJECT_CONFIG_TOML_PATH: &str = ".bbox/config.toml";
const PROJECT_BROFILES_DIR: &str = ".bro/brofiles/";
const PROJECT_TEAMPLATES_DIR: &str = ".bro/teamplates/";

impl ProjectConfigTargetV1 {
    /// Scope-root-relative path of this target.
    pub fn relative_path(&self) -> String {
        match self {
            Self::Brofile(name) => format!("{PROJECT_BROFILES_DIR}{name}.json"),
            Self::Teamplate(name) => format!("{PROJECT_TEAMPLATES_DIR}{name}.json"),
            Self::McpStore => PROJECT_MCP_STORE_PATH.to_string(),
        }
    }

    /// Classify a scope-root-relative path. Only the exact target shapes
    /// classify: nested directories, other extensions, hidden names and
    /// arbitrary `.bro/` or `.bbox/` files do not.
    pub fn from_relative_path(path: &str) -> Option<Self> {
        if validate_relative_path(path).is_err() {
            return None;
        }
        if path == PROJECT_MCP_STORE_PATH {
            return Some(Self::McpStore);
        }
        let named = |prefix: &str| {
            let stem = path.strip_prefix(prefix)?.strip_suffix(".json")?;
            validate_project_config_name(stem).ok()?;
            Some(stem.to_string())
        };
        if let Some(name) = named(PROJECT_BROFILES_DIR) {
            return Some(Self::Brofile(name));
        }
        named(PROJECT_TEAMPLATES_DIR).map(Self::Teamplate)
    }
}

/// A project configuration name is one plain path component: non-empty,
/// bounded, not hidden, and free of separators and controls.
pub fn validate_project_config_name(name: &str) -> Result<(), ContractError> {
    if name.is_empty()
        || name.len() > MAX_PROJECT_CONFIG_NAME_BYTES
        || name.starts_with('.')
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
        || validate_relative_path(name).is_err()
    {
        return Err(ContractError::InvalidCheckoutMutationField(
            "project configuration name must be one plain, non-hidden path component",
        ));
    }
    Ok(())
}

/// Exact-byte precondition on a guarded mutation.
///
/// `expected_sha256` is the SHA-256 of the bytes the owner must find at the
/// path before applying, or `None` when the path must be absent. It is derived
/// from the immediate predecessor in this path's chain (the accepted bytes
/// when there is none), never from the owner's current file. `predecessor`
/// names that immediate predecessor so the owner and daemon can refuse to let
/// a successor bypass it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationGuardV1 {
    pub expected_sha256: Option<String>,
    pub predecessor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationV1 {
    pub schema_version: u32,
    /// `cm-<16hex>`, minted by the daemon at enqueue time.
    pub mutation_id: String,
    /// The published scope whose checkout receives the write.
    pub scope: PublishedScope,
    /// Scope-root-relative path: below `.bbox/` for unguarded mutations, a
    /// [`ProjectConfigTargetV1`] path for guarded ones.
    pub relative_path: String,
    /// `write` (exact bytes in `content_json`) or `delete`.
    pub mode: String,
    pub content_json: Option<String>,
    /// Short agent/operator-facing provenance (which tool call enqueued).
    pub reason: String,
    pub enqueued_at: String,
    /// Present exactly on guarded configuration mutations. Legacy knowledge
    /// and gap mutations omit it, so their encoding is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<CheckoutMutationGuardV1>,
}

impl CheckoutMutationV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CHECKOUT_MUTATION_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        validate_scope(&self.scope)?;
        validate_checkout_mutation_id(&self.mutation_id, "mutation_id")?;
        if self.relative_path.len() > MAX_CHECKOUT_MUTATION_PATH_BYTES
            || validate_relative_path(&self.relative_path).is_err()
        {
            return Err(ContractError::InvalidCheckoutMutationField(
                "relative_path must be a clean scope-relative path",
            ));
        }
        let target = ProjectConfigTargetV1::from_relative_path(&self.relative_path);
        match &self.guard {
            None => {
                // Configuration targets are guarded-only: an unguarded write
                // there would be a downgraded, precondition-free overwrite.
                // The committed project identity is never a lane target.
                if !self.relative_path.starts_with(".bbox/")
                    || target.is_some()
                    || self.relative_path == PROJECT_CONFIG_TOML_PATH
                {
                    return Err(ContractError::InvalidCheckoutMutationField(
                        "relative_path must be a clean path below .bbox/ outside the guarded configuration targets",
                    ));
                }
            }
            Some(guard) => {
                if target.is_none() {
                    return Err(ContractError::InvalidCheckoutMutationField(
                        "guarded relative_path must be a project configuration target",
                    ));
                }
                if let Some(expected) = &guard.expected_sha256 {
                    validate_sha256(expected)?;
                } else if self.mode == "delete" {
                    return Err(ContractError::InvalidCheckoutMutationField(
                        "guarded delete requires an expected present file",
                    ));
                }
                if let Some(predecessor) = &guard.predecessor {
                    validate_checkout_mutation_id(predecessor, "guard.predecessor")?;
                    if predecessor == &self.mutation_id {
                        return Err(ContractError::InvalidCheckoutMutationField(
                            "guard.predecessor",
                        ));
                    }
                }
            }
        }
        match self.mode.as_str() {
            "write" => match &self.content_json {
                Some(content) if content.len() <= MAX_CHECKOUT_MUTATION_CONTENT_BYTES => {}
                _ => {
                    return Err(ContractError::InvalidCheckoutMutationField(
                        "write mode requires content_json within the byte cap",
                    ));
                }
            },
            "delete" => {
                if self.content_json.is_some() {
                    return Err(ContractError::InvalidCheckoutMutationField(
                        "delete mode carries no content_json",
                    ));
                }
            }
            _ => return Err(ContractError::InvalidCheckoutMutationField("mode")),
        }
        if self.reason.is_empty() || self.reason.len() > MAX_CHECKOUT_MUTATION_REASON_BYTES {
            return Err(ContractError::InvalidCheckoutMutationField("reason"));
        }
        if self.enqueued_at.is_empty() {
            return Err(ContractError::InvalidCheckoutMutationField("enqueued_at"));
        }
        Ok(())
    }

    /// SHA-256 of the bytes this mutation leaves at its path, or `None` for
    /// a delete. A guarded owner that already finds this state recognizes a
    /// redelivered, already-applied mutation.
    pub fn target_sha256(&self) -> Option<String> {
        self.content_json
            .as_deref()
            .map(|content| format!("{:x}", Sha256::digest(content.as_bytes())))
    }
}

fn validate_checkout_mutation_id(value: &str, field: &'static str) -> Result<(), ContractError> {
    if value.len() != 19
        || !value.starts_with("cm-")
        || !value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ContractError::InvalidCheckoutMutationField(field));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationPollRequestV1 {
    pub schema_version: u32,
}

impl CheckoutMutationPollRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CHECKOUT_MUTATION_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationPollResponseV1 {
    /// Pending mutations whose scopes the producer grant covers, oldest
    /// first, capped at MAX_CHECKOUT_MUTATIONS_PER_POLL. A guarded path
    /// contributes at most its oldest undelivered mutation.
    pub mutations: Vec<CheckoutMutationV1>,
    /// Pending mutations behind the cap, outside the grant, waiting on a
    /// predecessor, or needing a capability the collector did not declare,
    /// for observability (the producer re-polls next cycle).
    pub deferred: u64,
}

pub const CHECKOUT_MUTATION_OUTCOME_APPLIED: &str = "applied";
pub const CHECKOUT_MUTATION_OUTCOME_FAILED: &str = "failed";
/// A guarded mutation whose precondition did not match the owner's current
/// file. The owner left its bytes untouched.
pub const CHECKOUT_MUTATION_OUTCOME_CONFLICTED: &str = "conflicted";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationAckRequestV1 {
    pub schema_version: u32,
    pub mutation_id: String,
    /// `applied`, `failed`, or (guarded mutations only) `conflicted`.
    pub outcome: String,
    pub error: Option<String>,
    /// sha256 of the bytes written, when outcome is `applied` on a write.
    pub content_sha256: Option<String>,
    /// For `conflicted`: the SHA-256 of the owner's current bytes, or `None`
    /// when the path was absent. Omitted on every other outcome, so legacy
    /// acks keep their encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_sha256: Option<String>,
}

impl CheckoutMutationAckRequestV1 {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != CHECKOUT_MUTATION_SCHEMA_VERSION {
            return Err(ContractError::UnsupportedSchema(self.schema_version));
        }
        if !matches!(
            self.outcome.as_str(),
            CHECKOUT_MUTATION_OUTCOME_APPLIED
                | CHECKOUT_MUTATION_OUTCOME_FAILED
                | CHECKOUT_MUTATION_OUTCOME_CONFLICTED
        ) {
            return Err(ContractError::InvalidCheckoutMutationField("outcome"));
        }
        if let Some(error) = &self.error {
            if error.len() > MAX_CHECKOUT_MUTATION_REASON_BYTES {
                return Err(ContractError::InvalidCheckoutMutationField("error"));
            }
        }
        if let Some(digest) = &self.content_sha256 {
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(ContractError::InvalidDigest);
            }
        }
        if let Some(digest) = &self.observed_sha256 {
            if self.outcome != CHECKOUT_MUTATION_OUTCOME_CONFLICTED {
                return Err(ContractError::InvalidCheckoutMutationField(
                    "observed_sha256 applies only to conflicted outcomes",
                ));
            }
            validate_sha256(digest)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckoutMutationAckResponseV1 {
    /// `applied`, `failed`, `conflicted`, `already_settled` (duplicate
    /// terminal ack), or `unknown_mutation`.
    pub status: String,
}

pub fn validate_scope(scope: &PublishedScope) -> Result<(), ContractError> {
    if scope.repo_id().trim().is_empty()
        || scope.repo_id().trim() != scope.repo_id()
        || scope.repo_id().len() > 256
        || scope.repo_id().bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ContractError::InvalidScope("repo_id".into()));
    }
    if scope.bbox_root_relpath() == "." {
        return Ok(());
    }
    validate_relative_path(scope.bbox_root_relpath())
        .map_err(|_| ContractError::InvalidScope("bbox_root_relpath".into()))
}

pub fn validate_producer_id(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ContractError::InvalidProducerId);
    }
    Ok(())
}

pub fn validate_relative_path(value: &str) -> Result<(), ContractError> {
    if value.is_empty()
        || value == "."
        || value.len() > MAX_RELATIVE_PATH_BYTES
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|byte| *byte == b':' && value.as_bytes()[0].is_ascii_alphabetic())
        || value.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || component.len() > MAX_PATH_COMPONENT_BYTES
        })
        || value.contains('\\')
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ContractError::InvalidRelativePath(value.into()));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(ContractError::InvalidRelativePath(value.into()));
    }
    for component in path.components() {
        let Component::Normal(component) = component else {
            return Err(ContractError::InvalidRelativePath(value.into()));
        };
        let component = component
            .to_str()
            .ok_or_else(|| ContractError::InvalidRelativePath(value.into()))?;
        if component.is_empty() || component.len() > MAX_PATH_COMPONENT_BYTES {
            return Err(ContractError::InvalidRelativePath(value.into()));
        }
    }
    Ok(())
}

pub fn validate_sha256(value: &str) -> Result<(), ContractError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(ContractError::InvalidDigest)
    }
}

fn validate_git_commit(value: &str) -> Result<(), ContractError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(ContractError::InvalidDigest)
    }
}

pub fn validate_manifest(
    entries: &[ManifestEntry],
    max_files: u64,
    max_logical_bytes: u64,
) -> Result<(), ContractError> {
    let file_count = entries.len() as u64;
    if file_count > max_files {
        return Err(ContractError::TooManyFiles {
            actual: file_count,
            limit: max_files,
        });
    }
    let mut previous: Option<&str> = None;
    let mut logical_bytes = 0_u64;
    for entry in entries {
        entry.validate()?;
        if let Some(previous) = previous {
            if entry.relative_path == previous {
                return Err(ContractError::DuplicatePath(entry.relative_path.clone()));
            }
            if entry.relative_path.as_str() < previous {
                return Err(ContractError::ManifestNotSorted);
            }
        }
        previous = Some(&entry.relative_path);
        logical_bytes =
            logical_bytes
                .checked_add(entry.size)
                .ok_or(ContractError::TooManyBytes {
                    actual: u64::MAX,
                    limit: max_logical_bytes,
                })?;
        if logical_bytes > max_logical_bytes {
            return Err(ContractError::TooManyBytes {
                actual: logical_bytes,
                limit: max_logical_bytes,
            });
        }
    }
    Ok(())
}

pub fn is_skipped_component(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

pub fn max_bytes_for_path(path: &Path) -> Option<u64> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "pdf" | "docx" | "pptx" | "xlsx" | "xlsm" | "xlam" | "xlsb" | "xls" | "ods"
    ) {
        return Some(MAX_DOCUMENT_FILE_BYTES);
    }
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        return Some(MAX_IMAGE_FILE_BYTES);
    }
    is_supported_extension(&extension).then_some(MAX_TEXT_FILE_BYTES)
}

pub fn is_supported_source_path(path: &Path) -> bool {
    max_bytes_for_path(path).is_some()
}

fn is_supported_extension(extension: &str) -> bool {
    matches!(
        extension,
        "md" | "markdown"
            | "mdown"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "txt"
            | "text"
            | "log"
            | "ipynb"
            | "vtt"
            | "srt"
            | "xhtml"
            | "rs"
            | "py"
            | "cs"
            | "java"
            | "go"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hh"
            | "hpp"
            | "hxx"
            | "erl"
            | "hrl"
            | "ex"
            | "exs"
            | "rb"
            | "ml"
            | "mli"
            | "hs"
            | "swift"
            | "kt"
            | "scala"
            | "lua"
            | "sh"
            | "bash"
            | "html"
            | "htm"
            | "css"
            | "sql"
    )
}

pub fn manifest_sha256(entries: &[ManifestEntry]) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-code-source-manifest-v1");
    for entry in entries {
        put_field(&mut hasher, entry.relative_path.as_bytes());
        put_field(&mut hasher, entry.content_sha256.as_bytes());
        hasher.update(entry.size.to_be_bytes());
    }
    hex::encode(hasher.finalize())
}

pub fn dirty_fingerprint(head_commit: &str, entries: &[ManifestEntry]) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-code-source-dirty-v1");
    put_field(&mut hasher, head_commit.as_bytes());
    put_field(&mut hasher, manifest_sha256(entries).as_bytes());
    hex::encode(hasher.finalize())
}

pub fn generation_id(producer_id: &str, descriptor: &GenerationDescriptor) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-code-source-generation-v1");
    put_field(&mut hasher, producer_id.as_bytes());
    put_scope(&mut hasher, &descriptor.scope);
    put_field(&mut hasher, descriptor.walker_policy_version.as_bytes());
    put_field(&mut hasher, descriptor.head_commit.as_bytes());
    put_field(&mut hasher, descriptor.dirty_fingerprint.as_bytes());
    put_field(&mut hasher, descriptor.manifest_sha256.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn scope_hash(scope: &PublishedScope) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-code-source-scope-v1");
    put_scope(&mut hasher, scope);
    hex::encode(hasher.finalize())
}

pub fn source_selector(project_id: &str, generation_id: &str) -> String {
    format!("collected:{project_id}:{generation_id}")
}

/// Validate the historical selector shape without deriving its materialization
/// suffix from the current runtime version.
pub fn validate_collected_materialization_selector(
    project_id: &str,
    generation_id: &str,
    selector: &str,
) -> Result<(), ContractError> {
    let prefix = format!("{}:m", source_selector(project_id, generation_id));
    let Some(suffix) = selector.strip_prefix(&prefix) else {
        return Err(ContractError::InvalidCollectedMaterializationSelector);
    };
    if suffix.len() != 16
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(ContractError::InvalidCollectedMaterializationSelector);
    }
    Ok(())
}

pub fn local_selector(project_id: &str) -> String {
    format!("local:{project_id}")
}

pub fn source_entry_key(selector: &str, relative_path: &str) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-code-source-entry-v1");
    put_field(&mut hasher, selector.as_bytes());
    put_field(&mut hasher, relative_path.as_bytes());
    hex::encode(hasher.finalize())
}

/// Source kind for a materialization selector, as the stored `source_kind`
/// document field and the `FileMeta` composite key carry it (Phase 3 plan
/// section 4.6). Derived from the selector rather than stored a second time
/// so a document's `source_kind` and its selector can never disagree; the
/// two selector constructors above are the only shapes that exist.
pub const SOURCE_KIND_LOCAL: &str = "local";
pub const SOURCE_KIND_COLLECTED: &str = "collected";
pub const SOURCE_KIND_UNKNOWN: &str = "unknown";

pub fn source_kind_for_selector(selector: &str) -> &'static str {
    if selector.starts_with("local:") {
        SOURCE_KIND_LOCAL
    } else if selector.starts_with("collected:") {
        SOURCE_KIND_COLLECTED
    } else {
        SOURCE_KIND_UNKNOWN
    }
}

/// The `FileMeta` map key for one project-file freshness row.
///
/// NUL-delimited so no component can forge a boundary: relative paths reject
/// NUL and control bytes in [`validate_relative_path`], and project ids and
/// source kinds are both NUL-free by construction. The `pf` tag keeps the
/// project lane disjoint from the absolute-path keys transcripts, adapter
/// rows, and the `git:<project_id>` history source key still use, which is
/// what lets the purge loops split their delete arms on the key itself.
pub fn project_file_meta_key(project_id: &str, source_kind: &str, relative_path: &str) -> String {
    format!("pf\0{project_id}\0{source_kind}\0{relative_path}")
}

/// Split a key produced by [`project_file_meta_key`] back into its parts.
/// `None` for any key from another lane, which is exactly the discriminator
/// the purge loops and the edge-purge relative-path read need.
pub fn parse_project_file_meta_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.split('\0');
    if parts.next()? != "pf" {
        return None;
    }
    let project_id = parts.next()?;
    let source_kind = parts.next()?;
    let relative_path = parts.next()?;
    if parts.next().is_some() || project_id.is_empty() || relative_path.is_empty() {
        return None;
    }
    Some((project_id, source_kind, relative_path))
}

const SOURCE_URI_PREFIX: &str = "bbox://project/";

/// Bounded defense-in-depth check on the `source_uri` codec's own boundary:
/// non-empty, and free of `/`, `%`, and control bytes, so a `project_id`
/// this codec embeds unencoded can never be mistaken for a path separator
/// or a percent-escape by a reader, and never smuggles a raw control byte
/// into a rendered URI. This is intentionally not `ProjectId::parse`'s full
/// charset (this crate does not depend on the catalog crate's id type):
/// callers that hold a real `ProjectId` already satisfy this bound, and
/// callers that don't still get a safe URI shape out of this codec.
fn valid_source_uri_project_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte != b'/' && byte != b'%' && !byte.is_ascii_control())
}

/// Render the stable, machine-facing `source_uri` for a project-relative
/// path (durable-project-catalog governing section 10.2, Phase 3 plan
/// section 5). Validates the relative path first, then percent-encodes each
/// `/`-delimited segment's UTF-8 bytes: ASCII alphanumerics and `- . _ ~`
/// pass through unencoded, everything else becomes `%XX` with uppercase hex.
/// No Unicode normalization is applied. `project_id` is embedded unencoded
/// (its own catalog-level charset already excludes `/`, `%`, and control
/// bytes) and must be non-empty and slash-free.
pub fn encode_source_uri(project_id: &str, relative_path: &str) -> Result<String, ContractError> {
    if !valid_source_uri_project_id(project_id) {
        return Err(ContractError::InvalidSourceUri);
    }
    validate_relative_path(relative_path)?;
    let mut encoded = String::with_capacity(relative_path.len());
    for (index, segment) in relative_path.split('/').enumerate() {
        if index > 0 {
            encoded.push('/');
        }
        percent_encode_segment(segment, &mut encoded);
    }
    Ok(format!("{SOURCE_URI_PREFIX}{project_id}/{encoded}"))
}

/// Parse a `source_uri` rendered by [`encode_source_uri`] back into
/// `(project_id, relative_path)`. Decodes each segment's percent-escapes
/// exactly once, rejects an escape that decodes to `/` or `\`, then requires
/// canonical re-encoding equality: any non-canonical encoding (wrong case,
/// an unnecessarily escaped unreserved character, or a lowercase hex digit)
/// fails closed here rather than being silently normalized. The decoded
/// relative path is validated the same way `encode_source_uri` validates
/// its input.
pub fn decode_source_uri(uri: &str) -> Result<(String, String), ContractError> {
    let rest = uri
        .strip_prefix(SOURCE_URI_PREFIX)
        .ok_or(ContractError::InvalidSourceUri)?;
    let (project_id, encoded_path) = rest
        .split_once('/')
        .ok_or(ContractError::InvalidSourceUri)?;
    if !valid_source_uri_project_id(project_id) {
        return Err(ContractError::InvalidSourceUri);
    }
    let mut segments = Vec::new();
    for encoded_segment in encoded_path.split('/') {
        let decoded_segment =
            percent_decode_segment(encoded_segment).ok_or(ContractError::InvalidSourceUri)?;
        if decoded_segment.contains('/') || decoded_segment.contains('\\') {
            return Err(ContractError::InvalidSourceUri);
        }
        segments.push(decoded_segment);
    }
    let decoded_path = segments.join("/");
    validate_relative_path(&decoded_path)?;
    let re_encoded = encode_source_uri(project_id, &decoded_path)?;
    if re_encoded != uri {
        return Err(ContractError::InvalidSourceUri);
    }
    Ok((project_id.to_string(), decoded_path))
}

fn percent_encode_segment(segment: &str, out: &mut String) {
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit_upper(byte >> 4));
            out.push(hex_digit_upper(byte & 0x0f));
        }
    }
}

fn hex_digit_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

fn percent_decode_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_digit_value(*bytes.get(index + 1)?)?;
            let low = hex_digit_value(*bytes.get(index + 2)?)?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn put_scope(hasher: &mut Sha256, scope: &PublishedScope) {
    put_field(hasher, scope.repo_id().as_bytes());
    put_field(hasher, scope.bbox_root_relpath().as_bytes());
}

fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-family", ".").unwrap()
    }

    fn valid_onboard_request() -> CatalogOnboardRequestV1 {
        CatalogOnboardRequestV1 {
            schema_version: CATALOG_ONBOARD_SCHEMA_VERSION,
            scope: scope(),
            producer_checkout_dir: "/home/operator/repos/example".into(),
            producer_project_dir: "/home/operator/repos/example".into(),
            project_root_relpath: ".".into(),
            checkout_kind: "base".into(),
            checkout_id: "a".repeat(32),
            branch_ref: Some("refs/heads/main".into()),
            committed_repo_id: Some("repo-family".into()),
            declared_aliases: vec!["example".into()],
            capabilities: bbox_corpus_core::project_catalog::AttachmentCapabilities {
                local_code_source: true,
                git_history: true,
                blame: true,
                repo_knowledge: true,
                repo_mutation: true,
                render_output: true,
                provenance_note_io: true,
                artifact_watching: true,
            },
        }
    }

    #[test]
    fn catalog_onboard_request_accepts_a_valid_probe() {
        valid_onboard_request().validate().unwrap();
    }

    fn valid_checkout_mutation() -> CheckoutMutationV1 {
        CheckoutMutationV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: "cm-0123456789abcdef".into(),
            scope: scope(),
            relative_path: ".bbox/gaps/gap-0123abcd.json".into(),
            mode: "write".into(),
            content_json: Some("{\"id\":\"gap-0123abcd\"}".into()),
            reason: "bbox_gap(scope=project) via checkout-owner lane".into(),
            enqueued_at: "2026-08-12T00:00:00Z".into(),
            guard: None,
        }
    }

    fn guarded_config_mutation(relative_path: &str) -> CheckoutMutationV1 {
        CheckoutMutationV1 {
            relative_path: relative_path.into(),
            content_json: Some("{\"name\":\"reviewer\"}".into()),
            reason: "bro_brofile(scope=project) via checkout-owner lane".into(),
            guard: Some(CheckoutMutationGuardV1 {
                expected_sha256: None,
                predecessor: None,
            }),
            ..valid_checkout_mutation()
        }
    }

    fn valid_presence() -> ProducerPresenceV1 {
        ProducerPresenceV1 {
            enroll_roots: vec!["/home/operator/repos".into()],
            host_label: "checkout-host-a".into(),
            config_path: "/etc/blackbox/code-collector.toml".into(),
            service_label: Some("collector-a".into()),
            collector_version: "0.0.1".into(),
        }
    }

    fn valid_enroll_receipt() -> EnrollReceiptV1 {
        EnrollReceiptV1 {
            project_id: Some("p_00000000000000000000000000000001".into()),
            attachment_id: Some("pa_000000000000000000000000000001".into()),
            created_project: true,
            already_attached: false,
            scope: scope(),
            published_ref: "refs/heads/main".into(),
            identity_committed: false,
            commit_paths: vec![
                ".bbox/config.toml".into(),
                ".bbox/mcp.json".into(),
                ".bbox/local/.gitignore".into(),
            ],
            onboard_error: None,
        }
    }

    #[test]
    fn producer_command_contract_accepts_valid_poll_command_and_ack() {
        ProducerCommandPollRequestV1 {
            schema_version: PRODUCER_COMMAND_SCHEMA_VERSION,
            presence: valid_presence(),
        }
        .validate()
        .unwrap();
        ProducerCommandPollResponseV1 {
            commands: vec![ProducerCommandV1 {
                command_id: "pc-0123456789abcdef".into(),
                kind: "enroll".into(),
                path: "/home/operator/repos/example".into(),
                full_ref: Some("refs/heads/main".into()),
            }],
        }
        .validate()
        .unwrap();
        ProducerCommandAckRequestV1 {
            command_id: "pc-0123456789abcdef".into(),
            outcome: "applied".into(),
            receipt: Some(valid_enroll_receipt()),
            error: None,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn producer_command_contract_rejects_relative_paths_and_payload_mismatches() {
        let mut relative_presence = valid_presence();
        relative_presence.enroll_roots = vec!["repos".into()];
        assert!(relative_presence.validate().is_err());

        let mut relative_config = valid_presence();
        relative_config.config_path = "collector.toml".into();
        assert!(relative_config.validate().is_err());

        let mut bad_command = ProducerCommandV1 {
            command_id: "pc-0123456789abcdef".into(),
            kind: "enroll".into(),
            path: "repos/example".into(),
            full_ref: None,
        };
        assert!(bad_command.validate().is_err());
        bad_command.path = "/repos/example".into();
        bad_command.kind = "delete".into();
        assert!(bad_command.validate().is_err());

        let mismatched_ack = ProducerCommandAckRequestV1 {
            command_id: "pc-0123456789abcdef".into(),
            outcome: "failed".into(),
            receipt: Some(valid_enroll_receipt()),
            error: None,
        };
        assert!(mismatched_ack.validate().is_err());
    }

    #[test]
    fn producer_command_contract_enforces_string_and_list_bounds() {
        let mut too_many_roots = valid_presence();
        too_many_roots.enroll_roots =
            vec!["/home/operator/repos".into(); MAX_PRODUCER_ENROLL_ROOTS + 1];
        assert!(too_many_roots.validate().is_err());

        let mut long_label = valid_presence();
        long_label.host_label = "x".repeat(MAX_PRODUCER_COMMAND_LABEL_BYTES + 1);
        assert!(long_label.validate().is_err());

        let response = ProducerCommandPollResponseV1 {
            commands: vec![
                ProducerCommandV1 {
                    command_id: "pc-0123456789abcdef".into(),
                    kind: "enroll".into(),
                    path: "/home/operator/repos/example".into(),
                    full_ref: None,
                };
                MAX_PRODUCER_COMMANDS_PER_POLL + 1
            ],
        };
        assert!(response.validate().is_err());

        let mut too_many_paths = valid_enroll_receipt();
        too_many_paths.commit_paths =
            vec![".bbox/config.toml".into(); MAX_ENROLL_RECEIPT_COMMIT_PATHS + 1];
        assert!(too_many_paths.validate().is_err());

        let failed = ProducerCommandAckRequestV1 {
            command_id: "pc-fedcba9876543210".into(),
            outcome: "failed".into(),
            receipt: None,
            error: Some(ProducerCommandErrorV1 {
                code: "enroll_failed".into(),
                message: "x".repeat(MAX_PRODUCER_COMMAND_ERROR_BYTES + 1),
            }),
        };
        assert!(failed.validate().is_err());
    }

    #[test]
    fn checkout_mutation_accepts_a_valid_write() {
        valid_checkout_mutation().validate().unwrap();
        let mut delete = valid_checkout_mutation();
        delete.mode = "delete".into();
        delete.content_json = None;
        delete.validate().unwrap();
    }

    #[test]
    fn checkout_mutation_rejects_paths_outside_bbox_state() {
        for bad in [
            "src/main.rs",
            ".bbox",
            ".bbox/../Cargo.toml",
            "/abs/.bbox/gaps/gap-x.json",
            ".bbox/gaps/../escape",
        ] {
            let mut mutation = valid_checkout_mutation();
            mutation.relative_path = bad.into();
            assert!(mutation.validate().is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn project_config_targets_classify_exactly_the_three_shapes() {
        for (path, target) in [
            (
                ".bro/brofiles/reviewer.json",
                ProjectConfigTargetV1::Brofile("reviewer".into()),
            ),
            (
                ".bro/brofiles/code-reviewer_v2.json",
                ProjectConfigTargetV1::Brofile("code-reviewer_v2".into()),
            ),
            (
                ".bro/teamplates/squad.json",
                ProjectConfigTargetV1::Teamplate("squad".into()),
            ),
            (".bbox/mcp.json", ProjectConfigTargetV1::McpStore),
        ] {
            assert_eq!(
                ProjectConfigTargetV1::from_relative_path(path),
                Some(target.clone()),
                "{path}"
            );
            assert_eq!(target.relative_path(), path);
        }
        for path in [
            ".bro/brofiles/nested/reviewer.json",
            ".bro/brofiles/.hidden.json",
            ".bro/brofiles/.json",
            ".bro/brofiles/reviewer.toml",
            ".bro/brofiles/reviewer",
            ".bro/brofiles/../mcp.json",
            ".bro/teamplates/a/b.json",
            ".bro/mcp.json",
            ".bro/config.json",
            ".bro/accounts.json",
            ".bbox/config.toml",
            ".bbox/knowledge/abc.json",
            "sub/.bbox/mcp.json",
            "/abs/.bbox/mcp.json",
            "../.bbox/mcp.json",
            ".bbox/mcp.json/",
            ".bro\\brofiles\\x.json",
            &format!(
                ".bro/brofiles/{}.json",
                "n".repeat(MAX_PROJECT_CONFIG_NAME_BYTES + 1)
            ),
        ] {
            assert_eq!(
                ProjectConfigTargetV1::from_relative_path(path),
                None,
                "{path} must not classify as a configuration target"
            );
        }
    }

    #[test]
    fn guarded_mutations_accept_only_configuration_targets() {
        for path in [
            ".bro/brofiles/reviewer.json",
            ".bro/teamplates/squad.json",
            ".bbox/mcp.json",
        ] {
            guarded_config_mutation(path).validate().unwrap();
        }
        for path in [
            ".bbox/config.toml",
            ".bbox/gaps/gap-0123abcd.json",
            ".bro/brofiles/nested/reviewer.json",
            ".bro/other/reviewer.json",
            "src/main.rs",
            "/abs/.bro/brofiles/reviewer.json",
            ".bro/brofiles/../../escape.json",
        ] {
            assert!(
                guarded_config_mutation(path).validate().is_err(),
                "guarded {path} must be rejected"
            );
        }
        let mut bad_expected = guarded_config_mutation(".bbox/mcp.json");
        bad_expected.guard.as_mut().unwrap().expected_sha256 = Some("A".repeat(64));
        assert!(bad_expected.validate().is_err());
        let mut absent_delete = guarded_config_mutation(".bbox/mcp.json");
        absent_delete.mode = "delete".into();
        absent_delete.content_json = None;
        assert!(
            absent_delete.validate().is_err(),
            "a guarded delete must name the present bytes it removes"
        );
        absent_delete.guard.as_mut().unwrap().expected_sha256 = Some("a".repeat(64));
        absent_delete.validate().unwrap();
        let mut self_predecessor = guarded_config_mutation(".bbox/mcp.json");
        self_predecessor.guard.as_mut().unwrap().predecessor =
            Some(self_predecessor.mutation_id.clone());
        assert!(self_predecessor.validate().is_err());
        let mut bad_predecessor = guarded_config_mutation(".bbox/mcp.json");
        bad_predecessor.guard.as_mut().unwrap().predecessor = Some("cm-nothex".into());
        assert!(bad_predecessor.validate().is_err());
    }

    #[test]
    fn unguarded_mutations_never_reach_configuration_targets() {
        for path in [
            ".bbox/mcp.json",
            ".bbox/config.toml",
            ".bro/brofiles/reviewer.json",
            ".bro/teamplates/squad.json",
        ] {
            let mut mutation = valid_checkout_mutation();
            mutation.relative_path = path.into();
            assert!(
                mutation.validate().is_err(),
                "unguarded {path} would be a precondition-free overwrite"
            );
        }
        for path in [
            ".bbox/knowledge/0123456789abcdef.json",
            ".bbox/gaps/gap-0123abcd.json",
        ] {
            let mut mutation = valid_checkout_mutation();
            mutation.relative_path = path.into();
            mutation.validate().unwrap();
        }
    }

    #[test]
    fn legacy_mutation_and_ack_encodings_are_unchanged() {
        let legacy = serde_json::to_value(valid_checkout_mutation()).unwrap();
        assert!(legacy.get("guard").is_none());
        let decoded: CheckoutMutationV1 = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "mutation_id": "cm-0123456789abcdef",
            "scope": serde_json::to_value(scope()).unwrap(),
            "relative_path": ".bbox/gaps/gap-0123abcd.json",
            "mode": "write",
            "content_json": "{}",
            "reason": "legacy",
            "enqueued_at": "2026-08-12T00:00:00Z",
        }))
        .unwrap();
        assert_eq!(decoded.guard, None);
        decoded.validate().unwrap();
        let ack = CheckoutMutationAckRequestV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: "cm-0123456789abcdef".into(),
            outcome: CHECKOUT_MUTATION_OUTCOME_APPLIED.into(),
            error: None,
            content_sha256: None,
            observed_sha256: None,
        };
        assert!(
            serde_json::to_value(&ack)
                .unwrap()
                .get("observed_sha256")
                .is_none()
        );
        // A guarded mutation carries its explicit absence assertion as null.
        let guarded = serde_json::to_value(guarded_config_mutation(".bbox/mcp.json")).unwrap();
        assert_eq!(guarded["guard"]["expected_sha256"], serde_json::Value::Null);
    }

    #[test]
    fn capability_header_parsing_is_exact() {
        assert!(capabilities_support_guarded("guarded-v1"));
        assert!(capabilities_support_guarded("other, guarded-v1"));
        assert!(!capabilities_support_guarded(""));
        assert!(!capabilities_support_guarded("guarded-v2"));
        assert!(!capabilities_support_guarded("guarded-v1x"));
    }

    #[test]
    fn target_sha256_names_the_resulting_state() {
        let write = guarded_config_mutation(".bbox/mcp.json");
        assert_eq!(
            write.target_sha256().unwrap(),
            format!(
                "{:x}",
                Sha256::digest(write.content_json.as_deref().unwrap().as_bytes())
            )
        );
        let mut delete = write.clone();
        delete.mode = "delete".into();
        delete.content_json = None;
        assert_eq!(delete.target_sha256(), None);
    }

    #[test]
    fn checkout_mutation_rejects_mode_content_mismatches() {
        let mut write_without_content = valid_checkout_mutation();
        write_without_content.content_json = None;
        assert!(write_without_content.validate().is_err());

        let mut delete_with_content = valid_checkout_mutation();
        delete_with_content.mode = "delete".into();
        assert!(delete_with_content.validate().is_err());

        let mut unknown_mode = valid_checkout_mutation();
        unknown_mode.mode = "append".into();
        assert!(unknown_mode.validate().is_err());

        let mut bad_id = valid_checkout_mutation();
        bad_id.mutation_id = "0123456789abcdef".into();
        assert!(bad_id.validate().is_err());
    }

    #[test]
    fn checkout_mutation_ack_validates_outcome_and_digest() {
        let valid = CheckoutMutationAckRequestV1 {
            schema_version: CHECKOUT_MUTATION_SCHEMA_VERSION,
            mutation_id: "cm-0123456789abcdef".into(),
            outcome: "applied".into(),
            error: None,
            content_sha256: Some("a".repeat(64)),
            observed_sha256: None,
        };
        valid.validate().unwrap();

        let mut bad_outcome = valid.clone();
        bad_outcome.outcome = "maybe".into();
        assert!(bad_outcome.validate().is_err());

        let mut bad_digest = valid.clone();
        bad_digest.content_sha256 = Some("xyz".into());
        assert!(matches!(
            bad_digest.validate(),
            Err(ContractError::InvalidDigest)
        ));

        let mut conflicted = valid.clone();
        conflicted.outcome = CHECKOUT_MUTATION_OUTCOME_CONFLICTED.into();
        conflicted.content_sha256 = None;
        conflicted.observed_sha256 = Some("b".repeat(64));
        conflicted.validate().unwrap();
        conflicted.observed_sha256 = None;
        conflicted.validate().unwrap();

        let mut observed_on_applied = valid.clone();
        observed_on_applied.observed_sha256 = Some("b".repeat(64));
        assert!(observed_on_applied.validate().is_err());
    }

    #[test]
    fn catalog_onboard_request_rejects_field_violations() {
        let mut bad_schema = valid_onboard_request();
        bad_schema.schema_version += 1;
        assert!(matches!(
            bad_schema.validate(),
            Err(ContractError::UnsupportedSchema(_))
        ));

        let mut relative = valid_onboard_request();
        relative.producer_checkout_dir = "repos/example".into();
        assert!(relative.validate().is_err());

        let mut relpath_mismatch = valid_onboard_request();
        relpath_mismatch.project_root_relpath = "nested".into();
        assert!(relpath_mismatch.validate().is_err());

        let mut bad_kind = valid_onboard_request();
        bad_kind.checkout_kind = "symlink-farm".into();
        assert!(bad_kind.validate().is_err());

        let mut bad_checkout_id = valid_onboard_request();
        bad_checkout_id.checkout_id = "not-hex".into();
        assert!(bad_checkout_id.validate().is_err());

        let mut identity_mismatch = valid_onboard_request();
        identity_mismatch.committed_repo_id = Some("another-repo".into());
        assert!(identity_mismatch.validate().is_err());

        let mut alias_overflow = valid_onboard_request();
        alias_overflow.declared_aliases = (0..=MAX_ONBOARD_ALIASES)
            .map(|index| format!("alias-{index}"))
            .collect();
        assert!(alias_overflow.validate().is_err());
    }

    #[test]
    fn validates_and_hashes_manifest_deterministically() {
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "a".repeat(64),
            size: 12,
        }];
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope: scope(),
            head_commit: "b".repeat(40),
            dirty_fingerprint: dirty_fingerprint(&"b".repeat(40), &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: 1,
            logical_bytes: 12,
        };
        descriptor
            .validate_manifest(
                &entries,
                DEFAULT_MAX_MANIFEST_FILES,
                DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
            )
            .unwrap();
        assert_eq!(generation_id("host-a", &descriptor).len(), 64);
        assert_eq!(source_entry_key("local:p", "src/lib.rs").len(), 64);
        let generation = "a".repeat(64);
        assert!(
            validate_collected_materialization_selector(
                "project-a",
                &generation,
                &format!(
                    "{}:m0123456789abcdef",
                    source_selector("project-a", &generation)
                ),
            )
            .is_ok()
        );
        for invalid in [
            source_selector("project-a", &generation),
            format!(
                "{}:m0123456789abcde",
                source_selector("project-a", &generation)
            ),
            format!(
                "{}:m0123456789abcdeF",
                source_selector("project-a", &generation)
            ),
            format!(
                "{}:m0123456789abcdeg",
                source_selector("project-a", &generation)
            ),
            format!(
                "{}:m0123456789abcdef",
                source_selector("project-b", &generation)
            ),
        ] {
            assert!(
                validate_collected_materialization_selector("project-a", &generation, &invalid)
                    .is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn paths_fail_closed() {
        for invalid in [
            "",
            ".",
            "../x",
            "/tmp/x",
            "a//b",
            "a\\b",
            "C:relative.rs",
            "a/./b.rs",
            "a/../b.rs",
            "a\0b.rs",
            "a\nb.rs",
        ] {
            assert!(validate_relative_path(invalid).is_err(), "{invalid}");
        }
        assert!(validate_relative_path(&format!("{}.rs", "a".repeat(256))).is_err());
        assert!(validate_relative_path(&format!("{}.rs", "a".repeat(4096))).is_err());
        assert!(validate_relative_path("src/main.rs").is_ok());
    }

    #[test]
    fn source_uri_round_trips_reserved_and_non_ascii_names() {
        for relative_path in [
            "src/main.rs",
            "has space/file.rs",
            "100% done.md",
            "notes#1.md",
            "query?.txt",
            "café/日本語.md",
            "a-b_c.d~e.rs",
        ] {
            let uri = encode_source_uri("project-a", relative_path).unwrap();
            let (project_id, decoded) = decode_source_uri(&uri).unwrap();
            assert_eq!(project_id, "project-a");
            assert_eq!(decoded, relative_path, "round trip for {relative_path:?}");
            // Encoding is deterministic: encoding the decoded path again
            // reproduces the exact same URI byte for byte.
            assert_eq!(encode_source_uri("project-a", &decoded).unwrap(), uri);
        }
    }

    #[test]
    fn source_uri_uses_uppercase_hex_and_leaves_slashes_and_unreserved_bytes_alone() {
        let uri = encode_source_uri("p", "a b/c#d").unwrap();
        assert_eq!(uri, "bbox://project/p/a%20b/c%23d");
    }

    #[test]
    fn source_uri_decode_rejects_non_canonical_and_traversal_encodings() {
        for invalid in [
            // Lowercase hex: not the canonical uppercase form this codec emits.
            "bbox://project/p/a%2fb",
            // Over-encoding an unreserved ASCII byte that should be literal.
            "bbox://project/p/%61",
            // Encoded slash: must not be reinterpreted as a path separator.
            "bbox://project/p/a%2Fb",
            // Encoded backslash.
            "bbox://project/p/a%5Cb",
            // Percent-encoded traversal.
            "bbox://project/p/%2E%2E",
            "bbox://project/p/..",
            "bbox://project/p/.",
            "bbox://project/p/",
            "bbox://project/p",
            "bbox://project//a",
            "bbox://project/p/a%",
            "bbox://project/p/a%2",
            "bbox://project/p/a%gg",
            "not-a-source-uri",
            "",
        ] {
            assert!(
                decode_source_uri(invalid).is_err(),
                "{invalid} should be rejected"
            );
        }
    }

    #[test]
    fn source_uri_rejects_empty_or_slash_bearing_project_id() {
        assert!(encode_source_uri("", "a.rs").is_err());
        assert!(encode_source_uri("a/b", "a.rs").is_err());
        assert!(decode_source_uri("bbox://project//a.rs").is_err());
    }

    #[test]
    fn source_uri_rejects_a_project_id_containing_percent_or_control_bytes() {
        assert!(encode_source_uri("a%b", "a.rs").is_err());
        assert!(encode_source_uri("a\nb", "a.rs").is_err());
        assert!(encode_source_uri("a\0b", "a.rs").is_err());
        assert!(decode_source_uri("bbox://project/a%25b/a.rs").is_err());
        assert!(decode_source_uri("bbox://project/a\nb/a.rs").is_err());
    }

    #[test]
    fn policy_matches_document_and_code_caps() {
        assert_eq!(
            max_bytes_for_path(Path::new("a.rs")),
            Some(MAX_TEXT_FILE_BYTES)
        );
        assert_eq!(
            max_bytes_for_path(Path::new("a.pdf")),
            Some(MAX_DOCUMENT_FILE_BYTES)
        );
        assert_eq!(max_bytes_for_path(Path::new("a.bin")), None);
        assert!(is_skipped_component(".bbox"));
        assert!(is_skipped_component("target"));
    }

    #[test]
    fn manifest_validation_rejects_order_duplicates_and_caps() {
        let entry = |path: &str, hash: char, size: u64| ManifestEntry {
            relative_path: path.into(),
            content_sha256: hash.to_string().repeat(64),
            size,
        };
        assert!(matches!(
            validate_manifest(&[entry("b.rs", 'b', 1), entry("a.rs", 'a', 1)], 2, 2),
            Err(ContractError::ManifestNotSorted)
        ));
        assert!(matches!(
            validate_manifest(&[entry("a.rs", 'a', 1), entry("a.rs", 'b', 1)], 2, 2),
            Err(ContractError::DuplicatePath(_))
        ));
        assert!(matches!(
            validate_manifest(&[entry("a.rs", 'a', 1)], 0, 2),
            Err(ContractError::TooManyFiles { .. })
        ));
        assert!(matches!(
            validate_manifest(&[entry("a.rs", 'a', 2)], 1, 1),
            Err(ContractError::TooManyBytes { .. })
        ));
        assert!(entry("a.rs", 'A', 1).validate().is_err());
        assert!(entry("a.bin", 'a', 1).validate().is_err());
        for path in [
            ".bbox/knowledge/x.json",
            ".github/workflows/ci.yml",
            "target/cache.json",
            "src/.generated/value.rs",
        ] {
            assert!(
                matches!(
                    entry(path, 'a', 1).validate(),
                    Err(ContractError::UnsupportedPath(rejected)) if rejected == path
                ),
                "server-side manifest validation accepted skipped path {path}"
            );
        }
        assert!(
            entry("a.rs", 'a', MAX_TEXT_FILE_BYTES + 1)
                .validate()
                .is_err()
        );
    }
}
