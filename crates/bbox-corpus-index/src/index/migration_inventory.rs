//! Read-only, no-create migration inventory for the corpus index owner.
//!
//! This module is the only migration surface that understands Tantivy's
//! stored fields and the legacy Git cursor files. Callers receive bounded,
//! canonical evidence and never parse either owner-private format.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::collector::DocSetCollector;
use tantivy::query::AllQuery;
use tantivy::{Index, TantivyDocument};

use bbox_corpus_core::entity_ref::EntityRef;

use super::{FileMeta, FileMetaSource, INDEX_SCHEMA_VERSION, SCHEMA_VERSION_FILE, optional_text};

const SNAPSHOT_VERSION_V1: u32 = 1;
const INDEX_SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.schema.v1\0";
const INDEX_ROW_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.project-refs.v1\0";
const INDEX_SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.source.v1\0";
const CODE_META_SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.code-meta.schema.v1\0";
const CODE_META_ROW_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.code-meta.rows.v1\0";
const CODE_META_SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.code-meta.source.v1\0";
const COMMIT_ROW_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.commit-namespace.v1\0";
const GIT_CURSOR_ROW_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.git-cursors.v1\0";
const GIT_CURSOR_SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.git-schema.v1\0";
const GIT_CURSOR_SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.corpus-index.git-source.v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusMigrationSnapshotLimitsV1 {
    pub max_documents: u64,
    pub max_project_scoped_refs: usize,
    pub max_commit_namespaces: usize,
    pub max_code_metadata_rows: usize,
    pub max_git_cursor_files: usize,
    pub max_source_file_bytes: u64,
    pub max_total_string_bytes: usize,
}

impl Default for CorpusMigrationSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_documents: 10_000_000,
            max_project_scoped_refs: 5_000_000,
            max_commit_namespaces: 1_000_000,
            max_code_metadata_rows: 10_000_000,
            max_git_cursor_files: 1_000_000,
            max_source_file_bytes: 512 * 1024 * 1024,
            max_total_string_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusMigrationSourceStateV1 {
    Present,
    Missing,
    Corrupt { diagnostic_code: &'static str },
    Unavailable { diagnostic_code: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusProjectScopedRefV1 {
    pub project_id: String,
    pub entity_ref: String,
    /// More than one committed Tantivy document may expose the same ref.
    /// Retaining multiplicity prevents duplicate rows from disappearing.
    pub document_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusCommitNamespaceV1 {
    pub namespace: String,
    pub commit_document_count: u64,
    pub commit_document_commitment_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusIndexMigrationSnapshotV1 {
    pub version: u32,
    pub state: CorpusMigrationSourceStateV1,
    pub schema_version: Option<String>,
    pub schema_fingerprint_sha256: Option<String>,
    pub source_fingerprint_sha256: Option<String>,
    pub document_count: u64,
    pub project_scoped_ref_count: u64,
    pub project_scoped_ref_commitment_sha256: String,
    pub project_scoped_refs: Vec<CorpusProjectScopedRefV1>,
    pub commit_namespaces: Vec<CorpusCommitNamespaceV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeIndexMetadataSourceKindV1 {
    LegacyFilesystem,
    LocalProjectFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeIndexMetadataRowV1 {
    pub source_key_sha256: String,
    pub source_kind: CodeIndexMetadataSourceKindV1,
    pub mtime: u64,
    pub size: u64,
    pub materialization_version: Option<String>,
    pub project_id: Option<String>,
    pub selector: Option<String>,
    pub relative_path: Option<String>,
    pub entry_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeIndexMetadataMigrationSnapshotV1 {
    pub version: u32,
    pub state: CorpusMigrationSourceStateV1,
    pub schema_fingerprint_sha256: String,
    pub source_fingerprint_sha256: Option<String>,
    pub row_count: u64,
    pub project_scoped_row_count: u64,
    pub row_commitment_sha256: String,
    pub rows: Vec<CodeIndexMetadataRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCursorMigrationRowV1 {
    pub project_id: String,
    pub last_ingested_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCursorMigrationSnapshotV1 {
    pub version: u32,
    pub state: CorpusMigrationSourceStateV1,
    pub schema_fingerprint_sha256: String,
    pub source_fingerprint_sha256: Option<String>,
    pub row_count: u64,
    pub row_commitment_sha256: String,
    pub rows: Vec<GitCursorMigrationRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusOwnerMigrationSnapshotV1 {
    pub index: CorpusIndexMigrationSnapshotV1,
    pub code_metadata: CodeIndexMetadataMigrationSnapshotV1,
    pub git_cursors: GitCursorMigrationSnapshotV1,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GitIngestMetaV1 {
    last_ingested_sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CommitRowV1 {
    namespace: String,
    entity_ref: String,
    commit_sha: String,
    content_hash: String,
}

/// Capture committed corpus-index and legacy Git-cursor evidence without
/// creating, repairing, resetting, or rewriting either source.
pub fn capture_owner_migration_snapshot_no_create(
    index_path: &Path,
    git_meta_dir: &Path,
    limits: CorpusMigrationSnapshotLimitsV1,
) -> CorpusOwnerMigrationSnapshotV1 {
    CorpusOwnerMigrationSnapshotV1 {
        index: capture_index(index_path, limits),
        code_metadata: capture_code_metadata(index_path, limits),
        git_cursors: capture_git_cursors(git_meta_dir, limits),
    }
}

fn capture_code_metadata(
    index_path: &Path,
    limits: CorpusMigrationSnapshotLimitsV1,
) -> CodeIndexMetadataMigrationSnapshotV1 {
    if !strict_absolute_path(index_path) {
        return corrupt_code_metadata("code_index_metadata_path_invalid");
    }
    match fs::symlink_metadata(index_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return missing_code_metadata();
        }
        Err(_) => {
            return unavailable_code_metadata("code_index_metadata_root_unavailable");
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_code_metadata("code_index_metadata_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_code_metadata("code_index_metadata_root_not_directory");
        }
        Ok(_) => {}
    }
    let path = index_path.join("_meta.json");
    let bytes = match read_regular_bounded(&path, limits.max_source_file_bytes) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return missing_code_metadata(),
        Err(CaptureReadFailure::Corrupt(code)) => return corrupt_code_metadata(code),
        Err(CaptureReadFailure::Unavailable(code)) => return unavailable_code_metadata(code),
    };
    let metadata: BTreeMap<String, FileMeta> = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(_) => return corrupt_code_metadata("code_index_metadata_decode_failed"),
    };
    if metadata.len() > limits.max_code_metadata_rows {
        return corrupt_code_metadata("code_index_metadata_row_limit");
    }
    let mut rows = Vec::with_capacity(metadata.len());
    let mut row_digest = Sha256::new();
    row_digest.update(CODE_META_ROW_HASH_DOMAIN);
    let mut total_string_bytes = 0usize;
    for (source_key, meta) in metadata {
        let source_key_sha256 = domain_hash(CODE_META_ROW_HASH_DOMAIN, [source_key.as_bytes()]);
        hash_field(&mut row_digest, source_key.as_bytes());
        hash_field(&mut row_digest, &meta.mtime.to_be_bytes());
        hash_field(&mut row_digest, &meta.size.to_be_bytes());
        hash_field(
            &mut row_digest,
            meta.mat_version.as_deref().unwrap_or("").as_bytes(),
        );
        let (source_kind, project_id, selector, relative_path, entry_key) = match meta.source {
            FileMetaSource::LegacyFilesystem => (
                CodeIndexMetadataSourceKindV1::LegacyFilesystem,
                None,
                None,
                None,
                None,
            ),
            FileMetaSource::LocalProjectFile {
                project_id,
                selector,
                relative_path,
                entry_key,
            } => {
                if !valid_token(&project_id)
                    || !valid_token(&selector)
                    || !strict_relative_path(Path::new(&relative_path))
                    || !strict_relative_path(Path::new(&entry_key))
                {
                    return corrupt_code_metadata("code_index_metadata_local_source_invalid");
                }
                (
                    CodeIndexMetadataSourceKindV1::LocalProjectFile,
                    Some(project_id),
                    Some(selector),
                    Some(relative_path),
                    Some(entry_key),
                )
            }
        };
        if meta
            .mat_version
            .as_deref()
            .is_some_and(|value| !valid_bounded_text(value, 512))
        {
            return corrupt_code_metadata("code_index_metadata_version_invalid");
        }
        hash_field(
            &mut row_digest,
            match source_kind {
                CodeIndexMetadataSourceKindV1::LegacyFilesystem => b"legacy_filesystem",
                CodeIndexMetadataSourceKindV1::LocalProjectFile => b"local_project_file",
            },
        );
        for value in [
            project_id.as_deref(),
            selector.as_deref(),
            relative_path.as_deref(),
            entry_key.as_deref(),
        ] {
            hash_field(&mut row_digest, value.unwrap_or("").as_bytes());
        }
        total_string_bytes = match total_string_bytes.checked_add(
            source_key.len()
                + meta.mat_version.as_deref().map(str::len).unwrap_or(0)
                + project_id.as_deref().map(str::len).unwrap_or(0)
                + selector.as_deref().map(str::len).unwrap_or(0)
                + relative_path.as_deref().map(str::len).unwrap_or(0)
                + entry_key.as_deref().map(str::len).unwrap_or(0),
        ) {
            Some(value) if value <= limits.max_total_string_bytes => value,
            _ => return corrupt_code_metadata("code_index_metadata_string_byte_limit"),
        };
        rows.push(CodeIndexMetadataRowV1 {
            source_key_sha256,
            source_kind,
            mtime: meta.mtime,
            size: meta.size,
            materialization_version: meta.mat_version,
            project_id,
            selector,
            relative_path,
            entry_key,
        });
    }
    rows.sort_by(|left, right| left.source_key_sha256.cmp(&right.source_key_sha256));
    let project_scoped_row_count =
        rows.iter().filter(|row| row.project_id.is_some()).count() as u64;
    CodeIndexMetadataMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Present,
        schema_fingerprint_sha256: code_metadata_schema_fingerprint(),
        source_fingerprint_sha256: Some(domain_hash(CODE_META_SOURCE_HASH_DOMAIN, [&bytes[..]])),
        row_count: rows.len() as u64,
        project_scoped_row_count,
        row_commitment_sha256: hex::encode(row_digest.finalize()),
        rows,
    }
}

fn code_metadata_schema_fingerprint() -> String {
    domain_hash(
        CODE_META_SCHEMA_HASH_DOMAIN,
        [
            b"file-meta-v1".as_slice(),
            b"source-key+mtime+size+mat-version+source-kind+project+selector+relative-path+entry-key"
                .as_slice(),
        ],
    )
}

fn missing_code_metadata() -> CodeIndexMetadataMigrationSnapshotV1 {
    CodeIndexMetadataMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Missing,
        schema_fingerprint_sha256: code_metadata_schema_fingerprint(),
        source_fingerprint_sha256: None,
        row_count: 0,
        project_scoped_row_count: 0,
        row_commitment_sha256: empty_hash(CODE_META_ROW_HASH_DOMAIN),
        rows: Vec::new(),
    }
}

fn corrupt_code_metadata(code: &'static str) -> CodeIndexMetadataMigrationSnapshotV1 {
    let mut snapshot = missing_code_metadata();
    snapshot.state = CorpusMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot
}

fn unavailable_code_metadata(code: &'static str) -> CodeIndexMetadataMigrationSnapshotV1 {
    let mut snapshot = missing_code_metadata();
    snapshot.state = CorpusMigrationSourceStateV1::Unavailable {
        diagnostic_code: code,
    };
    snapshot
}

fn capture_index(
    index_path: &Path,
    limits: CorpusMigrationSnapshotLimitsV1,
) -> CorpusIndexMigrationSnapshotV1 {
    if !strict_absolute_path(index_path) {
        return corrupt_index("corpus_index_path_invalid");
    }
    match fs::symlink_metadata(index_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_index(),
        Err(_) => return unavailable_index("corpus_index_metadata_unavailable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_index("corpus_index_path_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_index("corpus_index_path_not_directory");
        }
        Ok(_) => {}
    }
    match directory_has_symlink(index_path) {
        Ok(true) => return corrupt_index("corpus_index_entry_symlinked"),
        Ok(false) => {}
        Err(code) => return unavailable_index(code),
    }

    let marker_path = index_path.join(SCHEMA_VERSION_FILE);
    let marker = match read_regular_bounded(&marker_path, limits.max_source_file_bytes) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return corrupt_index("corpus_index_schema_marker_missing"),
        Err(CaptureReadFailure::Corrupt(code)) => return corrupt_index(code),
        Err(CaptureReadFailure::Unavailable(code)) => return unavailable_index(code),
    };
    let schema_version = match String::from_utf8(marker) {
        Ok(value) if value.trim() == INDEX_SCHEMA_VERSION => value.trim().to_string(),
        _ => return corrupt_index("corpus_index_schema_marker_invalid"),
    };

    let index = match Index::open_in_dir(index_path) {
        Ok(index) => index,
        Err(_) => return unavailable_index("corpus_index_open_unavailable"),
    };
    let schema = index.schema();
    let schema_bytes = match serde_json::to_vec(&schema) {
        Ok(bytes) => bytes,
        Err(_) => return corrupt_index("corpus_index_schema_encode_failed"),
    };
    let schema_fingerprint = domain_hash(INDEX_SCHEMA_HASH_DOMAIN, [&schema_bytes[..]]);
    let entity_id = match schema.get_field("entity_id") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_entity_field_missing"),
    };
    let project_id = match schema.get_field("project_id") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_project_field_missing"),
    };
    let doc_type = match schema.get_field("doc_type") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_doc_type_field_missing"),
    };
    let repo_id = match schema.get_field("repo_id") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_repo_field_missing"),
    };
    let commit_sha = match schema.get_field("commit_sha") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_commit_field_missing"),
    };
    let chunk_hash = match schema.get_field("chunk_hash") {
        Ok(field) => field,
        Err(_) => return corrupt_index("corpus_index_chunk_hash_field_missing"),
    };
    let reader = match index.reader() {
        Ok(reader) => reader,
        Err(_) => return unavailable_index("corpus_index_reader_unavailable"),
    };
    let searcher = reader.searcher();
    let document_count = searcher.num_docs();
    if document_count > limits.max_documents {
        return corrupt_index("corpus_index_document_limit");
    }
    let addresses = match searcher.search(&AllQuery, &DocSetCollector) {
        Ok(addresses) => addresses,
        Err(_) => return unavailable_index("corpus_index_scan_unavailable"),
    };

    let mut project_refs = BTreeMap::<(String, String), u64>::new();
    let mut commit_rows = Vec::<CommitRowV1>::new();
    let mut total_string_bytes = 0usize;
    for address in addresses {
        let document: TantivyDocument = match searcher.doc(address) {
            Ok(document) => document,
            Err(_) => return unavailable_index("corpus_index_document_unavailable"),
        };
        let entity = optional_text(&document, entity_id).unwrap_or_default();
        let project = optional_text(&document, project_id).unwrap_or_default();
        if !entity.is_empty() && !project.is_empty() {
            if project_id_from_entity_ref(&entity) != Some(project.as_str()) {
                return corrupt_index("corpus_index_project_ref_invalid");
            }
            total_string_bytes = match total_string_bytes.checked_add(entity.len() + project.len())
            {
                Some(value) if value <= limits.max_total_string_bytes => value,
                _ => return corrupt_index("corpus_index_string_byte_limit"),
            };
            *project_refs.entry((project, entity)).or_default() += 1;
            if project_refs.len() > limits.max_project_scoped_refs {
                return corrupt_index("corpus_index_project_ref_limit");
            }
        }
        if optional_text(&document, doc_type).as_deref() != Some("commit") {
            continue;
        }
        let namespace = optional_text(&document, repo_id).unwrap_or_default();
        let sha = optional_text(&document, commit_sha).unwrap_or_default();
        let entity = optional_text(&document, entity_id).unwrap_or_default();
        let content_hash = optional_text(&document, chunk_hash).unwrap_or_default();
        if namespace.is_empty() || sha.is_empty() || entity.is_empty() || content_hash.is_empty() {
            return corrupt_index("corpus_index_commit_row_incomplete");
        }
        if !matches!(
            EntityRef::parse(&entity),
            Ok(EntityRef::Commit {
                repo_id,
                sha: entity_sha,
            }) if repo_id == namespace && entity_sha == sha
        ) {
            return corrupt_index("corpus_index_commit_row_invalid");
        }
        total_string_bytes = match total_string_bytes
            .checked_add(namespace.len() + sha.len() + entity.len() + content_hash.len())
        {
            Some(value) if value <= limits.max_total_string_bytes => value,
            _ => return corrupt_index("corpus_index_string_byte_limit"),
        };
        commit_rows.push(CommitRowV1 {
            namespace,
            entity_ref: entity,
            commit_sha: sha,
            content_hash,
        });
    }

    let project_scoped_refs = project_refs
        .into_iter()
        .map(
            |((project_id, entity_ref), document_count)| CorpusProjectScopedRefV1 {
                project_id,
                entity_ref,
                document_count,
            },
        )
        .collect::<Vec<_>>();
    let project_ref_count = project_scoped_refs
        .iter()
        .map(|row| row.document_count)
        .sum::<u64>();
    let project_ref_commitment = hash_project_ref_rows(&project_scoped_refs);

    commit_rows.sort();
    let mut commits_by_namespace = BTreeMap::<String, Vec<CommitRowV1>>::new();
    for row in commit_rows {
        commits_by_namespace
            .entry(row.namespace.clone())
            .or_default()
            .push(row);
    }
    if commits_by_namespace.len() > limits.max_commit_namespaces {
        return corrupt_index("corpus_index_commit_namespace_limit");
    }
    let commit_namespaces = commits_by_namespace
        .into_iter()
        .map(|(namespace, rows)| CorpusCommitNamespaceV1 {
            namespace,
            commit_document_count: rows.len() as u64,
            commit_document_commitment_sha256: hash_commit_rows(&rows),
        })
        .collect::<Vec<_>>();

    let mut source = Sha256::new();
    source.update(INDEX_SOURCE_HASH_DOMAIN);
    hash_field(&mut source, schema_version.as_bytes());
    hash_field(&mut source, schema_fingerprint.as_bytes());
    hash_field(&mut source, &document_count.to_be_bytes());
    hash_field(&mut source, project_ref_commitment.as_bytes());
    for namespace in &commit_namespaces {
        hash_field(&mut source, namespace.namespace.as_bytes());
        hash_field(&mut source, &namespace.commit_document_count.to_be_bytes());
        hash_field(
            &mut source,
            namespace.commit_document_commitment_sha256.as_bytes(),
        );
    }

    CorpusIndexMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Present,
        schema_version: Some(schema_version),
        schema_fingerprint_sha256: Some(schema_fingerprint),
        source_fingerprint_sha256: Some(hex::encode(source.finalize())),
        document_count,
        project_scoped_ref_count: project_ref_count,
        project_scoped_ref_commitment_sha256: project_ref_commitment,
        project_scoped_refs,
        commit_namespaces,
    }
}

fn capture_git_cursors(
    git_meta_dir: &Path,
    limits: CorpusMigrationSnapshotLimitsV1,
) -> GitCursorMigrationSnapshotV1 {
    if !strict_absolute_path(git_meta_dir) {
        return corrupt_git("git_cursor_path_invalid");
    }
    match fs::symlink_metadata(git_meta_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_git(),
        Err(_) => return unavailable_git("git_cursor_metadata_unavailable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_git("git_cursor_path_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_git("git_cursor_path_not_directory");
        }
        Ok(_) => {}
    }
    let entries = match fs::read_dir(git_meta_dir) {
        Ok(entries) => entries,
        Err(_) => return unavailable_git("git_cursor_directory_unavailable"),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return unavailable_git("git_cursor_entry_unavailable"),
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => return unavailable_git("git_cursor_entry_unavailable"),
        };
        if file_type.is_symlink() {
            return corrupt_git("git_cursor_entry_symlinked");
        }
        if !file_type.is_file() {
            continue;
        }
        let Some(file_name) = entry.file_name().to_str().map(str::to_string) else {
            return corrupt_git("git_cursor_filename_invalid");
        };
        if !file_name.ends_with(".json") {
            continue;
        }
        paths.push((file_name, entry.path()));
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    if paths.len() > limits.max_git_cursor_files {
        return corrupt_git("git_cursor_file_limit");
    }

    let mut rows = Vec::with_capacity(paths.len());
    let mut source = Sha256::new();
    source.update(GIT_CURSOR_SOURCE_HASH_DOMAIN);
    let mut total_string_bytes = 0usize;
    for (file_name, path) in paths {
        let Some(project_id) = file_name.strip_suffix(".json") else {
            continue;
        };
        if !valid_token(project_id) {
            return corrupt_git("git_cursor_project_id_invalid");
        }
        let bytes = match read_regular_bounded(&path, limits.max_source_file_bytes) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return corrupt_git("git_cursor_source_missing"),
            Err(CaptureReadFailure::Corrupt(code)) => return corrupt_git(code),
            Err(CaptureReadFailure::Unavailable(code)) => return unavailable_git(code),
        };
        let meta: GitIngestMetaV1 = match serde_json::from_slice(&bytes) {
            Ok(meta) => meta,
            Err(_) => return corrupt_git("git_cursor_decode_failed"),
        };
        if meta
            .last_ingested_sha
            .as_deref()
            .is_some_and(|sha| !valid_commit_sha(sha))
        {
            return corrupt_git("git_cursor_commit_invalid");
        }
        total_string_bytes = match total_string_bytes.checked_add(
            project_id.len() + meta.last_ingested_sha.as_deref().map(str::len).unwrap_or(0),
        ) {
            Some(value) if value <= limits.max_total_string_bytes => value,
            _ => return corrupt_git("git_cursor_string_byte_limit"),
        };
        hash_field(&mut source, file_name.as_bytes());
        hash_field(&mut source, &bytes);
        rows.push(GitCursorMigrationRowV1 {
            project_id: project_id.to_string(),
            last_ingested_sha: meta.last_ingested_sha,
        });
    }
    let row_commitment = hash_git_cursor_rows(&rows);
    GitCursorMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Present,
        schema_fingerprint_sha256: git_cursor_schema_fingerprint(),
        source_fingerprint_sha256: Some(hex::encode(source.finalize())),
        row_count: rows.len() as u64,
        row_commitment_sha256: row_commitment,
        rows,
    }
}

fn missing_index() -> CorpusIndexMigrationSnapshotV1 {
    CorpusIndexMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Missing,
        schema_version: None,
        schema_fingerprint_sha256: None,
        source_fingerprint_sha256: None,
        document_count: 0,
        project_scoped_ref_count: 0,
        project_scoped_ref_commitment_sha256: empty_hash(INDEX_ROW_HASH_DOMAIN),
        project_scoped_refs: Vec::new(),
        commit_namespaces: Vec::new(),
    }
}

fn corrupt_index(code: &'static str) -> CorpusIndexMigrationSnapshotV1 {
    let mut snapshot = missing_index();
    snapshot.state = CorpusMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot
}

fn unavailable_index(code: &'static str) -> CorpusIndexMigrationSnapshotV1 {
    let mut snapshot = missing_index();
    snapshot.state = CorpusMigrationSourceStateV1::Unavailable {
        diagnostic_code: code,
    };
    snapshot
}

fn missing_git() -> GitCursorMigrationSnapshotV1 {
    GitCursorMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: CorpusMigrationSourceStateV1::Missing,
        schema_fingerprint_sha256: git_cursor_schema_fingerprint(),
        source_fingerprint_sha256: None,
        row_count: 0,
        row_commitment_sha256: empty_hash(GIT_CURSOR_ROW_HASH_DOMAIN),
        rows: Vec::new(),
    }
}

fn git_cursor_schema_fingerprint() -> String {
    domain_hash(
        GIT_CURSOR_SCHEMA_HASH_DOMAIN,
        [b"git-ingest-meta-v1:last_ingested_sha".as_slice()],
    )
}

fn corrupt_git(code: &'static str) -> GitCursorMigrationSnapshotV1 {
    let mut snapshot = missing_git();
    snapshot.state = CorpusMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot
}

fn unavailable_git(code: &'static str) -> GitCursorMigrationSnapshotV1 {
    let mut snapshot = missing_git();
    snapshot.state = CorpusMigrationSourceStateV1::Unavailable {
        diagnostic_code: code,
    };
    snapshot
}

fn hash_project_ref_rows(rows: &[CorpusProjectScopedRefV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(INDEX_ROW_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.project_id.as_bytes());
        hash_field(&mut digest, row.entity_ref.as_bytes());
        hash_field(&mut digest, &row.document_count.to_be_bytes());
    }
    hex::encode(digest.finalize())
}

fn hash_commit_rows(rows: &[CommitRowV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(COMMIT_ROW_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.namespace.as_bytes());
        hash_field(&mut digest, row.entity_ref.as_bytes());
        hash_field(&mut digest, row.commit_sha.as_bytes());
        hash_field(&mut digest, row.content_hash.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn hash_git_cursor_rows(rows: &[GitCursorMigrationRowV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(GIT_CURSOR_ROW_HASH_DOMAIN);
    for row in rows {
        hash_field(&mut digest, row.project_id.as_bytes());
        hash_field(
            &mut digest,
            row.last_ingested_sha.as_deref().unwrap_or("").as_bytes(),
        );
    }
    hex::encode(digest.finalize())
}

fn domain_hash<'a>(domain: &[u8], fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for field in fields {
        hash_field(&mut digest, field);
    }
    hex::encode(digest.finalize())
}

fn empty_hash(domain: &[u8]) -> String {
    hex::encode(Sha256::new().chain_update(domain).finalize())
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn strict_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn strict_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.to_string_lossy().contains('\\')
        && !path.to_string_lossy().chars().any(char::is_control)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn directory_has_symlink(path: &Path) -> Result<bool, &'static str> {
    let Ok(entries) = fs::read_dir(path) else {
        return Err("corpus_index_directory_unavailable");
    };
    for entry in entries {
        let entry = entry.map_err(|_| "corpus_index_entry_unavailable")?;
        let kind = entry
            .file_type()
            .map_err(|_| "corpus_index_entry_unavailable")?;
        if kind.is_symlink() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureReadFailure {
    Corrupt(&'static str),
    Unavailable(&'static str),
}

fn read_regular_bounded(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, CaptureReadFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(CaptureReadFailure::Unavailable(
                "owner_source_metadata_unavailable",
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(CaptureReadFailure::Corrupt("owner_source_symlinked"));
    }
    if !metadata.is_file() {
        return Err(CaptureReadFailure::Corrupt("owner_source_not_regular"));
    }
    if metadata.len() > max_bytes {
        return Err(CaptureReadFailure::Corrupt("owner_source_byte_limit"));
    }
    let file = fs::File::open(path)
        .map_err(|_| CaptureReadFailure::Unavailable("owner_source_open_unavailable"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| CaptureReadFailure::Unavailable("owner_source_read_unavailable"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(CaptureReadFailure::Corrupt("owner_source_byte_limit"));
    }
    Ok(Some(bytes))
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains(['/', '\\'])
        && !matches!(value, "." | "..")
        && !value.chars().any(char::is_control)
}

fn valid_bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn project_id_from_entity_ref(entity_ref: &str) -> Option<&str> {
    let parsed_project_id = match EntityRef::parse(entity_ref).ok()? {
        EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => project_id,
        _ => return None,
    };
    let (_, rest) = entity_ref.split_once(':')?;
    let project_id = rest.split(':').next()?;
    (project_id == parsed_project_id).then_some(project_id)
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::TantivyDocument;

    #[test]
    fn missing_sources_are_typed_and_never_created() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let index = root.join("missing-index");
        let git = root.join("missing-git");

        let snapshot = capture_owner_migration_snapshot_no_create(
            &index,
            &git,
            CorpusMigrationSnapshotLimitsV1::default(),
        );

        assert_eq!(snapshot.index.state, CorpusMigrationSourceStateV1::Missing);
        assert_eq!(
            snapshot.git_cursors.state,
            CorpusMigrationSourceStateV1::Missing
        );
        assert!(!index.exists());
        assert!(!git.exists());
    }

    #[test]
    fn captures_project_refs_commit_namespaces_and_git_cursors() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let projects_path = root.join("projects.json");
        let index = super::super::TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            projects_path,
            root.join("knowledge.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(20_000_000).unwrap();

        let mut project_doc = TantivyDocument::new();
        project_doc.add_text(fields.doc_type, "project_file");
        project_doc.add_text(fields.project_id, "project-a");
        project_doc.add_text(
            fields.entity_id,
            "project_file:project-a:path-hash:chunk-hash:0",
        );
        writer.add_document(project_doc).unwrap();

        for sha in [
            "1111111111111111111111111111111111111111",
            "2222222222222222222222222222222222222222",
        ] {
            let mut commit_doc = TantivyDocument::new();
            commit_doc.add_text(fields.doc_type, "commit");
            commit_doc.add_text(fields.repo_id, "legacy-namespace");
            commit_doc.add_text(fields.commit_sha, sha);
            commit_doc.add_text(fields.chunk_hash, format!("message-{sha}"));
            commit_doc.add_text(fields.entity_id, format!("commit:legacy-namespace:{sha}"));
            writer.add_document(commit_doc).unwrap();
        }
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let git_meta = root.join("git_meta");
        fs::create_dir(&git_meta).unwrap();
        fs::write(
            git_meta.join("project-a.json"),
            br#"{"last_ingested_sha":"1111111111111111111111111111111111111111"}"#,
        )
        .unwrap();
        fs::write(
            index_path.join("_meta.json"),
            serde_json::to_vec(&serde_json::json!({
                "local:project-a:src/lib.rs": {
                    "mtime": 1,
                    "size": 42,
                    "mat_version": "materialization-v1",
                    "source": {
                        "kind": "local_project_file",
                        "project_id": "project-a",
                        "selector": "selector-a",
                        "relative_path": "src/lib.rs",
                        "entry_key": "src/lib.rs"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let snapshot = capture_owner_migration_snapshot_no_create(
            &index_path,
            &git_meta,
            CorpusMigrationSnapshotLimitsV1::default(),
        );

        assert_eq!(snapshot.index.state, CorpusMigrationSourceStateV1::Present);
        assert_eq!(snapshot.index.project_scoped_ref_count, 1);
        assert_eq!(snapshot.index.commit_namespaces.len(), 1);
        assert_eq!(snapshot.index.commit_namespaces[0].commit_document_count, 2);
        assert_eq!(
            snapshot.git_cursors.rows,
            vec![GitCursorMigrationRowV1 {
                project_id: "project-a".to_string(),
                last_ingested_sha: Some("1111111111111111111111111111111111111111".to_string()),
            }]
        );
        assert_eq!(
            snapshot.code_metadata.state,
            CorpusMigrationSourceStateV1::Present
        );
        assert_eq!(snapshot.code_metadata.project_scoped_row_count, 1);
        assert_eq!(
            snapshot.code_metadata.rows[0].selector.as_deref(),
            Some("selector-a")
        );
    }

    #[test]
    fn truncated_git_cursor_and_source_limit_are_distinct_corrupt_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let index = root.join("missing-index");
        let git = root.join("git-meta");
        fs::create_dir(&git).unwrap();
        fs::write(git.join("project-a.json"), b"{truncated").unwrap();

        let truncated = capture_owner_migration_snapshot_no_create(
            &index,
            &git,
            CorpusMigrationSnapshotLimitsV1::default(),
        );
        assert!(matches!(
            truncated.git_cursors.state,
            CorpusMigrationSourceStateV1::Corrupt {
                diagnostic_code: "git_cursor_decode_failed"
            }
        ));

        let mut limits = CorpusMigrationSnapshotLimitsV1::default();
        limits.max_source_file_bytes = 1;
        let oversized = capture_owner_migration_snapshot_no_create(&index, &git, limits);
        assert!(matches!(
            oversized.git_cursors.state,
            CorpusMigrationSourceStateV1::Corrupt {
                diagnostic_code: "owner_source_byte_limit"
            }
        ));
    }
}
