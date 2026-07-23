//! Shared provenance Git-note protocol and checkout-local application.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::{PublishedScope, bbox_root_relpath, resolve_recorded_repo_id};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SCHEMA_VERSION_V1: u32 = 1;
pub const SCHEMA_VERSION_V2: u32 = 2;
pub const MAX_NOTE_DOCUMENT_BYTES: usize = 24 * 1024;
pub const MAX_PAGE_DOCUMENTS: usize = 64;
pub const MAX_PAGE_DOCUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitProvenanceNote {
    #[serde(default = "v1_schema_version")]
    pub schema_version: u32,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<GitProvenanceNotePart>,
    pub produced_by: ProducedBy,
    pub tool_calls: Vec<NoteToolCall>,
    #[serde(default)]
    pub knowledge_writes: Vec<KnowledgeWrite>,
}

impl GitProvenanceNote {
    pub fn new_v2(
        commit: impl Into<String>,
        produced_by: ProducedBy,
        tool_calls: Vec<NoteToolCall>,
        knowledge_writes: Vec<KnowledgeWrite>,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION_V2,
            commit: commit.into(),
            part: None,
            produced_by,
            tool_calls,
            knowledge_writes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitProvenanceNotePart {
    pub document_id: String,
    pub part_index: u32,
    pub part_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProducedBy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub brofiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arc_thread_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteToolCall {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<[u64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct KnowledgeWrite {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceExportDocument {
    pub commit: String,
    pub part_index: u32,
    pub document: String,
    pub document_sha256: String,
}

impl ProvenanceExportDocument {
    pub fn from_note(note: &GitProvenanceNote) -> Result<Self> {
        let document = serialize_note(note)?;
        Ok(Self {
            commit: note.commit.clone(),
            part_index: note.part.as_ref().map_or(0, |part| part.part_index),
            document_sha256: document_sha256(&document),
            document,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceExportPlan {
    pub scope: PublishedScope,
    pub project_id: String,
    pub notes_ref: String,
    pub generation: String,
    pub documents: Vec<ProvenanceExportDocument>,
}

impl ProvenanceExportPlan {
    pub fn new(
        scope: PublishedScope,
        project_id: impl Into<String>,
        notes_ref: impl Into<String>,
        mut documents: Vec<ProvenanceExportDocument>,
    ) -> Result<Self> {
        let project_id = project_id.into();
        let notes_ref = notes_ref.into();
        if project_id.trim().is_empty() {
            bail!("provenance export project_id must not be empty");
        }
        validate_notes_ref(&notes_ref)?;
        documents.sort_by(|left, right| {
            (&left.commit, left.part_index).cmp(&(&right.commit, right.part_index))
        });
        let mut keys = BTreeSet::new();
        for document in &documents {
            validate_export_document(document)?;
            if !keys.insert((document.commit.as_str(), document.part_index)) {
                bail!("provenance export plan contains a duplicate commit part");
            }
        }
        let generation = plan_generation(&project_id, &notes_ref, &documents)?;
        Ok(Self {
            scope,
            project_id,
            notes_ref,
            generation,
            documents,
        })
    }

    pub fn page(
        &self,
        documents: Vec<ProvenanceExportDocument>,
        next_cursor: Option<String>,
    ) -> ProvenanceExportPage {
        ProvenanceExportPage {
            scope: self.scope.clone(),
            project_id: self.project_id.clone(),
            notes_ref: self.notes_ref.clone(),
            generation: self.generation.clone(),
            documents,
            next_cursor,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceExportPage {
    pub scope: PublishedScope,
    pub project_id: String,
    pub notes_ref: String,
    pub generation: String,
    pub documents: Vec<ProvenanceExportDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyExportPageResult {
    pub written: u64,
    pub unchanged: u64,
    pub rejected: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FragmentError {
    #[error("provenance note must use schema version 2 before fragmentation")]
    UnsupportedSchema,
    #[error("provenance note base exceeds the {max_bytes}-byte document limit")]
    DocumentBaseTooLarge { max_bytes: usize },
    #[error(
        "provenance tool call at index {index} cannot fit within the {max_bytes}-byte document limit"
    )]
    ToolCallTooLarge { index: usize, max_bytes: usize },
    #[error("serializing provenance note: {0}")]
    Serialization(String),
    #[error("provenance note has too many parts")]
    TooManyParts,
}

pub fn serialize_note(note: &GitProvenanceNote) -> Result<String> {
    Ok(serde_json::to_string_pretty(note)?)
}

pub fn parse_note_document(raw: &str) -> Result<GitProvenanceNote> {
    let note: GitProvenanceNote = serde_json::from_str(raw.trim())?;
    match note.schema_version {
        SCHEMA_VERSION_V1 | SCHEMA_VERSION_V2 => Ok(note),
        version => bail!("unsupported provenance note schema version {version}"),
    }
}

pub fn split_note_documents(raw: &str) -> Vec<&str> {
    raw.split(bbox_corpus_core::git::NOTE_DOCUMENT_SEPARATOR)
        .map(str::trim)
        .filter(|document| !document.is_empty())
        .collect()
}

pub fn document_sha256(document: &str) -> String {
    sha256_hex(document.as_bytes())
}

pub fn fragment_note(
    note: &GitProvenanceNote,
    max_bytes: usize,
) -> std::result::Result<Vec<GitProvenanceNote>, FragmentError> {
    if note.schema_version != SCHEMA_VERSION_V2 {
        return Err(FragmentError::UnsupportedSchema);
    }
    let mut logical = note.clone();
    logical.part = None;
    let logical_document = serialize_for_fragment(&logical)?;
    let document_id = document_sha256(&logical_document);

    let upper_part_count =
        u32::try_from(logical.tool_calls.len().max(1)).map_err(|_| FragmentError::TooManyParts)?;
    let conservative_index = upper_part_count.saturating_sub(1);

    let mut groups = Vec::<Vec<NoteToolCall>>::new();
    if logical.tool_calls.is_empty() {
        let candidate = note_part(&logical, &document_id, 0, 1, Vec::new());
        if serialize_for_fragment(&candidate)?.len() > max_bytes {
            return Err(FragmentError::DocumentBaseTooLarge { max_bytes });
        }
        groups.push(Vec::new());
    } else {
        let mut current = Vec::<NoteToolCall>::new();
        for (index, call) in logical.tool_calls.iter().cloned().enumerate() {
            let mut candidate_calls = current.clone();
            candidate_calls.push(call.clone());
            let candidate = note_part(
                &logical,
                &document_id,
                conservative_index,
                upper_part_count,
                candidate_calls,
            );
            if serialize_for_fragment(&candidate)?.len() <= max_bytes {
                current.push(call);
                continue;
            }
            if current.is_empty() {
                return Err(FragmentError::ToolCallTooLarge { index, max_bytes });
            }
            groups.push(std::mem::take(&mut current));
            let single = note_part(
                &logical,
                &document_id,
                conservative_index,
                upper_part_count,
                vec![call.clone()],
            );
            if serialize_for_fragment(&single)?.len() > max_bytes {
                return Err(FragmentError::ToolCallTooLarge { index, max_bytes });
            }
            current.push(call);
        }
        if !current.is_empty() {
            groups.push(current);
        }
    }

    let part_count = u32::try_from(groups.len()).map_err(|_| FragmentError::TooManyParts)?;
    groups
        .into_iter()
        .enumerate()
        .map(|(index, calls)| {
            let part_index = u32::try_from(index).map_err(|_| FragmentError::TooManyParts)?;
            let part = note_part(&logical, &document_id, part_index, part_count, calls);
            let size = serialize_for_fragment(&part)?.len();
            if size > max_bytes {
                return Err(FragmentError::DocumentBaseTooLarge { max_bytes });
            }
            Ok(part)
        })
        .collect()
}

pub fn validate_notes_ref(notes_ref: &str) -> Result<()> {
    let mut components = notes_ref.split('/');
    let valid_prefix = components.next() == Some("refs") && components.next() == Some("notes");
    let namespace = components.next().unwrap_or_default();
    let valid_suffix = components.next() == Some("provenance") && components.next().is_none();
    if !valid_prefix
        || !valid_suffix
        || !is_safe_notes_namespace(namespace)
        || !git_check_ref_format(notes_ref)
    {
        bail!("invalid provenance notes ref: expected refs/notes/<safe-namespace>/provenance");
    }
    Ok(())
}

pub fn resolve_committed_scope(root: &Path) -> Result<PublishedScope> {
    let root = canonical_project_root(root)?;
    let git_root = bbox_corpus_core::git::git_root_for_path(&root)
        .ok_or_else(|| anyhow!("{} is not inside a Git repository", root.display()))?
        .canonicalize()
        .with_context(|| "canonicalizing provenance repository root")?;
    let inputs = bbox_config::config::read_repo_id_inputs_at_ref(&root, "HEAD")
        .with_context(|| "reading committed project identity at HEAD")?;
    let repo_id = resolve_recorded_repo_id(&inputs).ok_or_else(|| {
        anyhow!(
            "committed .bbox/config.toml must record project.repo_id or project.project_key_override"
        )
    })?;
    let bbox_root_relpath = bbox_root_relpath(&git_root, &root)
        .ok_or_else(|| anyhow!("project root is outside its Git repository"))?;
    Ok(PublishedScope::try_new(repo_id, bbox_root_relpath)?)
}

pub fn apply_export_page(
    root: &Path,
    page: &ProvenanceExportPage,
) -> Result<ApplyExportPageResult> {
    let root = canonical_project_root(root)?;
    validate_page(&root, page)?;
    let _repository_lock = lock_repository(&root)?;

    let mut known_hashes = BTreeMap::<String, BTreeSet<String>>::new();
    for document in &page.documents {
        if known_hashes.contains_key(&document.commit) {
            continue;
        }
        let existing = bbox_corpus_core::git::show_note(&root, &page.notes_ref, &document.commit)?;
        let hashes = existing
            .as_deref()
            .map(split_note_documents)
            .unwrap_or_default()
            .into_iter()
            .map(document_sha256)
            .collect();
        known_hashes.insert(document.commit.clone(), hashes);
    }

    let mut pending = Vec::new();
    let mut unchanged = 0u64;
    for document in &page.documents {
        let hashes = known_hashes
            .get_mut(&document.commit)
            .expect("validated commit hash set");
        if !hashes.insert(document.document_sha256.clone()) {
            unchanged += 1;
            continue;
        }
        pending.push(document);
    }

    if pending.is_empty() {
        return Ok(ApplyExportPageResult {
            written: 0,
            unchanged,
            rejected: 0,
        });
    }

    bbox_corpus_core::git::ensure_notes_merge_strategy_union(&root)?;
    for document in &pending {
        let body = format!("{}\n", document.document);
        bbox_corpus_core::git::write_note(&root, &page.notes_ref, &document.commit, &body)?;
    }

    Ok(ApplyExportPageResult {
        written: pending.len() as u64,
        unchanged,
        rejected: 0,
    })
}

/// Capture every document under one explicit provenance notes ref.
///
/// The note blobs are read by the immutable object ids returned from one
/// `git notes list` invocation. The reader therefore cannot mix documents
/// across ref generations and does not need to create the writer lock.
pub fn capture_project_catalog_owner_snapshot(
    root: &Path,
    notes_ref: &str,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    let repository_authority =
        match bbox_corpus_core::json_store::NofollowDirectory::open_existing(root) {
            Ok(Some(authority)) => authority,
            _ => {
                return bbox_corpus_core::project_catalog_snapshot::corrupt_owner_snapshot(
                    "provenance",
                    "provenance:repository",
                    "provenance_repository_unsafe",
                    limits,
                );
            }
        };
    let repository = match bbox_corpus_core::git::open_stable_git_repository(&repository_authority)
    {
        Ok(Some(repository)) => repository,
        _ => {
            return bbox_corpus_core::project_catalog_snapshot::corrupt_owner_snapshot(
                "provenance",
                "provenance:repository",
                "provenance_repository_unsafe",
                limits,
            );
        }
    };
    capture_project_catalog_owner_snapshot_stable(&repository, notes_ref, project_id, limits)
}

pub fn capture_project_catalog_owner_snapshot_stable(
    repository: &bbox_corpus_core::git::StableGitRepository,
    notes_ref: &str,
    project_id: &str,
    limits: bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotLimitsV1,
) -> std::result::Result<
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotV1,
    bbox_corpus_core::project_catalog_snapshot::OwnerSnapshotError,
> {
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotRowV1, OwnerSnapshotStateV1, build_owner_snapshot, corrupt_owner_snapshot,
        finalize_owner_snapshot, missing_owner_snapshot, owner_subsource,
    };

    if project_id.trim().is_empty() || validate_notes_ref(notes_ref).is_err() {
        return corrupt_owner_snapshot(
            "provenance",
            "provenance:notes-ref",
            "provenance_capture_input_invalid",
            limits,
        );
    }
    let note_entries = match repository.snapshot_notes_bounded(
        notes_ref,
        limits.max_subsources,
        limits.max_source_bytes,
    ) {
        Ok(Some(entries)) => entries,
        Ok(None) => {
            return missing_owner_snapshot("provenance", "provenance:notes-ref", limits);
        }
        Err(_) => {
            return corrupt_owner_snapshot(
                "provenance",
                "provenance:notes-ref",
                "provenance_notes_snapshot_unreadable",
                limits,
            );
        }
    };
    let mut listing_commitment = Vec::new();
    for entry in &note_entries {
        listing_commitment.extend_from_slice(entry.target_oid.as_bytes());
        listing_commitment.push(0);
        listing_commitment.extend_from_slice(sha256_hex(&entry.bytes).as_bytes());
        listing_commitment.push(b'\n');
    }
    if note_entries.is_empty() {
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(&listing_commitment),
            byte_len: 0,
        };
        return build_owner_snapshot(
            "provenance",
            vec![owner_subsource("provenance:notes-ref", state, &[])],
            Vec::new(),
            limits,
        );
    }
    let mut total_bytes = listing_commitment.len();
    let mut rows = Vec::new();
    let mut subsources = Vec::new();
    for entry in note_entries {
        let commit_oid = entry.target_oid;
        let bytes = entry.bytes;
        total_bytes = match total_bytes.checked_add(bytes.len()) {
            Some(total_bytes) => total_bytes,
            None => {
                return corrupt_owner_snapshot(
                    "provenance",
                    "provenance:notes-ref",
                    "owner_source_byte_limit",
                    limits,
                );
            }
        };
        if total_bytes > limits.max_source_bytes {
            return corrupt_owner_snapshot(
                "provenance",
                "provenance:notes-ref",
                "owner_source_byte_limit",
                limits,
            );
        }
        let body = match std::str::from_utf8(&bytes) {
            Ok(body) => body,
            Err(_) => {
                return corrupt_owner_snapshot(
                    "provenance",
                    &format!("provenance:{project_id}:{commit_oid}"),
                    "provenance_note_invalid",
                    limits,
                );
            }
        };
        let subsource_id = format!("provenance:{project_id}:{commit_oid}");
        let mut subsource_rows = Vec::new();
        for (index, document) in split_note_documents(body).into_iter().enumerate() {
            let note = match parse_note_document(document) {
                Ok(note) => note,
                Err(_) => {
                    return corrupt_owner_snapshot(
                        "provenance",
                        &subsource_id,
                        "provenance_note_invalid",
                        limits,
                    );
                }
            };
            if note.commit != commit_oid {
                return corrupt_owner_snapshot(
                    "provenance",
                    &subsource_id,
                    "provenance_note_commit_mismatch",
                    limits,
                );
            }
            let hash = document_sha256(document);
            subsource_rows.push(OwnerSnapshotRowV1::inventory_target(
                format!("{commit_oid}:{index}:{hash}"),
                project_id,
                hash,
            ));
        }
        let state = OwnerSnapshotStateV1::Present {
            content_sha256: sha256_hex(&bytes),
            byte_len: bytes.len() as u64,
        };
        subsources.push(owner_subsource(subsource_id, state, &subsource_rows));
        rows.extend(subsource_rows);
    }
    finalize_owner_snapshot(
        "provenance",
        "provenance:notes-ref",
        subsources,
        rows,
        limits,
    )
}

pub fn append_note_documents_dedup(
    root: &Path,
    notes_ref: &str,
    commit: &str,
    documents: &[String],
) -> Result<ApplyExportPageResult> {
    let root = canonical_project_root(root)?;
    validate_notes_ref(notes_ref)?;
    if !commit_exists(&root, commit)? {
        bail!("provenance target commit does not exist");
    }
    let _repository_lock = lock_repository(&root)?;
    let existing = bbox_corpus_core::git::show_note(&root, notes_ref, commit)?;
    let mut known_hashes = existing
        .as_deref()
        .map(split_note_documents)
        .unwrap_or_default()
        .into_iter()
        .map(document_sha256)
        .collect::<BTreeSet<_>>();
    let mut written = 0_u64;
    let mut unchanged = 0_u64;
    bbox_corpus_core::git::ensure_notes_merge_strategy_union(&root)?;
    for document in documents {
        if !known_hashes.insert(document_sha256(document)) {
            unchanged += 1;
            continue;
        }
        bbox_corpus_core::git::write_note(&root, notes_ref, commit, &format!("{document}\n"))?;
        written += 1;
    }
    Ok(ApplyExportPageResult {
        written,
        unchanged,
        rejected: 0,
    })
}

fn validate_page(root: &Path, page: &ProvenanceExportPage) -> Result<()> {
    if page.project_id.trim().is_empty() {
        bail!("provenance export page project_id must not be empty");
    }
    if page.generation.trim().is_empty() {
        bail!("provenance export page generation must not be empty");
    }
    validate_notes_ref(&page.notes_ref)?;
    let local_scope = resolve_committed_scope(root)?;
    if local_scope != page.scope {
        bail!("provenance export page scope does not match the committed local project scope");
    }

    let mut part_counts = BTreeMap::<(String, String), u32>::new();
    let mut part_hashes = BTreeMap::<(String, String, u32), String>::new();
    for document in &page.documents {
        validate_export_document(document)?;
        if !commit_exists(root, &document.commit)? {
            bail!(
                "provenance export commit {} does not exist locally",
                document.commit
            );
        }
        let note = parse_note_document(&document.document)
            .with_context(|| format!("parsing provenance document for {}", document.commit))?;
        if note.commit != document.commit {
            bail!("provenance document commit does not match its export key");
        }
        if note.schema_version == SCHEMA_VERSION_V2 {
            let part = note
                .part
                .as_ref()
                .ok_or_else(|| anyhow!("v2 provenance document is missing part metadata"))?;
            if part.document_id.len() != 64
                || !part
                    .document_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || part.part_count == 0
                || part.part_index != document.part_index
                || part.part_index >= part.part_count
            {
                bail!("provenance document part metadata does not match its export key");
            }
            let logical_key = (document.commit.clone(), part.document_id.clone());
            if part_counts
                .insert(logical_key.clone(), part.part_count)
                .is_some_and(|count| count != part.part_count)
            {
                bail!("provenance document parts disagree on part_count");
            }
            let part_key = (logical_key.0, logical_key.1, part.part_index);
            if part_hashes
                .insert(part_key, document.document_sha256.clone())
                .is_some_and(|hash| hash != document.document_sha256)
            {
                bail!("provenance document part index has conflicting content");
            }
            for call in &note.tool_calls {
                validate_v2_target(call, &page.project_id)?;
            }
        }
    }
    Ok(())
}

fn validate_export_document(document: &ProvenanceExportDocument) -> Result<()> {
    if !is_commit_id(&document.commit) {
        bail!("provenance export commit must be a hexadecimal object id");
    }
    let actual = document_sha256(&document.document);
    if actual != document.document_sha256 {
        bail!("provenance export document hash mismatch");
    }
    Ok(())
}

fn validate_v2_target(call: &NoteToolCall, project_id: &str) -> Result<()> {
    let target_ref = call
        .target_ref
        .as_deref()
        .ok_or_else(|| anyhow!("v2 provenance tool call is missing target_ref"))?;
    let target = EntityRef::parse(target_ref)
        .map_err(|error| anyhow!("invalid v2 provenance target_ref: {error}"))?;
    let target_project_id = match target {
        EntityRef::ProjectFile { project_id, .. } | EntityRef::ProjectFileV2 { project_id, .. } => {
            project_id
        }
        _ => bail!("v2 provenance target_ref must identify a project file"),
    };
    if target_project_id != project_id {
        bail!("v2 provenance target_ref belongs to a different central project");
    }
    Ok(())
}

fn plan_generation(
    project_id: &str,
    notes_ref: &str,
    documents: &[ProvenanceExportDocument],
) -> Result<String> {
    #[derive(Serialize)]
    struct GenerationInput<'a> {
        project_id: &'a str,
        notes_ref: &'a str,
        documents: Vec<(&'a str, u32, &'a str)>,
    }
    let input = GenerationInput {
        project_id,
        notes_ref,
        documents: documents
            .iter()
            .map(|document| {
                (
                    document.commit.as_str(),
                    document.part_index,
                    document.document_sha256.as_str(),
                )
            })
            .collect(),
    };
    Ok(sha256_hex(&serde_json::to_vec(&input)?))
}

fn note_part(
    logical: &GitProvenanceNote,
    document_id: &str,
    part_index: u32,
    part_count: u32,
    tool_calls: Vec<NoteToolCall>,
) -> GitProvenanceNote {
    GitProvenanceNote {
        schema_version: SCHEMA_VERSION_V2,
        commit: logical.commit.clone(),
        part: Some(GitProvenanceNotePart {
            document_id: document_id.to_string(),
            part_index,
            part_count,
        }),
        produced_by: logical.produced_by.clone(),
        tool_calls,
        knowledge_writes: logical.knowledge_writes.clone(),
    }
}

fn serialize_for_fragment(note: &GitProvenanceNote) -> std::result::Result<String, FragmentError> {
    serialize_note(note).map_err(|error| FragmentError::Serialization(error.to_string()))
}

fn canonical_project_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("canonicalizing provenance project root {}", root.display()))
        .and_then(|root| {
            if root.is_dir() {
                Ok(root)
            } else {
                bail!("provenance project root is not a directory")
            }
        })
}

fn lock_repository(root: &Path) -> Result<std::fs::File> {
    let common_dir = bbox_corpus_core::git::git_common_dir(root)
        .ok_or_else(|| anyhow!("provenance project has no Git common directory"))?;
    let lock_path = common_dir.join("blackbox-provenance.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening provenance repository lock {}", lock_path.display()))?;
    FileExt::lock_exclusive(&lock)
        .with_context(|| format!("locking provenance repository {}", common_dir.display()))?;
    Ok(lock)
}

fn commit_exists(root: &Path, commit: &str) -> Result<bool> {
    if !is_commit_id(commit) {
        return Ok(false);
    }
    let object = format!("{commit}^{{commit}}");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e", &object])
        .output()
        .with_context(|| format!("checking provenance commit in {}", root.display()))?;
    Ok(output.status.success())
}

fn git_check_ref_format(notes_ref: &str) -> bool {
    Command::new("git")
        .args(["check-ref-format", notes_ref])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn is_safe_notes_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace != "."
        && namespace != ".."
        && !namespace.starts_with('-')
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_commit_id(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

const fn v1_schema_version() -> u32 {
    SCHEMA_VERSION_V1
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf8 git output")
            .trim()
            .to_string()
    }

    fn init_repo(repo_id: &str) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical temp root");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "Blackbox Test"]);
        git(
            &root,
            &["config", "user.email", "blackbox-test@example.invalid"],
        );
        fs::create_dir_all(root.join(".bbox")).expect("create bbox");
        fs::write(
            root.join(".bbox/config.toml"),
            format!("[project]\nrepo_id = \"{repo_id}\"\n"),
        )
        .expect("write config");
        fs::write(root.join("tracked.txt"), "tracked\n").expect("write tracked");
        git(&root, &["add", ".bbox/config.toml", "tracked.txt"]);
        git(&root, &["commit", "-qm", "initial"]);
        let commit = git(&root, &["rev-parse", "HEAD"]);
        (dir, root, commit)
    }

    fn call(project_id: &str, payload_len: usize) -> NoteToolCall {
        NoteToolCall {
            tool: "Edit".into(),
            edge_kind: Some("EDITED_FILE".into()),
            source_ref: Some("transcript:test:session:10:0".into()),
            target_ref: Some(format!(
                "project_file:{project_id}:path:{}:0",
                "a".repeat(64)
            )),
            file: Some(format!("src/{}.rs", "x".repeat(payload_len))),
            byte_range: Some([10, 20]),
            turn: Some(10),
        }
    }

    fn page(root: &Path, repo_id: &str, project_id: &str, commit: &str) -> ProvenanceExportPage {
        let note = GitProvenanceNote::new_v2(
            commit,
            ProducedBy::default(),
            vec![call(project_id, 1)],
            Vec::new(),
        );
        let part = fragment_note(&note, MAX_NOTE_DOCUMENT_BYTES)
            .expect("fragment")
            .remove(0);
        let document = ProvenanceExportDocument::from_note(&part).expect("document");
        ProvenanceExportPage {
            scope: PublishedScope::try_new(
                repo_id,
                bbox_root_relpath(root, root).expect("root scope"),
            )
            .expect("valid scope"),
            project_id: project_id.into(),
            notes_ref: "refs/notes/bbox/provenance".into(),
            generation: "generation".into(),
            documents: vec![document],
            next_cursor: None,
        }
    }

    #[test]
    fn v1_document_without_version_or_target_remains_compatible() {
        let raw = r#"{
          "commit":"abc123",
          "produced_by":{},
          "tool_calls":[{"tool":"Edit","file":"src/main.rs"}],
          "knowledge_writes":[]
        }"#;
        let note = parse_note_document(raw).expect("parse v1");
        assert_eq!(note.schema_version, SCHEMA_VERSION_V1);
        assert_eq!(note.part, None);
        assert_eq!(note.tool_calls[0].target_ref, None);
    }

    #[test]
    fn v2_serialization_is_deterministic_and_contains_target() {
        let logical = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy::default(),
            vec![call("project1", 1)],
            Vec::new(),
        );
        let part = fragment_note(&logical, MAX_NOTE_DOCUMENT_BYTES)
            .expect("fragment")
            .remove(0);
        let first = serialize_note(&part).expect("serialize");
        let second = serialize_note(&part).expect("serialize");
        assert_eq!(first, second);
        assert!(first.contains("target_ref"));
        assert_eq!(document_sha256(&first), document_sha256(&second));
        assert_ne!(document_sha256(&first), document_sha256(&(first + "\n")));
    }

    #[test]
    fn fragmentation_is_deterministic_and_bounded_at_tool_calls() {
        let logical = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy::default(),
            vec![
                call("project1", 700),
                call("project1", 700),
                call("project1", 700),
            ],
            Vec::new(),
        );
        let first = fragment_note(&logical, 1_800).expect("fragment");
        let second = fragment_note(&logical, 1_800).expect("fragment");
        assert_eq!(first, second);
        assert!(first.len() > 1);
        assert_eq!(
            first
                .iter()
                .map(|part| part.tool_calls.len())
                .sum::<usize>(),
            logical.tool_calls.len()
        );
        assert!(
            first
                .iter()
                .all(|part| serialize_note(part).expect("serialize").len() <= 1_800)
        );
        let document_ids: BTreeSet<_> = first
            .iter()
            .map(|part| part.part.as_ref().expect("part").document_id.as_str())
            .collect();
        assert_eq!(document_ids.len(), 1);
    }

    #[test]
    fn oversized_single_tool_call_is_rejected() {
        let logical = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy::default(),
            vec![call("project1", 4_000)],
            Vec::new(),
        );
        assert!(matches!(
            fragment_note(&logical, 1_000),
            Err(FragmentError::ToolCallTooLarge { index: 0, .. })
        ));
    }

    #[test]
    fn export_plan_generation_is_order_independent() {
        let commit = "a".repeat(40);
        let logical = GitProvenanceNote::new_v2(
            &commit,
            ProducedBy::default(),
            vec![call("project1", 700), call("project1", 700)],
            Vec::new(),
        );
        let parts = fragment_note(&logical, 1_400).expect("fragment");
        let mut documents: Vec<_> = parts
            .iter()
            .map(|part| ProvenanceExportDocument::from_note(part).expect("document"))
            .collect();
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        let first = ProvenanceExportPlan::new(
            scope.clone(),
            "project1",
            "refs/notes/bbox/provenance",
            documents.clone(),
        )
        .expect("plan");
        documents.reverse();
        let second =
            ProvenanceExportPlan::new(scope, "project1", "refs/notes/bbox/provenance", documents)
                .expect("plan");
        assert_eq!(first.generation, second.generation);
        assert_eq!(first.documents, second.documents);
    }

    #[test]
    fn note_ref_validation_is_structurally_confined() {
        assert!(validate_notes_ref("refs/notes/bbox/provenance").is_ok());
        for invalid in [
            "refs/notes/../provenance",
            "refs/notes/-bbox/provenance",
            "refs/notes/bbox/other",
            "refs/heads/main",
            "refs/notes/b box/provenance",
            "refs/notes/bbox/../../heads/main",
        ] {
            assert!(validate_notes_ref(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn committed_scope_ignores_working_tree_identity_edit() {
        let (_dir, root, _commit) = init_repo("repo-a");
        fs::write(
            root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-b\"\n",
        )
        .expect("rewrite config");

        assert_eq!(
            resolve_committed_scope(&root).expect("scope"),
            PublishedScope::try_new("repo-a", ".").unwrap()
        );
    }

    #[test]
    fn page_application_is_idempotent() {
        let (_dir, root, commit) = init_repo("repo-a");
        let page = page(&root, "repo-a", "project1", &commit);

        let first = apply_export_page(&root, &page).expect("first apply");
        let second = apply_export_page(&root, &page).expect("second apply");

        assert_eq!(first.written, 1);
        assert_eq!(first.unchanged, 0);
        assert_eq!(second.written, 0);
        assert_eq!(second.unchanged, 1);
        let raw = bbox_corpus_core::git::show_note(&root, &page.notes_ref, &commit)
            .expect("show note")
            .expect("note exists");
        assert_eq!(split_note_documents(&raw).len(), 1);
    }

    #[test]
    fn migration_snapshot_is_no_create_and_reads_immutable_note_blobs() {
        use bbox_corpus_core::project_catalog_snapshot::{
            OwnerSnapshotLimitsV1, OwnerSnapshotRowValueV1, OwnerSnapshotStateV1,
        };

        let (_dir, root, commit) = init_repo("repo-a");
        let notes_ref = "refs/notes/bbox/provenance";
        let common_dir = bbox_corpus_core::git::git_common_dir(&root).unwrap();
        let repository_lock = common_dir.join("blackbox-provenance.lock");
        let missing = capture_project_catalog_owner_snapshot(
            &root,
            notes_ref,
            "project1",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();
        assert!(matches!(
            missing.state,
            OwnerSnapshotStateV1::Missing { .. }
        ));
        assert!(!repository_lock.exists());

        let page = page(&root, "repo-a", "project1", &commit);
        apply_export_page(&root, &page).unwrap();
        let snapshot = capture_project_catalog_owner_snapshot(
            &root,
            notes_ref,
            "project1",
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap();
        assert_eq!(snapshot.row_count, 1);
        assert!(matches!(
            &snapshot.rows[0].value,
            OwnerSnapshotRowValueV1::InventoryTarget {
                project_id,
                target_sha256,
            } if project_id == "project1" && target_sha256.len() == 64
        ));
    }

    #[test]
    fn concurrent_page_application_serializes_and_stays_idempotent() {
        let (_dir, root, commit) = init_repo("repo-a");
        let page = page(&root, "repo-a", "project1", &commit);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let page = page.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                apply_export_page(&root, &page).expect("concurrent apply")
            }));
        }
        let results = workers
            .into_iter()
            .map(|worker| worker.join().expect("join apply worker"))
            .collect::<Vec<_>>();
        assert_eq!(results.iter().map(|result| result.written).sum::<u64>(), 1);
        assert_eq!(
            results.iter().map(|result| result.unchanged).sum::<u64>(),
            1
        );
        let raw = bbox_corpus_core::git::show_note(&root, &page.notes_ref, &commit)
            .expect("show note")
            .expect("note exists");
        assert_eq!(split_note_documents(&raw).len(), 1);
    }

    #[test]
    fn invalid_page_is_rejected_before_any_note_write() {
        let (_dir, root, commit) = init_repo("repo-a");
        let mut page = page(&root, "repo-a", "project1", &commit);
        page.documents.push(page.documents[0].clone());
        page.documents[1].commit = "deadbeef".into();

        assert!(apply_export_page(&root, &page).is_err());
        assert!(
            bbox_corpus_core::git::show_note(&root, &page.notes_ref, &commit)
                .expect("show note")
                .is_none()
        );
    }

    #[test]
    fn page_validation_rejects_scope_ref_hash_and_missing_commit_before_write() {
        let (_dir, root, commit) = init_repo("repo-a");
        let valid = page(&root, "repo-a", "project1", &commit);
        let mut cases = Vec::new();

        let mut wrong_scope = valid.clone();
        wrong_scope.scope =
            PublishedScope::try_new("repo-b", wrong_scope.scope.bbox_root_relpath()).unwrap();
        cases.push(wrong_scope);

        let mut wrong_ref = valid.clone();
        wrong_ref.notes_ref = "refs/heads/main".into();
        cases.push(wrong_ref);

        let mut wrong_hash = valid.clone();
        wrong_hash.documents[0].document_sha256 = "0".repeat(64);
        cases.push(wrong_hash);

        let mut missing_commit = valid.clone();
        missing_commit.documents[0].commit = "b".repeat(40);
        cases.push(missing_commit);

        for invalid in cases {
            assert!(apply_export_page(&root, &invalid).is_err());
            assert!(
                bbox_corpus_core::git::show_note(&root, &valid.notes_ref, &commit)
                    .expect("show note")
                    .is_none()
            );
        }
    }

    #[test]
    fn cross_project_v2_target_is_rejected() {
        let (_dir, root, commit) = init_repo("repo-a");
        let mut page = page(&root, "repo-a", "project1", &commit);
        let mut note = parse_note_document(&page.documents[0].document).expect("parse note");
        note.tool_calls[0].target_ref =
            Some(format!("project_file:project2:path:{}:0", "a".repeat(64)));
        page.documents[0] = ProvenanceExportDocument::from_note(&note).expect("document");

        assert!(apply_export_page(&root, &page).is_err());
    }
}
