//! Dependency-clean wire and filesystem policy for distributed code sources.

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
    if scope.repo_id.trim().is_empty()
        || scope.repo_id.trim() != scope.repo_id
        || scope.repo_id.len() > 256
        || scope.repo_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(ContractError::InvalidScope("repo_id".into()));
    }
    if scope.bbox_root_relpath == "." {
        return Ok(());
    }
    validate_relative_path(&scope.bbox_root_relpath)
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

fn put_scope(hasher: &mut Sha256, scope: &PublishedScope) {
    put_field(hasher, scope.repo_id.as_bytes());
    put_field(hasher, scope.bbox_root_relpath.as_bytes());
}

fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> PublishedScope {
        PublishedScope {
            repo_id: "repo-family".into(),
            bbox_root_relpath: ".".into(),
        }
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
        assert!(
            entry("a.rs", 'a', MAX_TEXT_FILE_BYTES + 1)
                .validate()
                .is_err()
        );
    }
}
