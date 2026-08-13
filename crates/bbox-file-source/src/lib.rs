//! Dependency-clean wire contract for connector-sourced files.
//!
//! This is the file-lane sibling of [`bbox_code_source`]: the leaf crate the
//! daemon and the producer satellite (`bbox-file-collector`) both link, so
//! neither side can drift from the other's idea of the manifest. The
//! conversation it describes mounts under `/internal/file-source/v1/*` and
//! mirrors the code lane's shape exactly (begin, paged manifest, complete,
//! missing-hash cursor, blob PUT, finalize, generation status) because the
//! store, blob cache, generation model, and activation behind it are the same
//! code. Route separation exists so producer grants, limits, and health can be
//! reasoned about per lane, not because the transport differs.
//!
//! Three things differ from the code lane, and each is load-bearing:
//!
//! 1. **The scope is a [`ConnectorScope`]**, an operator-minted opaque
//!    `connector_source_id` plus a declared `connector_kind`. There is no
//!    repository, no commit, and nothing the daemon can independently
//!    recompute from the producer's claim.
//! 2. **There is no `dirty_fingerprint` analog.** A remote store is always
//!    dirty; the manifest digest over the ordered entries IS the fingerprint.
//!    `remote_watermark` occupies the slot `head_commit` occupies for code
//!    sources and is explicitly not authority: it is display and diagnostic
//!    only.
//! 3. **Manifest entries carry remote provenance** (`remote_id`,
//!    `remote_version`, and a renderable `remote_url`) from this FIRST wire
//!    version, per the ratified wire decision, so evidence can cite the source
//!    document. The corpus still treats `content_sha256` as the only freshness
//!    authority; vendor version semantics never become index freshness.
//!
//! What this crate deliberately does NOT contain: any connector, any vendor
//! coordinate parsing, any chunker knowledge, and any credential type. The
//! satellite's policy engine and journal live in `bbox-file-collector`; the
//! byte caps and path rules live here because both sides enforce them.

use std::collections::BTreeMap;

use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Wire schema version for every `/internal/file-source/v1/*` payload.
pub const SCHEMA_VERSION: u32 = 1;

/// Producer-side policy version. Skew between satellite and server is a typed
/// rejection rather than a best-effort merge (design section 11), so this
/// constant changes whenever the derivation of a logical path, the byte caps,
/// or the manifest digest changes.
pub const CONNECTOR_POLICY_VERSION: &str = "file-source-connector-policy-v1";

/// Onboarding schema version, versioned independently of the publication
/// conversation so the onboarding shape can move without a manifest bump.
pub const CATALOG_ONBOARD_SCHEMA_VERSION: u32 = 1;

pub const MAX_MANIFEST_PAGE_ENTRIES: usize = 2_000;
pub const MAX_MANIFEST_PAGE_BYTES: usize = 2 * 1024 * 1024;
pub const DEFAULT_MAX_MANIFEST_FILES: u64 = 250_000;
pub const DEFAULT_MAX_MANIFEST_LOGICAL_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// A remote store's own id for an entry. Opaque; never parsed, never a path.
pub const MAX_REMOTE_ID_BYTES: usize = 512;
/// A remote store's version token (etag, revision, changestamp). Opaque.
pub const MAX_REMOTE_VERSION_BYTES: usize = 256;
/// A renderable remote URL. Bounded, scheme-restricted, never fetched by the
/// daemon.
pub const MAX_REMOTE_URL_BYTES: usize = 2048;
/// The opaque, connector-shaped watermark carried for display only.
pub const MAX_REMOTE_WATERMARK_BYTES: usize = 512;
/// Bound on one skip-reason label and on a degradation cause string.
pub const MAX_REASON_BYTES: usize = 128;
/// Bound on the per-reason skip counter map, so a buggy connector cannot
/// present an unbounded reason cardinality as telemetry.
pub const MAX_SKIP_REASONS: usize = 32;
pub const MAX_DISPLAY_NAME_BYTES: usize = 256;
pub const MAX_REMOTE_AUTHORITY_BYTES: usize = 256;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FileSourceError {
    #[error("unsupported file-source schema version {0}")]
    UnsupportedSchema(u32),
    #[error("connector policy mismatch: received {received}, expected {expected}")]
    ConnectorPolicyMismatch {
        received: String,
        expected: &'static str,
    },
    #[error("invalid connector scope: {0}")]
    InvalidScope(String),
    #[error("invalid logical path: {0}")]
    InvalidLogicalPath(String),
    #[error("logical path {path} exceeds its {max_bytes}-byte cap")]
    FileTooLarge { path: String, max_bytes: u64 },
    #[error("unsupported logical path: {0}")]
    UnsupportedPath(String),
    #[error("invalid sha256 digest")]
    InvalidDigest,
    #[error("invalid remote metadata field: {0}")]
    InvalidRemoteMetadata(&'static str),
    #[error("invalid onboard field: {0}")]
    InvalidOnboardField(&'static str),
    #[error("invalid telemetry field: {0}")]
    InvalidTelemetry(&'static str),
    #[error("manifest entries are not strictly sorted")]
    ManifestNotSorted,
    #[error("manifest contains duplicate logical path {0}")]
    DuplicatePath(String),
    #[error("manifest contains duplicate remote id {0}")]
    DuplicateRemoteId(String),
    #[error("manifest declares {actual} files, limit is {limit}")]
    TooManyFiles { actual: u64, limit: u64 },
    #[error("manifest declares {actual} logical bytes, limit is {limit}")]
    TooManyBytes { actual: u64, limit: u64 },
    #[error("manifest file count does not match descriptor")]
    FileCountMismatch,
    #[error("manifest logical byte count does not match descriptor")]
    LogicalBytesMismatch,
    #[error("manifest digest does not match descriptor")]
    ManifestDigestMismatch,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// One published file at a generation.
///
/// `content_sha256` covers the bytes the corpus will chunk, which for a
/// provider-native document means the EXPORTED bytes, not whatever the vendor
/// stores internally. The three remote fields are metadata: they let evidence
/// rendering cite the source document and let a reader open it, and they are
/// covered by the manifest digest so a tampered URL is a rejected manifest
/// rather than a rendered lie. They are never freshness authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileManifestEntryV1 {
    /// Validated, scope-relative, slash-separated. Derived from the remote
    /// name by [`derive_logical_path`]; never the raw remote name.
    pub logical_path: String,
    pub content_sha256: String,
    pub size: u64,
    /// The connector-stable remote identity. Opaque to the corpus.
    pub remote_id: String,
    /// The remote's version token at capture (etag, revision, changestamp).
    pub remote_version: String,
    /// A renderable URL for the source document, when the store has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
}

impl FileManifestEntryV1 {
    pub fn validate(&self) -> Result<(), FileSourceError> {
        validate_logical_path(&self.logical_path)?;
        validate_sha256(&self.content_sha256)?;
        let max_bytes =
            bbox_code_source::max_bytes_for_path(std::path::Path::new(&self.logical_path))
                .ok_or_else(|| FileSourceError::UnsupportedPath(self.logical_path.clone()))?;
        if self.size > max_bytes {
            return Err(FileSourceError::FileTooLarge {
                path: self.logical_path.clone(),
                max_bytes,
            });
        }
        validate_opaque_token(&self.remote_id, MAX_REMOTE_ID_BYTES)
            .map_err(|_| FileSourceError::InvalidRemoteMetadata("remote_id"))?;
        validate_opaque_token(&self.remote_version, MAX_REMOTE_VERSION_BYTES)
            .map_err(|_| FileSourceError::InvalidRemoteMetadata("remote_version"))?;
        if let Some(url) = &self.remote_url {
            validate_remote_url(url)?;
        }
        Ok(())
    }
}

/// A renderable remote URL is bounded, control-free, `https`/`http` only, and
/// carries no userinfo.
///
/// Userinfo is refused for one specific reason: a URL like
/// `https://token@drive.example/f/1` would put producer-side credential
/// material into the durable manifest, the index, and every rendered evidence
/// bundle. The satellite is the only thing holding vendor credentials and this
/// is the one field where they could plausibly leak across the wire, so the
/// leaf contract refuses the shape rather than trusting each adapter.
pub fn validate_remote_url(value: &str) -> Result<(), FileSourceError> {
    if value.is_empty() || value.len() > MAX_REMOTE_URL_BYTES {
        return Err(FileSourceError::InvalidRemoteMetadata("remote_url length"));
    }
    if value
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control() || byte == b' ')
    {
        return Err(FileSourceError::InvalidRemoteMetadata("remote_url bytes"));
    }
    let rest = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .ok_or(FileSourceError::InvalidRemoteMetadata("remote_url scheme"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err(FileSourceError::InvalidRemoteMetadata(
            "remote_url authority",
        ));
    }
    if authority.contains('@') {
        return Err(FileSourceError::InvalidRemoteMetadata(
            "remote_url must carry no userinfo",
        ));
    }
    Ok(())
}

/// Opaque connector token: non-empty, bounded, and free of NUL and control
/// bytes. Nothing parses these, so nothing more is demanded of them.
fn validate_opaque_token(value: &str, max_bytes: usize) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(());
    }
    Ok(())
}

/// Logical-path validation.
///
/// This applies the shared relative-path contract from `bbox-code-source`
/// (non-empty, relative, slash-separated, no empty / `.` / `..` / backslash /
/// NUL / control / drive-prefix component, bounded per component and in
/// total) and deliberately does NOT apply that crate's
/// `is_skipped_component` rule.
///
/// That rule (skip dotfiles, `target`, `node_modules`, `_build`,
/// `.worktrees`) is source-tree WALKER policy: it exists so a code checkout
/// does not publish its build output. A document store has no build output,
/// and a remote folder legitimately named `target` or a document named
/// `.plan.md` is ordinary content whose silent disappearance would be
/// indistinguishable from a broken connector. The byte caps and supported
/// extension set ARE shared, because those describe what the corpus can
/// actually chunk.
pub fn validate_logical_path(value: &str) -> Result<(), FileSourceError> {
    bbox_code_source::validate_relative_path(value)
        .map_err(|_| FileSourceError::InvalidLogicalPath(value.to_string()))
}

pub fn validate_sha256(value: &str) -> Result<(), FileSourceError> {
    bbox_code_source::validate_sha256(value).map_err(|_| FileSourceError::InvalidDigest)
}

pub fn validate_scope(scope: &ConnectorScope) -> Result<(), FileSourceError> {
    scope
        .validate()
        .map_err(|error| FileSourceError::InvalidScope(error.to_string()))
}

/// Validate a whole manifest: per-entry shape, strict path ordering, path and
/// remote-id uniqueness, and the cardinality and byte ceilings.
///
/// Remote-id uniqueness is checked in addition to path uniqueness because the
/// producer journal is keyed on `remote_id`: two entries claiming one remote
/// id would make the server's view unreconcilable with the journal that
/// produced it, and would let a rename be misread as a duplication.
pub fn validate_manifest(
    entries: &[FileManifestEntryV1],
    max_files: u64,
    max_logical_bytes: u64,
) -> Result<(), FileSourceError> {
    let file_count = entries.len() as u64;
    if file_count > max_files {
        return Err(FileSourceError::TooManyFiles {
            actual: file_count,
            limit: max_files,
        });
    }
    let mut previous: Option<&str> = None;
    let mut remote_ids = std::collections::BTreeSet::new();
    let mut logical_bytes = 0_u64;
    for entry in entries {
        entry.validate()?;
        if let Some(previous) = previous {
            if entry.logical_path == previous {
                return Err(FileSourceError::DuplicatePath(entry.logical_path.clone()));
            }
            if entry.logical_path.as_str() < previous {
                return Err(FileSourceError::ManifestNotSorted);
            }
        }
        previous = Some(&entry.logical_path);
        if !remote_ids.insert(entry.remote_id.as_str()) {
            return Err(FileSourceError::DuplicateRemoteId(entry.remote_id.clone()));
        }
        logical_bytes =
            logical_bytes
                .checked_add(entry.size)
                .ok_or(FileSourceError::TooManyBytes {
                    actual: u64::MAX,
                    limit: max_logical_bytes,
                })?;
        if logical_bytes > max_logical_bytes {
            return Err(FileSourceError::TooManyBytes {
                actual: logical_bytes,
                limit: max_logical_bytes,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Generation descriptor
// ---------------------------------------------------------------------------

/// The immutable header of one published generation (design section 10).
///
/// `connector_kind` is NOT a separate field: [`ConnectorScope`] already
/// carries it as the operator's grant-time declaration, and a second copy
/// could only ever disagree with the first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileGenerationDescriptorV1 {
    pub schema_version: u32,
    pub connector_policy_version: String,
    pub scope: ConnectorScope,
    /// Opaque, connector-shaped, display and diagnostic ONLY. It occupies the
    /// slot `head_commit` occupies on the code lane and is deliberately not
    /// authority: the manifest digest is the fingerprint.
    pub remote_watermark: String,
    /// Incremented by the satellite on every full re-enumeration, so a
    /// generation minted after a checkpoint invalidation is distinct from the
    /// one before it even when the manifest is byte-identical.
    pub cursor_epoch: u64,
    pub manifest_sha256: String,
    pub file_count: u64,
    pub logical_bytes: u64,
}

impl FileGenerationDescriptorV1 {
    pub fn validate_header(&self) -> Result<(), FileSourceError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(FileSourceError::UnsupportedSchema(self.schema_version));
        }
        if self.connector_policy_version != CONNECTOR_POLICY_VERSION {
            return Err(FileSourceError::ConnectorPolicyMismatch {
                received: self.connector_policy_version.clone(),
                expected: CONNECTOR_POLICY_VERSION,
            });
        }
        validate_scope(&self.scope)?;
        validate_sha256(&self.manifest_sha256)?;
        if validate_opaque_token(&self.remote_watermark, MAX_REMOTE_WATERMARK_BYTES).is_err() {
            return Err(FileSourceError::InvalidRemoteMetadata("remote_watermark"));
        }
        Ok(())
    }

    pub fn validate_manifest(
        &self,
        entries: &[FileManifestEntryV1],
        max_files: u64,
        max_logical_bytes: u64,
    ) -> Result<(), FileSourceError> {
        self.validate_header()?;
        validate_manifest(entries, max_files, max_logical_bytes)?;
        let file_count = entries.len() as u64;
        let logical_bytes = entries.iter().try_fold(0_u64, |sum, entry| {
            sum.checked_add(entry.size)
                .ok_or(FileSourceError::TooManyBytes {
                    actual: u64::MAX,
                    limit: max_logical_bytes,
                })
        })?;
        if file_count != self.file_count {
            return Err(FileSourceError::FileCountMismatch);
        }
        if logical_bytes != self.logical_bytes {
            return Err(FileSourceError::LogicalBytesMismatch);
        }
        if manifest_sha256(entries) != self.manifest_sha256 {
            return Err(FileSourceError::ManifestDigestMismatch);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Publication telemetry and cursor degradation
// ---------------------------------------------------------------------------

/// Per-reason skip counters and remote-call classes for one publication.
///
/// Silence is not an option: a policy quietly dropping half a drive is
/// indistinguishable from a broken connector, so every exclusion is counted
/// and every count crosses the wire onto publication status.
///
/// This rides the BEGIN request rather than the descriptor deliberately. The
/// generation id is derived from the descriptor, and telemetry is an
/// observation about how a manifest was produced, not part of what the
/// manifest IS: folding it into the descriptor would mint a new generation id
/// for a byte-identical corpus every time a skip counter moved.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationTelemetryV1 {
    /// Bounded per-reason counters (`oversize`, `unsupported`, `excluded`,
    /// `export_failed`, ...). Reason labels are connector-chosen and opaque.
    #[serde(default)]
    pub skipped: BTreeMap<String, u64>,
    /// Entries the connector enumerated before policy filtering.
    #[serde(default)]
    pub entries_enumerated: u64,
    /// Blobs actually fetched from the remote this cycle.
    #[serde(default)]
    pub blobs_fetched: u64,
    /// Provider-native documents re-exported this cycle. An unchanged native
    /// document must never appear here.
    #[serde(default)]
    pub documents_exported: u64,
}

impl PublicationTelemetryV1 {
    pub fn validate(&self) -> Result<(), FileSourceError> {
        if self.skipped.len() > MAX_SKIP_REASONS {
            return Err(FileSourceError::InvalidTelemetry("too many skip reasons"));
        }
        for reason in self.skipped.keys() {
            if validate_opaque_token(reason, MAX_REASON_BYTES).is_err() {
                return Err(FileSourceError::InvalidTelemetry("skip reason"));
            }
        }
        Ok(())
    }

    pub fn total_skipped(&self) -> u64 {
        self.skipped
            .values()
            .fold(0, |sum, n| sum.saturating_add(*n))
    }
}

/// One reported checkpoint invalidation: the cause, and what the degradation
/// cost.
///
/// An expired checkpoint degrades to full re-enumeration, and the degradation
/// is never silently absorbed. An operator watching repeated `cursor_epoch`
/// increments is watching a real problem (revoked consent, tenant policy, a
/// connector bug), so the cause and the cost travel to publication status,
/// health, and doctor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CursorDegradationV1 {
    /// The connector's own name for the invalidated enumeration stream.
    pub checkpoint_name: String,
    /// Connector-supplied cause text, bounded and opaque.
    pub cause: String,
    /// The epoch this degradation produced (the descriptor's `cursor_epoch`).
    pub cursor_epoch: u64,
    pub entries_enumerated: u64,
    pub blobs_refetched: u64,
    pub documents_reexported: u64,
    pub observed_at: String,
}

impl CursorDegradationV1 {
    pub fn validate(&self) -> Result<(), FileSourceError> {
        if validate_opaque_token(&self.checkpoint_name, MAX_REASON_BYTES).is_err() {
            return Err(FileSourceError::InvalidTelemetry("checkpoint_name"));
        }
        if validate_opaque_token(&self.cause, MAX_REASON_BYTES).is_err() {
            return Err(FileSourceError::InvalidTelemetry("degradation cause"));
        }
        if validate_opaque_token(&self.observed_at, MAX_REASON_BYTES).is_err() {
            return Err(FileSourceError::InvalidTelemetry("observed_at"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Publication conversation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BeginFileUploadRequestV1 {
    pub descriptor: FileGenerationDescriptorV1,
    #[serde(default)]
    pub telemetry: PublicationTelemetryV1,
    /// Present exactly when this generation was produced by a full
    /// re-enumeration forced by an invalidated checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<CursorDegradationV1>,
}

impl BeginFileUploadRequestV1 {
    pub fn validate_header(&self) -> Result<(), FileSourceError> {
        self.descriptor.validate_header()?;
        self.telemetry.validate()?;
        if let Some(degradation) = &self.degradation {
            degradation.validate()?;
            if degradation.cursor_epoch != self.descriptor.cursor_epoch {
                return Err(FileSourceError::InvalidTelemetry(
                    "degradation cursor_epoch disagrees with the descriptor",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BeginFileUploadResponseV1 {
    pub upload_id: String,
    pub ordinal: u64,
    pub max_page_entries: usize,
    pub max_page_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileManifestPageV1 {
    pub entries: Vec<FileManifestEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissingFileBlobsPageV1 {
    pub generation_id: String,
    pub hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FinalizeFileUploadResponseV1 {
    pub generation_id: String,
    pub status_url: String,
}

/// Lifecycle of one file-source generation, mirroring the code lane so the
/// shared store's states project onto this wire without translation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileGenerationStateV1 {
    ReceivingManifest,
    MissingBlobs,
    Ready,
    StagingIndex,
    Active,
    Superseded,
    MissingBlobData,
    Failed,
}

impl FileGenerationStateV1 {
    /// True for the states a producer may stop polling on. `Superseded` counts
    /// because a faster sibling generation activating over this one is a
    /// success for this one: its bytes are in the corpus.
    pub fn is_terminal_success(self) -> bool {
        matches!(self, Self::Active | Self::Superseded)
    }

    pub fn is_terminal_failure(self) -> bool {
        matches!(self, Self::Failed | Self::MissingBlobData)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileGenerationStatusV1 {
    pub generation_id: String,
    pub state: FileGenerationStateV1,
    pub file_count: u64,
    pub logical_bytes: u64,
    pub cursor_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Catalog onboarding
// ---------------------------------------------------------------------------

/// Facts the satellite probed from the remote store, presented over the
/// authenticated producer channel (design section 9).
///
/// The producer id is NOT carried here: it comes from the authenticated
/// bearer, so a producer cannot claim to be another. The daemon revalidates
/// grant membership, grant uniqueness, kind agreement, and remote-authority
/// agreement, and it cannot verify that the presented coordinates describe the
/// folder the operator meant. That residual producer trust is the same class
/// the code-source lane already accepts for the canonical path string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileCatalogOnboardRequestV1 {
    pub schema_version: u32,
    pub scope: ConnectorScope,
    /// The connector kind the producer actually ran, checked against the
    /// grant's declared kind.
    pub probed_connector_kind: String,
    /// The vendor tenant or account the producer actually reached.
    pub probed_remote_authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_remote_root_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed_remote_display_name: Option<String>,
    pub display_name: String,
}

impl FileCatalogOnboardRequestV1 {
    pub fn validate(&self) -> Result<(), FileSourceError> {
        if self.schema_version != CATALOG_ONBOARD_SCHEMA_VERSION {
            return Err(FileSourceError::UnsupportedSchema(self.schema_version));
        }
        validate_scope(&self.scope)?;
        if validate_opaque_token(&self.probed_connector_kind, 64).is_err() {
            return Err(FileSourceError::InvalidOnboardField(
                "probed_connector_kind",
            ));
        }
        if validate_opaque_token(&self.probed_remote_authority, MAX_REMOTE_AUTHORITY_BYTES).is_err()
        {
            return Err(FileSourceError::InvalidOnboardField(
                "probed_remote_authority",
            ));
        }
        if validate_opaque_token(&self.display_name, MAX_DISPLAY_NAME_BYTES).is_err() {
            return Err(FileSourceError::InvalidOnboardField("display_name"));
        }
        for (value, field) in [
            (&self.probed_remote_root_id, "probed_remote_root_id"),
            (
                &self.probed_remote_display_name,
                "probed_remote_display_name",
            ),
        ] {
            if let Some(value) = value
                && validate_opaque_token(value, MAX_DISPLAY_NAME_BYTES).is_err()
            {
                return Err(FileSourceError::InvalidOnboardField(field));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileCatalogOnboardResponseV1 {
    pub project_id: String,
    /// True only when THIS call minted the project. Find-or-create, so a
    /// producer driving it before every cycle is safe to retry.
    pub created_project: bool,
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// Digests and identity
// ---------------------------------------------------------------------------

/// The manifest digest: the fingerprint of a connector generation.
///
/// Every field of every entry is covered, remote metadata included, so a
/// manifest whose `remote_url` was rewritten in flight fails the descriptor
/// check rather than rendering a tampered citation.
pub fn manifest_sha256(entries: &[FileManifestEntryV1]) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-file-source-manifest-v1");
    for entry in entries {
        put_field(&mut hasher, entry.logical_path.as_bytes());
        put_field(&mut hasher, entry.content_sha256.as_bytes());
        hasher.update(entry.size.to_be_bytes());
        put_field(&mut hasher, entry.remote_id.as_bytes());
        put_field(&mut hasher, entry.remote_version.as_bytes());
        match &entry.remote_url {
            Some(url) => {
                hasher.update([1_u8]);
                put_field(&mut hasher, url.as_bytes());
            }
            None => hasher.update([0_u8]),
        }
    }
    hex::encode(hasher.finalize())
}

/// Derive the generation id server-side. The producer never supplies one.
///
/// `remote_watermark` is deliberately absent: it is display-only, so folding
/// it in would mint a fresh generation for a corpus-identical manifest every
/// time a vendor bumped a changestamp. `cursor_epoch` IS folded in, because a
/// generation produced by a full re-enumeration is a materially different
/// event from the incremental one before it even when the bytes agree.
pub fn generation_id(producer_id: &str, descriptor: &FileGenerationDescriptorV1) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-file-source-generation-v1");
    put_field(&mut hasher, producer_id.as_bytes());
    put_scope(&mut hasher, &descriptor.scope);
    put_field(&mut hasher, descriptor.connector_policy_version.as_bytes());
    hasher.update(descriptor.cursor_epoch.to_be_bytes());
    put_field(&mut hasher, descriptor.manifest_sha256.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn scope_hash(scope: &ConnectorScope) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-file-source-scope-v1");
    put_scope(&mut hasher, scope);
    hex::encode(hasher.finalize())
}

fn put_scope(hasher: &mut Sha256, scope: &ConnectorScope) {
    put_field(hasher, scope.connector_source_id().as_str().as_bytes());
    put_field(hasher, scope.connector_kind().as_str().as_bytes());
}

fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

// ---------------------------------------------------------------------------
// Logical path derivation (design section 12)
// ---------------------------------------------------------------------------

/// The collision key a derived logical path is deduplicated under.
///
/// Case-folded and NFC-shaped even though no local filesystem is involved,
/// for two independent reasons: manifest paths must be unique on the wire, and
/// downstream display, path-token search, and evidence rendering all treat the
/// logical path as a path.
///
/// The normalization here is deliberately conservative: ASCII case folding
/// plus a compatibility fold of the handful of shapes that a real remote store
/// produces and a reader would read as identical. Full Unicode NFC would need
/// a normalization table dependency in a leaf crate both the daemon and the
/// satellite link; the cost of the conservative rule is that two names
/// differing only by an exotic normalization survive as distinct entries,
/// which is the SAFE direction (two reachable documents rather than one
/// silently shadowing the other).
pub fn collision_key(logical_path: &str) -> String {
    logical_path.to_lowercase()
}

/// Sanitize one remote name component into a wire-legal path component.
///
/// Everything the shared path contract refuses is replaced with `_`, the
/// result is truncated to the component byte ceiling on a char boundary, and
/// the reserved shapes (`""`, `.`, `..`) collapse to `_`. This is
/// deterministic: the same remote name always derives the same component, so a
/// path is stable across scans and a rename is visible as a moved path rather
/// than a churned one.
pub fn sanitize_component(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let replace = matches!(
            ch,
            '/' | '\\' | '\0' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
        ) || ch.is_control();
        out.push(if replace { '_' } else { ch });
    }
    // Trailing dots and spaces are the classic display-and-filesystem
    // ambiguity; leading dots would additionally make a document look like a
    // dotfile to every reader downstream.
    let trimmed = out.trim_matches(|ch: char| ch == ' ' || ch == '.');
    let candidate = if trimmed.is_empty() {
        out.as_str()
    } else {
        trimmed
    };
    reserved_or(truncate_component(candidate))
}

fn reserved_or(candidate: String) -> String {
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        "_".to_string()
    } else {
        candidate
    }
}

/// Truncate one component to the shared byte ceiling WITHOUT losing its
/// extension.
///
/// The extension is what the chunker registry claims, so a plain right-trim
/// would silently unclaim an oversized-name document and drop it from the
/// manifest as unsupported. The stem is the only honest place to spend the
/// budget.
fn truncate_component(component: &str) -> String {
    if component.len() <= bbox_code_source::MAX_PATH_COMPONENT_BYTES {
        return component.to_string();
    }
    let (stem, extension) = split_extension(component);
    if extension.len() >= bbox_code_source::MAX_PATH_COMPONENT_BYTES {
        // An "extension" that alone blows the ceiling is not an extension.
        return truncate_on_char_boundary(component, bbox_code_source::MAX_PATH_COMPONENT_BYTES)
            .to_string();
    }
    let budget = bbox_code_source::MAX_PATH_COMPONENT_BYTES - extension.len();
    let stem = truncate_on_char_boundary(stem, budget);
    let stem = if stem.is_empty() { "_" } else { stem };
    format!("{stem}{extension}")
}

fn truncate_on_char_boundary(value: &str, budget: usize) -> &str {
    if value.len() <= budget {
        return value;
    }
    let mut end = budget;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// A short, deterministic disambiguator derived from the entry's remote id.
///
/// Derived from `remote_id` rather than from a counter so the suffix is stable
/// across scans: a counter would reshuffle two colliding documents' paths
/// whenever enumeration order changed, republishing both for no reason.
pub fn collision_suffix(remote_id: &str) -> String {
    let mut hasher = Sha256::new();
    put_field(&mut hasher, b"bbox-file-source-collision-v1");
    put_field(&mut hasher, remote_id.as_bytes());
    hex::encode(hasher.finalize())[..8].to_string()
}

/// Split a component into (stem, extension-with-dot), so a collision suffix
/// lands before the extension and the chunker registry still claims the file.
fn split_extension(component: &str) -> (&str, &str) {
    match component.rfind('.') {
        // A leading dot is not an extension separator (`.plan` is a stem).
        Some(index) if index > 0 => component.split_at(index),
        _ => (component, ""),
    }
}

/// Apply a collision suffix to the last component of a derived path.
pub fn apply_collision_suffix(logical_path: &str, remote_id: &str) -> String {
    let suffix = collision_suffix(remote_id);
    let (parent, last) = match logical_path.rfind('/') {
        Some(index) => (&logical_path[..=index], &logical_path[index + 1..]),
        None => ("", logical_path),
    };
    let (stem, extension) = split_extension(last);
    // Keep the whole component inside the byte ceiling by trimming the stem,
    // never the suffix or the extension: dropping the suffix would reintroduce
    // the collision, and dropping the extension would unclaim the chunker.
    let budget = bbox_code_source::MAX_PATH_COMPONENT_BYTES
        .saturating_sub(suffix.len() + 1 + extension.len());
    let stem = truncate_on_char_boundary(stem, budget);
    format!("{parent}{stem}-{suffix}{extension}")
}

/// Derive one entry's logical path from its scope-relative remote name path.
///
/// `remote_name_path` is the raw, untrusted sequence of remote name components
/// from the scope root down to the entry. `export_extension` is `Some` for a
/// provider-native document the connector exported, and replaces whatever
/// extension the remote name carried (the corpus chunks the EXPORT).
pub fn derive_logical_path(
    remote_name_path: &[String],
    export_extension: Option<&str>,
) -> Result<String, FileSourceError> {
    if remote_name_path.is_empty() {
        return Err(FileSourceError::InvalidLogicalPath(String::new()));
    }
    let last = remote_name_path.len() - 1;
    let mut components: Vec<String> = remote_name_path
        .iter()
        .map(|component| sanitize_component(component))
        .collect();
    if let Some(extension) = export_extension {
        // The stem is copied out before the slot is rewritten: the export
        // extension REPLACES whatever the remote name carried, because the
        // corpus chunks the exported bytes and nothing else.
        let stem = {
            let (stem, _) = split_extension(&components[last]);
            if stem.is_empty() {
                "_".to_string()
            } else {
                stem.to_string()
            }
        };
        components[last] = reserved_or(truncate_component(&format!("{stem}.{extension}")));
    }
    let path = components.join("/");
    validate_logical_path(&path)?;
    Ok(path)
}

/// The stable durable selector a connector generation's content is indexed
/// under. Deliberately the SAME `collected:` shape the code lane uses: a
/// connector source is an ordinary collected source to everything downstream
/// of byte acquisition, which is the whole point of the design. There is no
/// `connector:` source kind, and adding one would fork the indexing seam for
/// no gain.
pub fn source_selector(project_id: &str, generation_id: &str) -> String {
    bbox_code_source::source_selector(project_id, generation_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
    }

    fn entry(path: &str, hash: char, size: u64, remote_id: &str) -> FileManifestEntryV1 {
        FileManifestEntryV1 {
            logical_path: path.into(),
            content_sha256: hash.to_string().repeat(64),
            size,
            remote_id: remote_id.into(),
            remote_version: "v1".into(),
            remote_url: Some(format!("https://fixture.example/d/{remote_id}")),
        }
    }

    fn descriptor(
        entries: &[FileManifestEntryV1],
        cursor_epoch: u64,
    ) -> FileGenerationDescriptorV1 {
        FileGenerationDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            connector_policy_version: CONNECTOR_POLICY_VERSION.into(),
            scope: scope(),
            remote_watermark: "watermark-1".into(),
            cursor_epoch,
            manifest_sha256: manifest_sha256(entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.size).sum(),
        }
    }

    #[test]
    fn a_valid_manifest_round_trips_through_its_descriptor() {
        let entries = vec![
            entry("notes/plan.md", 'a', 12, "remote-1"),
            entry("notes/report.pdf", 'b', 900, "remote-2"),
        ];
        let descriptor = descriptor(&entries, 0);
        descriptor
            .validate_manifest(
                &entries,
                DEFAULT_MAX_MANIFEST_FILES,
                DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
            )
            .unwrap();
        assert_eq!(generation_id("producer-a", &descriptor).len(), 64);
    }

    #[test]
    fn remote_metadata_is_covered_by_the_manifest_digest() {
        let entries = vec![entry("notes/plan.md", 'a', 12, "remote-1")];
        let descriptor = descriptor(&entries, 0);
        for tampered in [
            {
                let mut tampered = entries.clone();
                tampered[0].remote_url = Some("https://evil.example/phish".into());
                tampered
            },
            {
                let mut tampered = entries.clone();
                tampered[0].remote_id = "remote-9".into();
                tampered
            },
            {
                let mut tampered = entries.clone();
                tampered[0].remote_version = "v2".into();
                tampered
            },
        ] {
            assert!(
                matches!(
                    descriptor.validate_manifest(
                        &tampered,
                        DEFAULT_MAX_MANIFEST_FILES,
                        DEFAULT_MAX_MANIFEST_LOGICAL_BYTES,
                    ),
                    Err(FileSourceError::ManifestDigestMismatch)
                ),
                "rewritten remote metadata must fail the descriptor digest"
            );
        }
    }

    #[test]
    fn a_full_reenumeration_epoch_changes_the_generation_id() {
        let entries = vec![entry("notes/plan.md", 'a', 12, "remote-1")];
        let first = descriptor(&entries, 0);
        let second = descriptor(&entries, 1);
        assert_ne!(
            generation_id("producer-a", &first),
            generation_id("producer-a", &second),
            "a converged re-enumeration is still a distinct generation"
        );
    }

    #[test]
    fn a_display_only_watermark_does_not_change_the_generation_id() {
        let entries = vec![entry("notes/plan.md", 'a', 12, "remote-1")];
        let first = descriptor(&entries, 0);
        let mut second = descriptor(&entries, 0);
        second.remote_watermark = "watermark-2".into();
        assert_eq!(
            generation_id("producer-a", &first),
            generation_id("producer-a", &second),
            "remote_watermark is diagnostic, never identity"
        );
    }

    #[test]
    fn manifest_validation_rejects_order_duplicates_and_caps() {
        assert!(matches!(
            validate_manifest(
                &[
                    entry("b.md", 'b', 1, "remote-2"),
                    entry("a.md", 'a', 1, "remote-1"),
                ],
                8,
                8
            ),
            Err(FileSourceError::ManifestNotSorted)
        ));
        assert!(matches!(
            validate_manifest(
                &[
                    entry("a.md", 'a', 1, "remote-1"),
                    entry("a.md", 'b', 1, "remote-2"),
                ],
                8,
                8
            ),
            Err(FileSourceError::DuplicatePath(_))
        ));
        assert!(matches!(
            validate_manifest(
                &[
                    entry("a.md", 'a', 1, "remote-1"),
                    entry("b.md", 'b', 1, "remote-1"),
                ],
                8,
                8
            ),
            Err(FileSourceError::DuplicateRemoteId(_))
        ));
        assert!(matches!(
            validate_manifest(&[entry("a.md", 'a', 1, "remote-1")], 0, 8),
            Err(FileSourceError::TooManyFiles { .. })
        ));
        assert!(matches!(
            validate_manifest(&[entry("a.md", 'a', 2, "remote-1")], 8, 1),
            Err(FileSourceError::TooManyBytes { .. })
        ));
    }

    #[test]
    fn a_document_store_may_publish_names_the_code_walker_skips() {
        // `target`, `node_modules`, and dotted names are code-checkout walker
        // policy. A remote folder named `target` is ordinary content and must
        // survive; its silent disappearance would be indistinguishable from a
        // broken connector.
        for path in [
            "target/quarterly.md",
            "node_modules/handbook.md",
            ".plan/roadmap.md",
        ] {
            validate_logical_path(path).unwrap_or_else(|error| {
                panic!("document-store path {path} must be legal: {error}")
            });
            entry(path, 'a', 4, "remote-1").validate().unwrap();
        }
    }

    #[test]
    fn a_manifest_entry_the_corpus_cannot_chunk_is_refused() {
        assert!(matches!(
            entry("archive/blob.bin", 'a', 4, "remote-1").validate(),
            Err(FileSourceError::UnsupportedPath(_))
        ));
        assert!(matches!(
            entry(
                "notes/huge.md",
                'a',
                bbox_code_source::MAX_TEXT_FILE_BYTES + 1,
                "r"
            )
            .validate(),
            Err(FileSourceError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn remote_urls_refuse_userinfo_and_foreign_schemes() {
        validate_remote_url("https://drive.example/file/d/abc/view").unwrap();
        validate_remote_url("http://127.0.0.1:8080/f/1").unwrap();
        for invalid in [
            // Userinfo could carry producer-side credential material into the
            // durable manifest and every rendered evidence bundle.
            "https://secret-token@drive.example/f/1",
            "ftp://drive.example/f/1",
            "javascript:alert(1)",
            "https://",
            "",
            "https://drive.example/a b",
            "https://drive.example/a\nb",
        ] {
            assert!(
                validate_remote_url(invalid).is_err(),
                "{invalid} must be refused"
            );
        }
    }

    #[test]
    fn logical_paths_are_derived_not_trusted() {
        assert_eq!(
            derive_logical_path(&["Q3 Reports".into(), "plan/draft.md".into()], None).unwrap(),
            "Q3 Reports/plan_draft.md"
        );
        assert_eq!(
            derive_logical_path(&["..".into(), "notes.md".into()], None).unwrap(),
            "_/notes.md"
        );
        assert_eq!(
            derive_logical_path(&["a\0b".into(), "c\nd.md".into()], None).unwrap(),
            "a_b/c_d.md"
        );
        // A provider-native document takes the EXPORTED extension, because
        // those are the bytes the corpus chunks.
        assert_eq!(
            derive_logical_path(&["Ops".into(), "Runbook".into()], Some("docx")).unwrap(),
            "Ops/Runbook.docx"
        );
        assert_eq!(
            derive_logical_path(&["Ops".into(), "Budget.gsheet".into()], Some("xlsx")).unwrap(),
            "Ops/Budget.xlsx"
        );
        assert!(derive_logical_path(&[], None).is_err());
    }

    #[test]
    fn colliding_names_derive_distinct_reachable_paths() {
        // Drive's duplicate-sibling model and case folding both produce genuine
        // collisions. Both entries must land, both must be reachable, and the
        // suffix must be stable across scans.
        let first = derive_logical_path(&["Ops".into(), "Report.md".into()], None).unwrap();
        let second = derive_logical_path(&["Ops".into(), "report.md".into()], None).unwrap();
        assert_ne!(first, second, "case-distinct names stay distinct paths");
        assert_eq!(
            collision_key(&first),
            collision_key(&second),
            "but they collide under the case-folded key"
        );

        let disambiguated = apply_collision_suffix(&second, "remote-2");
        assert_ne!(collision_key(&first), collision_key(&disambiguated));
        assert!(disambiguated.starts_with("Ops/report-"));
        assert!(
            disambiguated.ends_with(".md"),
            "the extension survives so the chunker registry still claims it"
        );
        assert_eq!(
            disambiguated,
            apply_collision_suffix(&second, "remote-2"),
            "the suffix is derived from remote_id, so it is stable across scans"
        );
        assert_ne!(
            disambiguated,
            apply_collision_suffix(&second, "remote-3"),
            "distinct remote ids disambiguate distinctly"
        );
        validate_logical_path(&disambiguated).unwrap();
    }

    #[test]
    fn derived_components_stay_inside_the_shared_path_ceilings() {
        let long = "n".repeat(4096);
        let derived = derive_logical_path(&[long.clone(), format!("{long}.md")], None).unwrap();
        validate_logical_path(&derived).unwrap();
        for component in derived.split('/') {
            assert!(component.len() <= bbox_code_source::MAX_PATH_COMPONENT_BYTES);
        }
        let suffixed = apply_collision_suffix(&derived, "remote-1");
        validate_logical_path(&suffixed).unwrap();
        for component in suffixed.split('/') {
            assert!(
                component.len() <= bbox_code_source::MAX_PATH_COMPONENT_BYTES,
                "a collision suffix must not push a component past the ceiling"
            );
        }
        assert!(suffixed.ends_with(".md"));
    }

    #[test]
    fn a_descriptor_rejects_policy_skew_and_schema_skew() {
        let entries = vec![entry("a.md", 'a', 1, "remote-1")];
        let mut skewed = descriptor(&entries, 0);
        skewed.connector_policy_version = "file-source-connector-policy-v0".into();
        assert!(matches!(
            skewed.validate_header(),
            Err(FileSourceError::ConnectorPolicyMismatch { .. })
        ));

        let mut old_schema = descriptor(&entries, 0);
        old_schema.schema_version = SCHEMA_VERSION + 1;
        assert!(matches!(
            old_schema.validate_header(),
            Err(FileSourceError::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn a_degradation_must_agree_with_the_epoch_it_produced() {
        let entries = vec![entry("a.md", 'a', 1, "remote-1")];
        let mut request = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, 4),
            telemetry: PublicationTelemetryV1::default(),
            degradation: Some(CursorDegradationV1 {
                checkpoint_name: "root".into(),
                cause: "checkpoint_expired".into(),
                cursor_epoch: 4,
                entries_enumerated: 12,
                blobs_refetched: 3,
                documents_reexported: 1,
                observed_at: "2026-08-13T00:00:00Z".into(),
            }),
        };
        request.validate_header().unwrap();
        request.degradation.as_mut().unwrap().cursor_epoch = 3;
        assert!(request.validate_header().is_err());
    }

    #[test]
    fn telemetry_reason_cardinality_is_bounded() {
        let entries = vec![entry("a.md", 'a', 1, "remote-1")];
        let mut telemetry = PublicationTelemetryV1::default();
        for index in 0..=MAX_SKIP_REASONS {
            telemetry.skipped.insert(format!("reason-{index}"), 1);
        }
        let request = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, 0),
            telemetry,
            degradation: None,
        };
        assert!(request.validate_header().is_err());
    }

    #[test]
    fn an_onboard_request_validates_its_probed_facts() {
        let valid = FileCatalogOnboardRequestV1 {
            schema_version: CATALOG_ONBOARD_SCHEMA_VERSION,
            scope: scope(),
            probed_connector_kind: "fixture".into(),
            probed_remote_authority: "fixture.example".into(),
            probed_remote_root_id: Some("root-1".into()),
            probed_remote_display_name: Some("Ops shared folder".into()),
            display_name: "Ops shared folder".into(),
        };
        valid.validate().unwrap();

        let mut bad_schema = valid.clone();
        bad_schema.schema_version += 1;
        assert!(matches!(
            bad_schema.validate(),
            Err(FileSourceError::UnsupportedSchema(_))
        ));

        let mut empty_authority = valid.clone();
        empty_authority.probed_remote_authority = String::new();
        assert!(empty_authority.validate().is_err());

        let mut control_name = valid.clone();
        control_name.display_name = "Ops\nfolder".into();
        assert!(control_name.validate().is_err());
    }

    #[test]
    fn connector_content_is_selected_as_an_ordinary_collected_source() {
        let selector = source_selector("p_abc", &"a".repeat(64));
        assert!(selector.starts_with("collected:"));
        assert_eq!(
            bbox_code_source::source_kind_for_selector(&selector),
            bbox_code_source::SOURCE_KIND_COLLECTED,
            "a connector source is an ordinary collected source downstream"
        );
    }

    #[test]
    fn wire_payloads_deny_unknown_fields() {
        let json = r#"{"logical_path":"a.md","content_sha256":"aaaa","size":1,
            "remote_id":"r","remote_version":"v","surprise":true}"#;
        assert!(serde_json::from_str::<FileManifestEntryV1>(json).is_err());
    }
}
