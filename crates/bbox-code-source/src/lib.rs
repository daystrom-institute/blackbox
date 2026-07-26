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
