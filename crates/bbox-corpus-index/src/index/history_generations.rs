//! Immutable repo-history generation store (Phase 3 milestone P3-D).
//!
//! Governing design `durable-project-catalog-impl.md` section 11 and D-027:
//! repo-level commit documents and their vector inputs get their own
//! immutable, self-contained generations so a destructive index replacement
//! can rematerialize history without ever reopening a Git checkout. This
//! module owns the on-disk format, the content-addressed identity, and the
//! self-verification; the orchestration that decides WHICH generations to
//! build lives in `bbox-indexing`'s `index::history_materializer`.
//!
//! # Placement invariant
//!
//! Generations live at `<family_root>/history-generations/<generation_id>/`
//! where `family_root` is the PARENT of `index_path`, so the generations
//! root is a SIBLING of the tantivy index and never a descendant of it.
//! This is load-bearing rather than cosmetic: the schema-mismatch reset in
//! `index::reset_index_on_schema_mismatch` does `remove_dir_all(index_path)`,
//! so anything under `index_path` is destroyed by the very replacement these
//! generations exist to survive. `generations_root_for_index` enforces the
//! siblinghood and refuses any derivation that would nest the two.
//!
//! # Identity
//!
//! Generation ids are content-addressed SHA-256 over a versioned domain
//! separator, the namespace, the typed owner/disposition, and the canonical
//! generation bytes, using the length-prefixed `put_field` convention from
//! `bbox_code_source::generation_id`. Re-materializing the same namespace
//! from the same source therefore lands on the same id and the same bytes:
//! creation is idempotent and cannot remint identity.
//!
//! # Commitment scope
//!
//! The document-set commitment is `migration_inventory::hash_commit_rows`,
//! the Phase 1 capture's own function, folding ONLY namespace, entity_ref,
//! commit_sha, and content_hash. It is deliberately path-free so a
//! generation can re-emit its documents with new `project` / `file_path`
//! values (the P3-E schema cut does exactly that) and still prove set
//! equality against the Phase 1 evidence. Do not widen it.
//!
//! # Vector inputs are index-sourced, and that is visible in the format
//!
//! Commit vectors are enqueued with the hash of the RAW commit message while
//! commit documents store the hash of the message TRUNCATED at
//! `MAX_COMMIT_MESSAGE_BYTES`. A generation is built from the index, so the
//! only message text it can carry is the truncated one. For a namespace with
//! no truncated message the two hashes coincide; above the cap they do not,
//! and re-emission produces a vector key whose content hash differs from the
//! legacy key. The manifest records `truncated_message_count` so that
//! divergence is observable, and no code in this module or its callers may
//! assert equality between a vector-input content hash and a vector-store
//! key hash.
//!
//! # Blocking I/O posture
//!
//! Every filesystem call here is synchronous, matching the sibling
//! `migration_inventory` capture module. That is deliberate: this module's
//! callers are the boot-time recovery path and the writer pass that owns a
//! rebuild, never a tokio worker. `clippy.toml`'s disallowed-methods gate
//! denies only in `src/tools/` and the harness crates, so these calls warn
//! here exactly as the rest of this crate's store I/O does; a blanket
//! `#[allow]` would claim an actor sanction this module does not have and
//! does not need.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::collector::DocSetCollector;
use tantivy::query::AllQuery;
use tantivy::{Index, TantivyDocument};

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::project_catalog::{
    CommitNamespace, RepoHistoryGenerationId, RepoHistoryId, RepoHistoryQuarantineGenerationId,
};

use super::git_history::TRUNCATED_COMMIT_MESSAGE_SUFFIX;
use super::migration_inventory::{CommitRowV1, hash_commit_rows};
use super::{SCHEMA_VERSION_FILE, optional_text, optional_u64};

/// Directory name of the generations root under the index family root.
pub const HISTORY_GENERATIONS_DIRNAME: &str = "history-generations";

const GENERATION_MANIFEST_FILE: &str = "manifest.json";
const COMMIT_DOCUMENTS_FILE: &str = "commit-documents.jsonl";
const VECTOR_INPUTS_FILE: &str = "vector-inputs.jsonl";
/// The rebuild manifest is a FILE directly under the generations root, so
/// enumerating generations (which are directories) never confuses the two.
const REBUILD_MANIFEST_FILE: &str = "rebuild-manifest.json";

const GENERATION_VERSION_V1: u32 = 1;
const REBUILD_MANIFEST_VERSION_V1: u32 = 1;

const GENERATION_ID_DOMAIN: &[u8] = b"blackbox.repo-history-generation.v1";
const VECTOR_INPUT_COMMITMENT_DOMAIN: &[u8] = b"blackbox.repo-history-generation.vector-inputs.v1";
const SOURCE_FINGERPRINT_DOMAIN: &[u8] = b"blackbox.repo-history-generation.source.v1";
const REBUILD_MANIFEST_ID_DOMAIN: &[u8] = b"blackbox.repo-history-rebuild-manifest.v1";

/// Tantivy's own index metadata file. Its presence is what distinguishes a
/// directory that HOLDS an index from a directory that merely sits where one
/// would go.
const TANTIVY_META_FILE: &str = "meta.json";

/// `source_schema_version` recorded when the scanned index carried no
/// `schema_version.txt` marker at all.
///
/// Shaped like `LIVE_REFRESH_SOURCE_MARKER`: a reserved, documented sentinel
/// that is non-hex and can never collide with a real `INDEX_SCHEMA_VERSION`
/// value, so no reader can mistake it for an observed schema. It is a
/// CONSTANT rather than an absent field so `source_schema_version` stays a
/// required non-empty `String`: the field is in the generation-id preimage,
/// and a deterministic sentinel keeps pre-marker generations content-addressed
/// and idempotent across repeated scans exactly like every other generation.
///
/// This changes no existing generation id. Every generation written before
/// this constant existed recorded a real marker, because a marker-less index
/// previously ended the scan before any generation could be built.
pub const PRE_MARKER_SOURCE_SCHEMA: &str = "blackbox.repo-history-generation.pre-marker-source.v1";

/// Upper bounds on one scan. Mirrors the Phase 1 capture's posture: a
/// hostile or corrupt index must fail closed rather than exhaust memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryScanLimitsV1 {
    pub max_documents: u64,
    pub max_commit_namespaces: usize,
    pub max_total_string_bytes: usize,
}

impl Default for HistoryScanLimitsV1 {
    fn default() -> Self {
        Self {
            max_documents: 10_000_000,
            max_commit_namespaces: 1_000_000,
            max_total_string_bytes: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGenerationError {
    code: String,
    message: String,
}

impl HistoryGenerationError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for HistoryGenerationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HistoryGenerationError {}

pub type HistoryGenerationResult<T> = Result<T, HistoryGenerationError>;

fn corrupt(message: impl Into<String>) -> HistoryGenerationError {
    HistoryGenerationError::new("error.history_generation_corrupt", message)
}

fn identity_error(message: impl Into<String>) -> HistoryGenerationError {
    HistoryGenerationError::new("error.history_generation_identity", message)
}

fn io_error(message: impl Into<String>) -> HistoryGenerationError {
    HistoryGenerationError::new("error.history_generation_io", message)
}

fn unsafe_path(message: impl Into<String>) -> HistoryGenerationError {
    HistoryGenerationError::new("error.history_generation_unsafe_path", message)
}

// ---------------------------------------------------------------------------
// Crash-fault seam
// ---------------------------------------------------------------------------

/// Fault points a test IO can fail at, mirroring the Phase 1
/// `project_catalog_store` seam: the real IO's `checkpoint` is a no-op, so
/// production paths pay nothing and crash-recovery tests get exact
/// positioning instead of process kills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFaultPoint {
    GenerationBodyWrite,
    GenerationManifestWrite,
    GenerationVerify,
    RebuildManifestPreparedWrite,
    RebuildManifestCommittedWrite,
    RebuildManifestDelete,
}

pub trait HistoryGenerationIo: Send + Sync + std::fmt::Debug {
    fn checkpoint(&self, _point: HistoryFaultPoint) -> HistoryGenerationResult<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct RealHistoryGenerationIo;

impl HistoryGenerationIo for RealHistoryGenerationIo {}

// ---------------------------------------------------------------------------
// Row and document shapes
// ---------------------------------------------------------------------------

/// One commit document's full stored-field set MINUS the two path-bearing
/// fields (`project`, which carried a raw host canonical path, and
/// `file_path`, which carried the per-project `git:<project_id>` source
/// key). Both are re-derived at re-emission from the then-current catalog
/// and schema; storing them would freeze host paths into an immutable
/// artifact and defeat the path-free cut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCommitDocumentV1 {
    pub entity_id: String,
    pub doc_type: String,
    pub chunk_kind: String,
    pub repo_id: String,
    pub commit_sha: String,
    pub content: String,
    pub content_hash: String,
    pub path_tokens: String,
    pub parser_version: String,
    pub commit_author_name: String,
    pub commit_author_email: String,
    pub session_id: String,
    pub account: String,
    pub role: String,
    pub byte_offset: u64,
    pub is_subagent: u64,
}

/// One vector input row: exactly what `embed_queue::enqueue_git_message`
/// needs to re-enqueue the commit's embedding after a replacement. The
/// `content_hash` here is over `message` as this generation carries it (the
/// indexed, possibly truncated, text) and is NOT claimed equal to the
/// legacy vector-store key hash; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryVectorInputV1 {
    pub entity_id: String,
    pub content_hash: String,
    pub message: String,
}

/// Typed owner/disposition. `Owned` yields an `rhg_` id and is the only
/// disposition a `RepoHistoryRecord` may name; `Ambiguous` and `Unclaimed`
/// yield `rhq_` quarantine ids. An `Unclaimed` generation has no catalog
/// home at all: `validate_catalog`'s ">= 2 candidates and every candidate
/// exists" rule makes an unclaimed namespace unrepresentable as an
/// `AmbiguousNamespaceRecord`, so the rebuild manifest is its sole durable
/// owner (Phase 3 plan sections 4.4 and 8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryGenerationOwnerV1 {
    Owned {
        repo_history_id: RepoHistoryId,
    },
    Ambiguous {
        candidate_repo_history_ids: BTreeSet<RepoHistoryId>,
    },
    Unclaimed {
        inventory_diagnostic: String,
    },
}

impl HistoryGenerationOwnerV1 {
    fn discriminant(&self) -> &'static str {
        match self {
            Self::Owned { .. } => "owned",
            Self::Ambiguous { .. } => "ambiguous",
            Self::Unclaimed { .. } => "unclaimed",
        }
    }

    fn is_owned(&self) -> bool {
        matches!(self, Self::Owned { .. })
    }
}

/// A generation id in either of its two shapes. Parsing is delegated to the
/// P3-A catalog types so a generation id and a catalog `Ready` state can
/// never disagree about what a valid id looks like.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum HistoryGenerationIdV1 {
    Owned(RepoHistoryGenerationId),
    Quarantine(RepoHistoryQuarantineGenerationId),
}

impl HistoryGenerationIdV1 {
    pub fn parse(value: &str) -> HistoryGenerationResult<Self> {
        if let Ok(id) = RepoHistoryGenerationId::parse(value) {
            return Ok(Self::Owned(id));
        }
        RepoHistoryQuarantineGenerationId::parse(value)
            .map(Self::Quarantine)
            .map_err(|_| identity_error(format!("generation id {value} has an unknown shape")))
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Owned(id) => id.as_str(),
            Self::Quarantine(id) => id.as_str(),
        }
    }

    pub fn owned(&self) -> Option<&RepoHistoryGenerationId> {
        match self {
            Self::Owned(id) => Some(id),
            Self::Quarantine(_) => None,
        }
    }

    pub fn quarantine(&self) -> Option<&RepoHistoryQuarantineGenerationId> {
        match self {
            Self::Quarantine(id) => Some(id),
            Self::Owned(_) => None,
        }
    }
}

impl std::fmt::Display for HistoryGenerationIdV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The canonical, id-free body of a generation. The generation id is a hash
/// over the body's CONTENT-BEARING subset (`HistoryGenerationIdPreimageV1`)
/// plus namespace and owner, so the body is serialized WITHOUT the id: an id
/// can never be part of its own preimage. The three `source_*` evidence
/// fields are provenance, deliberately OUTSIDE the preimage (D-039): identity
/// is a pure content address, so re-emitting byte-identical history under a
/// different source (the next schema bump's scan, a live refresh) re-derives
/// the SAME id instead of tripping the strict no-remint advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryGenerationBodyV1 {
    pub version: u32,
    pub namespace: CommitNamespace,
    pub owner: HistoryGenerationOwnerV1,
    pub commit_document_count: u64,
    /// `hash_commit_rows` over the ordered rows: namespace, entity_ref,
    /// commit_sha, content_hash. Path-free by construction.
    pub commit_document_commitment_sha256: String,
    /// SHA-256 of the exact `commit-documents.jsonl` bytes.
    pub commit_documents_sha256: String,
    pub vector_input_count: u64,
    pub vector_input_commitment_sha256: String,
    /// SHA-256 of the exact `vector-inputs.jsonl` bytes.
    pub vector_inputs_sha256: String,
    /// How many carried messages were truncated at ingest. Non-zero means
    /// re-emitted vector keys will not reproduce the legacy key hashes.
    pub truncated_message_count: u64,
    /// Source evidence: which schema marker, schema fingerprint, and index
    /// population (or live-refresh marker) this generation's rows were
    /// observed under. PROVENANCE ONLY, excluded from the id preimage
    /// (D-039): the evidence is volatile across schema bumps and across the
    /// scan/live-refresh construction sites, while the id must stay a stable
    /// content address for the same carried history. When identical content
    /// is re-created under different evidence, the first writer's evidence
    /// is retained on disk (see `create_or_open`).
    pub source_schema_version: String,
    pub source_schema_fingerprint_sha256: String,
    pub source_index_fingerprint_sha256: String,
}

/// The exact fields the generation id commits to: everything in the body
/// EXCEPT the three `source_*` evidence fields. A borrowed view so hashing
/// and persistence can never drift apart field-by-field without this struct
/// changing shape. A future body field is OUT of the preimage unless it is
/// consciously added here; only content-bearing fields belong. (No domain
/// bump is needed relative to the pre-D-039 whole-body preimage: this JSON
/// always lacks the `source_*` keys while a whole-body encoding always
/// carries them, so the two encodings can never collide byte-for-byte.)
#[derive(Serialize)]
struct HistoryGenerationIdPreimageV1<'a> {
    version: u32,
    namespace: &'a CommitNamespace,
    owner: &'a HistoryGenerationOwnerV1,
    commit_document_count: u64,
    commit_document_commitment_sha256: &'a str,
    commit_documents_sha256: &'a str,
    vector_input_count: u64,
    vector_input_commitment_sha256: &'a str,
    vector_inputs_sha256: &'a str,
    truncated_message_count: u64,
}

impl<'a> HistoryGenerationIdPreimageV1<'a> {
    fn of(body: &'a HistoryGenerationBodyV1) -> Self {
        Self {
            version: body.version,
            namespace: &body.namespace,
            owner: &body.owner,
            commit_document_count: body.commit_document_count,
            commit_document_commitment_sha256: &body.commit_document_commitment_sha256,
            commit_documents_sha256: &body.commit_documents_sha256,
            vector_input_count: body.vector_input_count,
            vector_input_commitment_sha256: &body.vector_input_commitment_sha256,
            vector_inputs_sha256: &body.vector_inputs_sha256,
            truncated_message_count: body.truncated_message_count,
        }
    }

    fn canonical_bytes(&self) -> HistoryGenerationResult<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|error| corrupt(format!("generation id preimage cannot be encoded: {error}")))
    }
}

/// The generation manifest as persisted: the body plus its derived id.
///
/// The body is a nested object rather than a flattened one on purpose: the
/// id's preimage is a canonical serialization of the body's content-bearing
/// fields, so the body must keep one unambiguous field layout whether it is
/// being projected for hashing or persisted, and `serde(flatten)` is
/// incompatible with the `deny_unknown_fields` posture the rest of this
/// format uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryGenerationManifestV1 {
    pub generation_id: String,
    pub body: HistoryGenerationBodyV1,
}

/// A generation loaded off disk together with its documents and inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGenerationRecordV1 {
    pub id: HistoryGenerationIdV1,
    pub manifest: HistoryGenerationManifestV1,
    pub commit_documents: Vec<HistoryCommitDocumentV1>,
    pub vector_inputs: Vec<HistoryVectorInputV1>,
}

impl HistoryGenerationRecordV1 {
    /// Self-verification following the `StoredGenerationV2::validate`
    /// template: re-derive the id and refuse a mismatch, re-derive every
    /// commitment from the carried rows, and require complete-or-absent
    /// evidence pairs (a zero count must carry the empty-set commitment and
    /// a non-zero count must not).
    pub fn validate(&self) -> HistoryGenerationResult<()> {
        let body = &self.manifest.body;
        if body.version != GENERATION_VERSION_V1 {
            return Err(corrupt(format!(
                "generation version {} is not supported",
                body.version
            )));
        }
        if self.manifest.generation_id != self.id.as_str() {
            return Err(identity_error(
                "generation manifest id disagrees with its own parsed id",
            ));
        }
        let expected = derive_generation_id(body)?;
        if expected.as_str() != self.manifest.generation_id {
            return Err(identity_error(format!(
                "generation {} does not re-derive its own id",
                self.manifest.generation_id
            )));
        }
        if expected.owned().is_some() != body.owner.is_owned() {
            return Err(identity_error(
                "generation id shape disagrees with its owner disposition",
            ));
        }

        if self.commit_documents.len() as u64 != body.commit_document_count {
            return Err(corrupt(
                "generation commit document count disagrees with its rows",
            ));
        }
        if self.vector_inputs.len() as u64 != body.vector_input_count {
            return Err(corrupt(
                "generation vector input count disagrees with its rows",
            ));
        }
        let rows = commit_rows_for(&self.commit_documents);
        if hash_commit_rows(&rows) != body.commit_document_commitment_sha256 {
            return Err(corrupt("generation document commitment does not re-derive"));
        }
        if hash_vector_inputs(&self.vector_inputs) != body.vector_input_commitment_sha256 {
            return Err(corrupt(
                "generation vector-input commitment does not re-derive",
            ));
        }
        if encode_commit_documents(&self.commit_documents)?.1 != body.commit_documents_sha256 {
            return Err(corrupt("generation document bytes do not re-derive"));
        }
        if encode_vector_inputs(&self.vector_inputs)?.1 != body.vector_inputs_sha256 {
            return Err(corrupt("generation vector-input bytes do not re-derive"));
        }

        // Complete-or-absent evidence pairs, following the
        // `StoredGenerationV2::validate` template: an empty set is only legal
        // beside the empty-set commitment, and a non-empty set only beside a
        // different one. Defense in depth behind the re-derive clauses above
        // (which already refuse any commitment that does not recompute from
        // the carried rows), kept because it states the invariant a future
        // format change must preserve rather than leaving it implicit.
        let empty_documents = hash_commit_rows(&[]);
        if (body.commit_document_count == 0)
            != (body.commit_document_commitment_sha256 == empty_documents)
        {
            return Err(corrupt("generation document count and commitment disagree"));
        }
        let empty_inputs = hash_vector_inputs(&[]);
        if (body.vector_input_count == 0) != (body.vector_input_commitment_sha256 == empty_inputs) {
            return Err(corrupt(
                "generation vector-input count and commitment disagree",
            ));
        }
        if body.truncated_message_count > body.vector_input_count {
            return Err(corrupt(
                "generation truncated count exceeds its vector inputs",
            ));
        }

        for document in &self.commit_documents {
            if document.repo_id != body.namespace.as_str() {
                return Err(corrupt("generation carries a foreign-namespace document"));
            }
            if !matches!(
                EntityRef::parse(&document.entity_id),
                Ok(EntityRef::Commit { repo_id, sha })
                    if repo_id == document.repo_id && sha == document.commit_sha
            ) {
                return Err(corrupt("generation carries an invalid commit entity ref"));
            }
        }
        let document_entities = self
            .commit_documents
            .iter()
            .map(|document| document.entity_id.as_str())
            .collect::<BTreeSet<_>>();
        for input in &self.vector_inputs {
            if !document_entities.contains(input.entity_id.as_str()) {
                return Err(corrupt("generation vector input has no matching document"));
            }
        }
        Ok(())
    }
}

/// The generation rows one freshly walked commit contributes.
///
/// EQUIVALENCE CONTRACT: these rows must be byte-identical to what
/// [`scan_commit_documents`] would read back after `build_commit_doc` wrote
/// the same commit into tantivy. That equivalence is what lets a live refresh
/// and a pre-replacement materialization of the same namespace converge on
/// the same content-addressed generation id instead of forking identity. Any
/// change to `build_commit_doc`'s stored fields must land here in the same
/// commit; the two path-bearing fields (`project`, `file_path`) are excluded
/// from the generation by design and so have no counterpart here.
///
/// The vector input's `content_hash` is over the INDEXED (possibly truncated)
/// message, matching the scan; it is deliberately not the raw-message hash
/// the legacy vector key used, and the two are never compared.
pub fn generation_rows_for_commit(
    commit: &bbox_corpus_core::git::GitCommit,
    namespace: &str,
) -> (HistoryCommitDocumentV1, HistoryVectorInputV1) {
    use super::git_history::{
        commit_entity_id, commit_message_hash, commit_subject_tokens, indexable_commit_message,
    };

    let entity_id = commit_entity_id(namespace, &commit.sha);
    let content = indexable_commit_message(&commit.message);
    let content_hash = commit_message_hash(&content);
    let document = HistoryCommitDocumentV1 {
        entity_id: entity_id.clone(),
        doc_type: "commit".to_string(),
        chunk_kind: "git_message".to_string(),
        repo_id: namespace.to_string(),
        commit_sha: commit.sha.clone(),
        path_tokens: commit_subject_tokens(&content).to_string(),
        parser_version: bbox_corpus_core::entity_ref::PARSER_VERSION.to_string(),
        commit_author_name: commit.author_name.clone(),
        commit_author_email: commit.author_email.clone(),
        session_id: String::new(),
        account: "git".to_string(),
        role: "commit".to_string(),
        byte_offset: 0,
        is_subagent: 0,
        content_hash: content_hash.clone(),
        content,
    };
    let vector = HistoryVectorInputV1 {
        entity_id,
        content_hash,
        message: document.content.clone(),
    };
    (document, vector)
}

/// Everything the caller must supply to create a generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryGenerationInputV1 {
    pub namespace: CommitNamespace,
    pub owner: HistoryGenerationOwnerV1,
    pub commit_documents: Vec<HistoryCommitDocumentV1>,
    pub vector_inputs: Vec<HistoryVectorInputV1>,
    pub truncated_message_count: u64,
    pub source_schema_version: String,
    pub source_schema_fingerprint_sha256: String,
    pub source_index_fingerprint_sha256: String,
}

// ---------------------------------------------------------------------------
// Identity derivation
// ---------------------------------------------------------------------------

/// Length-prefixed field folding, the `bbox_code_source::generation_id`
/// convention: an eight-byte big-endian length then the bytes, so no field
/// boundary can be forged by concatenation.
fn put_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn put_owner(hasher: &mut Sha256, owner: &HistoryGenerationOwnerV1) {
    put_field(hasher, owner.discriminant().as_bytes());
    match owner {
        HistoryGenerationOwnerV1::Owned { repo_history_id } => {
            put_field(hasher, repo_history_id.as_str().as_bytes());
        }
        HistoryGenerationOwnerV1::Ambiguous {
            candidate_repo_history_ids,
        } => {
            put_field(
                hasher,
                &(candidate_repo_history_ids.len() as u64).to_be_bytes(),
            );
            for candidate in candidate_repo_history_ids {
                put_field(hasher, candidate.as_str().as_bytes());
            }
        }
        HistoryGenerationOwnerV1::Unclaimed {
            inventory_diagnostic,
        } => {
            put_field(hasher, inventory_diagnostic.as_bytes());
        }
    }
}

fn derive_generation_id(
    body: &HistoryGenerationBodyV1,
) -> HistoryGenerationResult<HistoryGenerationIdV1> {
    // The preimage view, NOT the whole body: the `source_*` evidence fields
    // are provenance and must not shift identity (D-039). Two construction
    // sites (pre-replacement scan, live refresh) and every future schema
    // bump re-derive the same id for the same carried content.
    let canonical = HistoryGenerationIdPreimageV1::of(body).canonical_bytes()?;
    let mut hasher = Sha256::new();
    put_field(&mut hasher, GENERATION_ID_DOMAIN);
    put_field(&mut hasher, body.namespace.as_str().as_bytes());
    put_owner(&mut hasher, &body.owner);
    put_field(&mut hasher, &canonical);
    let digest = hex::encode(hasher.finalize());
    if body.owner.is_owned() {
        RepoHistoryGenerationId::parse(format!("rhg_{digest}"))
            .map(HistoryGenerationIdV1::Owned)
            .map_err(|error| identity_error(error.to_string()))
    } else {
        RepoHistoryQuarantineGenerationId::parse(format!("rhq_{digest}"))
            .map(HistoryGenerationIdV1::Quarantine)
            .map_err(|error| identity_error(error.to_string()))
    }
}

fn commit_rows_for(documents: &[HistoryCommitDocumentV1]) -> Vec<CommitRowV1> {
    documents
        .iter()
        .map(|document| CommitRowV1 {
            namespace: document.repo_id.clone(),
            entity_ref: document.entity_id.clone(),
            commit_sha: document.commit_sha.clone(),
            content_hash: document.content_hash.clone(),
        })
        .collect()
}

/// Commitment over the generation's OWN vector inputs. Deliberately a
/// different domain and shape from `bbox_vectors`'s active-key commitment:
/// that one folds a route and a raw-message content hash this module cannot
/// observe from the index, so the two are never compared.
fn hash_vector_inputs(inputs: &[HistoryVectorInputV1]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(VECTOR_INPUT_COMMITMENT_DOMAIN);
    for input in inputs {
        put_field(&mut hasher, input.entity_id.as_bytes());
        put_field(&mut hasher, input.content_hash.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn encode_commit_documents(
    documents: &[HistoryCommitDocumentV1],
) -> HistoryGenerationResult<(Vec<u8>, String)> {
    encode_jsonl(documents)
}

fn encode_vector_inputs(
    inputs: &[HistoryVectorInputV1],
) -> HistoryGenerationResult<(Vec<u8>, String)> {
    encode_jsonl(inputs)
}

fn encode_jsonl<T: Serialize>(rows: &[T]) -> HistoryGenerationResult<(Vec<u8>, String)> {
    let mut bytes = Vec::new();
    for row in rows {
        let line = serde_json::to_vec(row)
            .map_err(|error| corrupt(format!("generation row cannot be encoded: {error}")))?;
        bytes.extend_from_slice(&line);
        bytes.push(b'\n');
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    Ok((bytes, hash))
}

fn decode_jsonl<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> HistoryGenerationResult<Vec<T>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| corrupt("generation row file is not valid utf-8"))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| corrupt(format!("generation row cannot be decoded: {error}")))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// Derive the generations root for an index path, enforcing siblinghood.
///
/// Refuses a relative index path, an index path with no parent, and (belt
/// and braces against a future caller passing a family root by mistake) any
/// derivation whose result would sit inside `index_path`.
pub fn generations_root_for_index(index_path: &Path) -> HistoryGenerationResult<PathBuf> {
    if !strict_absolute_path(index_path) {
        return Err(unsafe_path(
            "index path must be absolute and free of traversal",
        ));
    }
    let family_root = index_path
        .parent()
        .ok_or_else(|| unsafe_path("index path has no family root"))?;
    let root = family_root.join(HISTORY_GENERATIONS_DIRNAME);
    if root.starts_with(index_path) {
        return Err(unsafe_path(
            "history generations root must be a sibling of the index, never inside it",
        ));
    }
    Ok(root)
}

fn strict_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

#[derive(Debug, Clone)]
pub struct HistoryGenerationStore {
    root: PathBuf,
    io: Arc<dyn HistoryGenerationIo>,
}

/// Deterministic, fully validated generation body before its durable commit
/// point is published.
///
/// The id is already final here. Transaction orchestrators may therefore bind
/// it into a `Prepared` journal before calling [`HistoryGenerationStore::publish`].
/// Construction remains singular: [`HistoryGenerationStore::prepare`] owns
/// sorting, canonical encoding, commitments, and id derivation, while
/// `publish` merely installs and verifies those exact prepared bytes.
#[derive(Debug)]
pub struct PreparedHistoryGenerationV1 {
    record: HistoryGenerationRecordV1,
    document_bytes: Vec<u8>,
    input_bytes: Vec<u8>,
}

impl PreparedHistoryGenerationV1 {
    pub fn record(&self) -> &HistoryGenerationRecordV1 {
        &self.record
    }

    pub fn generation_id(&self) -> &HistoryGenerationIdV1 {
        &self.record.id
    }
}

impl HistoryGenerationStore {
    /// Open (creating on demand) the generations root sibling to `index_path`.
    pub fn open_for_index(index_path: &Path) -> HistoryGenerationResult<Self> {
        Self::open_for_index_with_io(index_path, Arc::new(RealHistoryGenerationIo))
    }

    /// Open an existing generations root without creating it. Offline
    /// cutover preflight uses this read-only entry point so inspection cannot
    /// mutate an uninitialized target.
    pub fn open_existing_for_index(index_path: &Path) -> HistoryGenerationResult<Self> {
        let root = generations_root_for_index(index_path)?;
        refuse_symlinked_directory(&root, "history generations root")?;
        let metadata = fs::symlink_metadata(&root)
            .map_err(|error| io_error(format!("cannot inspect generations root: {error}")))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io_error("history generations root is not a real directory"));
        }
        Ok(Self {
            root,
            io: Arc::new(RealHistoryGenerationIo),
        })
    }

    pub fn open_for_index_with_io(
        index_path: &Path,
        io: Arc<dyn HistoryGenerationIo>,
    ) -> HistoryGenerationResult<Self> {
        let root = generations_root_for_index(index_path)?;
        // Refuse before AND after creating: `create_dir_all` happily follows
        // an existing symlink, so a pre-existing link would otherwise plant
        // the whole generations tree wherever it points.
        refuse_symlinked_directory(&root, "history generations root")?;
        fs::create_dir_all(&root)
            .map_err(|error| io_error(format!("cannot create generations root: {error}")))?;
        refuse_symlinked_directory(&root, "history generations root")?;
        Ok(Self { root, io })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn generation_dir(&self, id: &HistoryGenerationIdV1) -> PathBuf {
        // Generation ids are `rhg_`/`rhq_` plus 64 lowercase hex, validated
        // by the catalog parser, so they are safe basenames by construction.
        self.root.join(id.as_str())
    }

    /// Create the generation, or return the identical one already on disk.
    ///
    /// Identity is content-addressed, so an existing directory under the
    /// derived id can only hold the same CONTENT; it is loaded and validated
    /// rather than rewritten. That is what makes re-materialization
    /// idempotent and byte-identical instead of a remint. The existing
    /// manifest's `source_*` evidence may legitimately differ from this
    /// input's (the same history observed from another source: an earlier
    /// schema's scan, a live refresh); evidence is outside the id preimage
    /// (D-039), the first writer's evidence is retained, and the caller gets
    /// the on-disk record.
    pub fn create_or_open(
        &self,
        input: HistoryGenerationInputV1,
    ) -> HistoryGenerationResult<HistoryGenerationRecordV1> {
        self.publish(Self::prepare(input)?)
    }

    /// Derive and validate the exact future generation without writing it.
    pub fn prepare(
        input: HistoryGenerationInputV1,
    ) -> HistoryGenerationResult<PreparedHistoryGenerationV1> {
        let mut commit_documents = input.commit_documents;
        commit_documents.sort_by(|left, right| {
            (
                &left.repo_id,
                &left.entity_id,
                &left.commit_sha,
                &left.content_hash,
            )
                .cmp(&(
                    &right.repo_id,
                    &right.entity_id,
                    &right.commit_sha,
                    &right.content_hash,
                ))
        });
        let mut vector_inputs = input.vector_inputs;
        vector_inputs.sort_by(|left, right| {
            (&left.entity_id, &left.content_hash).cmp(&(&right.entity_id, &right.content_hash))
        });

        let (document_bytes, documents_sha256) = encode_commit_documents(&commit_documents)?;
        let (input_bytes, inputs_sha256) = encode_vector_inputs(&vector_inputs)?;
        let body = HistoryGenerationBodyV1 {
            version: GENERATION_VERSION_V1,
            namespace: input.namespace,
            owner: input.owner,
            commit_document_count: commit_documents.len() as u64,
            commit_document_commitment_sha256: hash_commit_rows(&commit_rows_for(
                &commit_documents,
            )),
            commit_documents_sha256: documents_sha256,
            vector_input_count: vector_inputs.len() as u64,
            vector_input_commitment_sha256: hash_vector_inputs(&vector_inputs),
            vector_inputs_sha256: inputs_sha256,
            truncated_message_count: input.truncated_message_count,
            source_schema_version: input.source_schema_version,
            source_schema_fingerprint_sha256: input.source_schema_fingerprint_sha256,
            source_index_fingerprint_sha256: input.source_index_fingerprint_sha256,
        };
        let id = derive_generation_id(&body)?;
        let manifest = HistoryGenerationManifestV1 {
            generation_id: id.as_str().to_string(),
            body,
        };
        let record = HistoryGenerationRecordV1 {
            id: id.clone(),
            manifest,
            commit_documents,
            vector_inputs,
        };
        record.validate()?;

        Ok(PreparedHistoryGenerationV1 {
            record,
            document_bytes,
            input_bytes,
        })
    }

    /// Publish one value produced by [`Self::prepare`], then verify it from
    /// disk. No caller can inject separately encoded rows through this seam.
    pub fn publish(
        &self,
        prepared: PreparedHistoryGenerationV1,
    ) -> HistoryGenerationResult<HistoryGenerationRecordV1> {
        let PreparedHistoryGenerationV1 {
            record,
            document_bytes,
            input_bytes,
        } = prepared;
        let id = record.id.clone();

        // The manifest file is the commit point. A crash after the two row
        // files but before the manifest leaves a directory that `load`
        // refuses and that this branch therefore rewrites from scratch on
        // the next pass. Such a directory can never be referenced by anyone:
        // an id only escapes this function after a successful verify, so no
        // catalog record or rebuild manifest can name it.
        let directory = self.generation_dir(&id);
        if directory.join(GENERATION_MANIFEST_FILE).exists() {
            let existing = self.load(&id)?;
            // Compare identity content, NOT whole records: the manifest's
            // `source_*` evidence is allowed to differ (same history observed
            // from another source), while the rows and every preimage field
            // must agree. Unreachable while ids stay content-addressed; if it
            // ever fires the store is corrupt, not merely stale.
            let existing_preimage =
                HistoryGenerationIdPreimageV1::of(&existing.manifest.body).canonical_bytes()?;
            let record_preimage =
                HistoryGenerationIdPreimageV1::of(&record.manifest.body).canonical_bytes()?;
            if existing_preimage != record_preimage
                || existing.commit_documents != record.commit_documents
                || existing.vector_inputs != record.vector_inputs
            {
                return Err(identity_error(
                    "existing generation disagrees with its content-addressed identity",
                ));
            }
            return Ok(existing);
        }

        fs::create_dir_all(&directory)
            .map_err(|error| io_error(format!("cannot create generation directory: {error}")))?;
        write_file(&directory.join(COMMIT_DOCUMENTS_FILE), &document_bytes)?;
        write_file(&directory.join(VECTOR_INPUTS_FILE), &input_bytes)?;
        self.io.checkpoint(HistoryFaultPoint::GenerationBodyWrite)?;
        let manifest_bytes = serde_json::to_vec(&record.manifest)
            .map_err(|error| corrupt(format!("generation manifest cannot be encoded: {error}")))?;
        write_file(&directory.join(GENERATION_MANIFEST_FILE), &manifest_bytes)?;
        self.io
            .checkpoint(HistoryFaultPoint::GenerationManifestWrite)?;

        // Verify from disk, never from the in-memory value we just built:
        // the durable artifact is what later rebuilds read.
        let loaded = self.load(&id)?;
        self.io.checkpoint(HistoryFaultPoint::GenerationVerify)?;
        Ok(loaded)
    }

    pub fn load(
        &self,
        id: &HistoryGenerationIdV1,
    ) -> HistoryGenerationResult<HistoryGenerationRecordV1> {
        let directory = self.generation_dir(id);
        refuse_symlinked_directory(&directory, "history generation directory")?;
        let manifest_bytes = read_file(&directory.join(GENERATION_MANIFEST_FILE))?
            .ok_or_else(|| corrupt(format!("generation {id} has no manifest")))?;
        let manifest: HistoryGenerationManifestV1 = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| corrupt(format!("generation manifest cannot be decoded: {error}")))?;
        let documents = decode_jsonl(
            &read_file(&directory.join(COMMIT_DOCUMENTS_FILE))?
                .ok_or_else(|| corrupt(format!("generation {id} has no documents file")))?,
        )?;
        let inputs = decode_jsonl(
            &read_file(&directory.join(VECTOR_INPUTS_FILE))?
                .ok_or_else(|| corrupt(format!("generation {id} has no vector inputs file")))?,
        )?;
        let parsed = HistoryGenerationIdV1::parse(&manifest.generation_id)?;
        if &parsed != id {
            return Err(identity_error(format!(
                "generation directory {id} holds manifest {}",
                manifest.generation_id
            )));
        }
        let record = HistoryGenerationRecordV1 {
            id: parsed,
            manifest,
            commit_documents: documents,
            vector_inputs: inputs,
        };
        record.validate()?;
        Ok(record)
    }

    /// Enumerate the generation ids present on disk. Non-directory entries
    /// (the rebuild manifest) and unparseable names are skipped rather than
    /// failing the sweep: an unreadable stray must not disable GC planning.
    pub fn list(&self) -> HistoryGenerationResult<BTreeSet<HistoryGenerationIdV1>> {
        let mut ids = BTreeSet::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ids),
            Err(error) => return Err(io_error(format!("cannot list generations root: {error}"))),
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| io_error(format!("cannot read generations root: {error}")))?;
            // `entry.file_type()` is lstat-based, unlike `Path::is_dir()`:
            // a symlinked entry is skipped rather than enumerated as a
            // generation, matching the refusal in `load`.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Ok(id) = HistoryGenerationIdV1::parse(&name) {
                ids.insert(id);
            }
        }
        Ok(ids)
    }

    /// Remove one generation, refusing any id in the pinned GC-root set.
    ///
    /// Governing section 16: a generation named by a catalog record or by a
    /// prepared/committed rebuild manifest is a GC root and cannot be swept.
    pub fn remove_unreferenced(
        &self,
        id: &HistoryGenerationIdV1,
        roots: &BTreeSet<String>,
    ) -> HistoryGenerationResult<()> {
        if roots.contains(id.as_str()) {
            return Err(HistoryGenerationError::new(
                "error.history_generation_referenced",
                format!("generation {id} is pinned by a catalog record or rebuild manifest"),
            ));
        }
        let directory = self.generation_dir(id);
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(format!("cannot remove generation {id}: {error}"))),
        }
    }

    // -- rebuild manifest ---------------------------------------------------

    fn rebuild_manifest_path(&self) -> PathBuf {
        self.root.join(REBUILD_MANIFEST_FILE)
    }

    pub fn read_rebuild_manifest(
        &self,
    ) -> HistoryGenerationResult<Option<RepoHistoryRebuildManifestV1>> {
        let Some(bytes) = read_file(&self.rebuild_manifest_path())? else {
            return Ok(None);
        };
        let manifest: RepoHistoryRebuildManifestV1 = serde_json::from_slice(&bytes)
            .map_err(|error| corrupt(format!("rebuild manifest cannot be decoded: {error}")))?;
        manifest.validate()?;
        Ok(Some(manifest))
    }

    /// Write the prepared manifest. Prepared is the state that authorizes
    /// the destructive step, so it is written BEFORE the drop and verified
    /// after; see `classify_rebuild_recovery` for the recovery contract.
    pub fn write_prepared_rebuild_manifest(
        &self,
        prepared: RepoHistoryRebuildPreparedV1,
    ) -> HistoryGenerationResult<RepoHistoryRebuildManifestV1> {
        let manifest = RepoHistoryRebuildManifestV1::new_prepared(prepared)?;
        manifest.validate()?;
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|error| corrupt(format!("rebuild manifest cannot be encoded: {error}")))?;
        write_file(&self.rebuild_manifest_path(), &bytes)?;
        self.io
            .checkpoint(HistoryFaultPoint::RebuildManifestPreparedWrite)?;
        Ok(manifest)
    }

    /// Promote the on-disk prepared manifest to committed, binding the
    /// verified replacement views and the resulting catalog epoch.
    pub fn commit_rebuild_manifest(
        &self,
        committed: RepoHistoryRebuildCommittedV1,
    ) -> HistoryGenerationResult<RepoHistoryRebuildManifestV1> {
        let Some(manifest) = self.read_rebuild_manifest()? else {
            return Err(corrupt(
                "cannot commit a rebuild manifest that is not prepared",
            ));
        };
        if manifest.state != RepoHistoryRebuildStateV1::Prepared {
            return Err(corrupt("rebuild manifest is not in the prepared state"));
        }
        let manifest = manifest.into_committed(committed);
        manifest.validate()?;
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|error| corrupt(format!("rebuild manifest cannot be encoded: {error}")))?;
        write_file(&self.rebuild_manifest_path(), &bytes)?;
        self.io
            .checkpoint(HistoryFaultPoint::RebuildManifestCommittedWrite)?;
        Ok(manifest)
    }

    /// Roll back a prepared manifest: delete it and leave everything else
    /// alone. The generations it named stay on disk (immutable and
    /// content-addressed, so re-preparing recreates the same ids for free)
    /// and the last-good lexical and vector views stay selected because
    /// nothing has replaced them yet.
    pub fn roll_back_rebuild_manifest(&self) -> HistoryGenerationResult<()> {
        match fs::remove_file(self.rebuild_manifest_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_error(format!("cannot remove rebuild manifest: {error}")));
            }
        }
        self.io.checkpoint(HistoryFaultPoint::RebuildManifestDelete)
    }

    /// Classify what a restart must do about an observed rebuild manifest.
    ///
    /// ORDERING CONTRACT: this runs before any read view binds, so a resume
    /// arm never races a reader against a half-replaced index. The wiring
    /// that calls it at open is milestone P3-E; P3-D owns the classifier and
    /// its proof.
    ///
    /// Classification is POSITION-RELATIVE to the destructive drop, per plan
    /// section 8 item 3, and reads that position off the index itself rather
    /// than off any extra journal:
    ///
    /// - index directory present with the schema marker the manifest was
    ///   prepared against: the drop has NOT happened, the last-good views are
    ///   still selected, so roll back;
    /// - index directory absent, or present with a different or missing
    ///   marker (the reset removed it and a fresh index was created under the
    ///   new schema): the drop HAS happened, there is no last-good index to
    ///   roll back to, so the only sound arm is resume from the pinned
    ///   generations.
    pub fn classify_rebuild_recovery(
        &self,
        index_path: &Path,
    ) -> HistoryGenerationResult<RepoHistoryRebuildRecoveryV1> {
        let Some(manifest) = self.read_rebuild_manifest()? else {
            return Ok(RepoHistoryRebuildRecoveryV1::NoManifest);
        };
        if manifest.state == RepoHistoryRebuildStateV1::Committed {
            return Ok(RepoHistoryRebuildRecoveryV1::AlreadyCommitted { manifest });
        }
        if manifest.prepared.source_schema_version == PRE_MARKER_SOURCE_SCHEMA {
            // The source index carried no marker, so pre-drop and post-drop
            // are INDISTINGUISHABLE: there is no marker to compare against and
            // a marker-less directory looks the same on both sides of the
            // destructive step. Resume unconditionally, because it is
            // convergent in both worlds - the synchronous rebuild is an
            // idempotent delete-then-re-emit, and rolling back to a
            // marker-less index under a new-schema binary would only
            // re-trigger the very replacement this manifest authorizes.
            // Never RollBack here: that arm's premise is a last-good index
            // this classifier can positively identify, and it cannot.
            return Ok(RepoHistoryRebuildRecoveryV1::ResumePrepared { manifest });
        }
        let marker = read_file(&index_path.join(SCHEMA_VERSION_FILE))?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(|value| value.trim().to_string());
        let source_intact = index_path.is_dir()
            && marker.as_deref() == Some(manifest.prepared.source_schema_version.as_str());
        if source_intact {
            Ok(RepoHistoryRebuildRecoveryV1::RollBackPrepared { manifest })
        } else {
            Ok(RepoHistoryRebuildRecoveryV1::ResumePrepared { manifest })
        }
    }
}

/// Durably publish `bytes` at `path`: temp file, fsync the file, atomic
/// rename, fsync the containing directory, fsync its parent.
///
/// Why this discipline and not `fs::write`, for EVERY file this module
/// writes: these artifacts are the durable crash surface of a DESTRUCTIVE
/// boundary. The prepared rebuild manifest is written before the index drop
/// precisely so a restart can tell "roll back" from "resume"; if a power loss
/// can persist the drop but not the manifest, recovery classifies
/// `NoManifest`, resume never fires, and the carried history is silently
/// lost - the exact window this manifest exists to close. A torn write is
/// just as bad: a half-written manifest fails to decode and wedges
/// classification instead of merely losing it. The same reasoning covers the
/// generation row files and generation manifest, which are what a resume
/// re-emits from.
///
/// Matches the spill lane's discipline in `schema_replacement::write_commit_spill`
/// and the catalog journal's `atomic_replace_sync_nofollow`. The parent fsync
/// is not belt-and-braces: the containing directory is frequently created
/// immediately before this call (a fresh generation directory), and on a
/// non-journaled filesystem an unsynced parent entry can lose the whole
/// directory with the synced file inside it.
fn write_file(path: &Path, bytes: &[u8]) -> HistoryGenerationResult<()> {
    use std::io::Write as _;

    let directory = path
        .parent()
        .ok_or_else(|| unsafe_path("cannot publish a file with no parent directory"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| unsafe_path("cannot publish a file with an unreadable name"))?;
    // The temporary is a FILE beside the target inside the generations tree.
    // `list` enumerates directories only, so a crash-orphaned temporary can
    // never be mistaken for a generation.
    let temporary = directory.join(format!("{file_name}.tmp"));
    {
        let mut file = fs::File::create(&temporary)
            .map_err(|error| io_error(format!("cannot create {}: {error}", temporary.display())))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error(format!("cannot write {}: {error}", temporary.display())))?;
    }
    fs::rename(&temporary, path)
        .map_err(|error| io_error(format!("cannot publish {}: {error}", path.display())))?;
    fsync_dir(directory)?;
    if let Some(parent) = directory.parent() {
        fsync_dir(parent)?;
    }
    Ok(())
}

fn fsync_dir(path: &Path) -> HistoryGenerationResult<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error(format!("cannot sync {}: {error}", path.display())))
}

/// Refuse a path that is a symlink, or that exists as a non-directory.
///
/// The catalog family opens every component `O_NOFOLLOW` and this module's
/// own `scan_commit_documents` already refuses a symlinked `index_path`; the
/// generations tree holds the only checkout-independent copy of commit
/// history, so it gets the same confinement rather than trusting whatever a
/// symlink points at.
fn refuse_symlinked_directory(path: &Path, role: &str) -> HistoryGenerationResult<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(format!("cannot stat {role}: {error}"))),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(unsafe_path(format!("{role} is a symlink")))
        }
        Ok(metadata) if !metadata.is_dir() => {
            Err(unsafe_path(format!("{role} is not a directory")))
        }
        Ok(_) => Ok(()),
    }
}

fn read_file(path: &Path) -> HistoryGenerationResult<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(format!("cannot read {}: {error}", path.display()))),
    }
}

// ---------------------------------------------------------------------------
// Rebuild manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoHistoryRebuildStateV1 {
    Prepared,
    Committed,
}

/// One row of the complete namespace inventory the prepared manifest binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRebuildNamespaceV1 {
    pub namespace: CommitNamespace,
    pub generation_id: String,
    pub commit_document_count: u64,
    pub commit_document_commitment_sha256: String,
    pub disposition: RepoHistoryRebuildDispositionV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoHistoryRebuildDispositionV1 {
    /// The namespace is a repo-history record's PRIMARY namespace, and that
    /// record's `materialization` names this generation.
    Owned,
    /// The namespace is a repo-history record's COMPATIBILITY namespace: a
    /// legacy-lookup surface the record still answers for, but not the one
    /// its single `materialization` field names.
    ///
    /// Its generation is catalog-ATTRIBUTED (so it carries an `rhg_` id, not
    /// a quarantine id) but manifest-OWNED: no catalog record names it, so
    /// like an unclaimed generation its only durable identity is this
    /// manifest. It gets its own bucket rather than sharing `Owned` because
    /// an auditor asking "which generations are reachable from the catalog?"
    /// would otherwise be told yes about a generation that is not.
    OwnedCompatibility,
    Ambiguous,
    Unclaimed,
}

/// Which proof the materializer was able to run against the persisted Phase 1
/// namespace-inventory asset.
///
/// The asset is a POINT-IN-TIME migration record. It is an exact description
/// of the index only while the index is unchanged since migration; an index
/// that has been live-indexed since then legitimately outgrows it, both by
/// gaining namespaces (a `local_` mint for a new project) and by growing
/// within recorded ones (append-only history). Equality is therefore the
/// right contract for exactly one shape, and the mode records which one ran.
///
/// CONSUMER CONTRACT: an offline rebuild that needs the asset to be an exact
/// description of what it is rebuilding (the Phase 6 path-free-rebuild
/// subcommand) must REQUIRE `Equality` and refuse `Drift`. `Drift` proves
/// only that no recorded history was lost, not that the asset enumerates
/// everything present. A consumer that treats the two as interchangeable is
/// reading a weaker proof than it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryProofModeV1 {
    /// The recomputed source fingerprint equals the recorded one: the index
    /// is unchanged since migration, so per-namespace count AND commitment
    /// equality is provable and is enforced.
    Equality,
    /// The fingerprints differ, or no comparable fingerprint could be
    /// recomputed. Recorded namespaces must still be present and must not
    /// have shrunk; commitments are not compared, because a fold hash cannot
    /// prove subset containment.
    Drift,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRebuildPreparedV1 {
    pub source_index_fingerprint_sha256: String,
    pub source_schema_version: String,
    /// Which asset proof ran for this rebuild. See [`HistoryProofModeV1`] for
    /// the consumer contract; a reader that requires an exact asset must
    /// refuse anything but `Equality`.
    pub proof_mode: HistoryProofModeV1,
    /// The `source_index_fingerprint` the migration asset recorded, and the
    /// one recomputed over the index this rebuild observed. Both are stored
    /// even in `Equality` mode so a later audit can re-derive the mode
    /// decision instead of trusting it. `None` means no asset was consulted
    /// (a fresh v2 store) or no comparable value could be recomputed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_source_index_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_source_index_fingerprint: Option<String>,
    /// The COMPLETE namespace inventory observed at prepare time, including
    /// unclaimed namespaces. This manifest is the only durable owner of an
    /// unclaimed generation's identity.
    pub namespace_inventory: Vec<RepoHistoryRebuildNamespaceV1>,
    pub catalog_epoch: u64,
    pub owned_generation_ids: BTreeSet<String>,
    /// Generations for records' compatibility namespaces. Defaulted so a
    /// manifest written before this bucket existed still decodes; absent
    /// means "none recorded", and a reader must not read the empty set as
    /// proof that no compatibility namespace existed.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub compatibility_generation_ids: BTreeSet<String>,
    pub ambiguous_generation_ids: BTreeSet<String>,
    pub unclaimed_generation_ids: BTreeSet<String>,
    pub planned_lexical_generation_label: String,
    pub planned_vector_generation_label: String,
}

/// Per-generation vector-coverage evidence for one committed replacement
/// (governing section 10.3): how many of the generation's vector-input rows were
/// PROVED to have an active vector, and how many were re-enqueued because they
/// were not. A replacement must never commit a lexical-only history view whose
/// vector view was promised, so the manifest records which of the two happened
/// per generation rather than leaving it in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRebuildVectorRowV1 {
    pub generation_id: String,
    pub vector_inputs_verified: u64,
    pub vector_inputs_reenqueued: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRebuildCommittedV1 {
    pub verified_lexical_view: String,
    pub verified_vector_view: String,
    pub resulting_catalog_epoch: u64,
    /// Defaulted so a manifest committed before this field existed still
    /// decodes. Absent means "no inventory recorded", never "nothing to
    /// record": a reader must not treat the empty vector as proof of coverage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vector_inventory: Vec<RepoHistoryRebuildVectorRowV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistoryRebuildManifestV1 {
    pub version: u32,
    pub rebuild_id: String,
    pub state: RepoHistoryRebuildStateV1,
    pub prepared: RepoHistoryRebuildPreparedV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed: Option<RepoHistoryRebuildCommittedV1>,
}

impl RepoHistoryRebuildManifestV1 {
    fn new_prepared(prepared: RepoHistoryRebuildPreparedV1) -> HistoryGenerationResult<Self> {
        let rebuild_id = derive_rebuild_id(&prepared)?;
        Ok(Self {
            version: REBUILD_MANIFEST_VERSION_V1,
            rebuild_id,
            state: RepoHistoryRebuildStateV1::Prepared,
            prepared,
            committed: None,
        })
    }

    fn into_committed(mut self, committed: RepoHistoryRebuildCommittedV1) -> Self {
        self.state = RepoHistoryRebuildStateV1::Committed;
        self.committed = Some(committed);
        self
    }

    /// Every generation id this manifest pins, in either state. Governing
    /// section 16 makes both prepared and committed manifests GC roots.
    pub fn pinned_generation_ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        ids.extend(self.prepared.owned_generation_ids.iter().cloned());
        // Load-bearing: a compatibility generation has NO catalog record
        // naming it, so this manifest is its only GC root. Dropping it from
        // this union makes it sweepable.
        ids.extend(self.prepared.compatibility_generation_ids.iter().cloned());
        ids.extend(self.prepared.ambiguous_generation_ids.iter().cloned());
        ids.extend(self.prepared.unclaimed_generation_ids.iter().cloned());
        ids
    }

    pub fn validate(&self) -> HistoryGenerationResult<()> {
        if self.version != REBUILD_MANIFEST_VERSION_V1 {
            return Err(corrupt(format!(
                "rebuild manifest version {} is not supported",
                self.version
            )));
        }
        if derive_rebuild_id(&self.prepared)? != self.rebuild_id {
            return Err(identity_error(
                "rebuild manifest does not re-derive its own id",
            ));
        }
        match (self.state, &self.committed) {
            (RepoHistoryRebuildStateV1::Prepared, Some(_)) => {
                return Err(corrupt(
                    "prepared rebuild manifest carries committed evidence",
                ));
            }
            (RepoHistoryRebuildStateV1::Committed, None) => {
                return Err(corrupt(
                    "committed rebuild manifest carries no committed evidence",
                ));
            }
            _ => {}
        }
        let mut named = BTreeSet::new();
        for row in &self.prepared.namespace_inventory {
            let id = HistoryGenerationIdV1::parse(&row.generation_id)?;
            let owned = matches!(
                row.disposition,
                RepoHistoryRebuildDispositionV1::Owned
                    | RepoHistoryRebuildDispositionV1::OwnedCompatibility
            );
            if owned != id.owned().is_some() {
                return Err(identity_error(
                    "rebuild manifest namespace disposition disagrees with its generation id shape",
                ));
            }
            if !named.insert(row.namespace.clone()) {
                return Err(corrupt("rebuild manifest repeats a namespace"));
            }
            let bucket = match row.disposition {
                RepoHistoryRebuildDispositionV1::Owned => &self.prepared.owned_generation_ids,
                RepoHistoryRebuildDispositionV1::OwnedCompatibility => {
                    &self.prepared.compatibility_generation_ids
                }
                RepoHistoryRebuildDispositionV1::Ambiguous => {
                    &self.prepared.ambiguous_generation_ids
                }
                RepoHistoryRebuildDispositionV1::Unclaimed => {
                    &self.prepared.unclaimed_generation_ids
                }
            };
            if !bucket.contains(&row.generation_id) {
                return Err(corrupt(
                    "rebuild manifest namespace row is missing from its disposition bucket",
                ));
            }
        }
        let bucketed = self.prepared.owned_generation_ids.len()
            + self.prepared.compatibility_generation_ids.len()
            + self.prepared.ambiguous_generation_ids.len()
            + self.prepared.unclaimed_generation_ids.len();
        if bucketed != self.prepared.namespace_inventory.len() {
            return Err(corrupt(
                "rebuild manifest bucket totals disagree with its namespace inventory",
            ));
        }
        Ok(())
    }
}

fn derive_rebuild_id(prepared: &RepoHistoryRebuildPreparedV1) -> HistoryGenerationResult<String> {
    let canonical = serde_json::to_vec(prepared)
        .map_err(|error| corrupt(format!("rebuild manifest cannot be encoded: {error}")))?;
    let mut hasher = Sha256::new();
    put_field(&mut hasher, REBUILD_MANIFEST_ID_DOMAIN);
    put_field(&mut hasher, &canonical);
    Ok(format!("rhb_{}", hex::encode(hasher.finalize())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoHistoryRebuildRecoveryV1 {
    NoManifest,
    /// Prepared observed with the source index still intact: undo.
    RollBackPrepared {
        manifest: RepoHistoryRebuildManifestV1,
    },
    /// Prepared observed after the drop: there is no last-good index to
    /// return to, so the replacement must be re-executed from the pinned
    /// generations.
    ResumePrepared {
        manifest: RepoHistoryRebuildManifestV1,
    },
    AlreadyCommitted {
        manifest: RepoHistoryRebuildManifestV1,
    },
}

// ---------------------------------------------------------------------------
// Index scan
// ---------------------------------------------------------------------------

/// Everything one namespace contributed to the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryNamespaceCaptureV1 {
    pub namespace: String,
    pub commit_documents: Vec<HistoryCommitDocumentV1>,
    pub vector_inputs: Vec<HistoryVectorInputV1>,
    pub truncated_message_count: u64,
    pub commit_document_commitment_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryIndexScanV1 {
    pub schema_version: String,
    pub schema_fingerprint_sha256: String,
    /// Fingerprint over THIS scan's evidence. Deliberately not comparable to
    /// the migration asset's `source_index_fingerprint`, which folds owner
    /// subsource states rather than commit rows.
    pub source_index_fingerprint_sha256: String,
    pub namespaces: BTreeMap<String, HistoryNamespaceCaptureV1>,
}

/// Schema evidence for a generation created from a LIVE walk rather than
/// from a scan of an outgoing index.
///
/// The pre-replacement materializer reads its evidence off the index it is
/// about to destroy. A live refresh has no such index to read: it is writing
/// INTO the running one, at the running schema. This helper supplies the same
/// two values from the running binary so both creation callers populate the
/// generation body identically in shape.
///
/// The third value, `source_index_fingerprint_sha256`, is deliberately NOT
/// produced here: it fingerprints an observed document population, and a live
/// refresh's population is the generation's own rows. The live caller passes
/// a documented constant marker for that field (see
/// `LIVE_REFRESH_SOURCE_MARKER` in the refresh module): non-hex, never
/// validated as a SHA-256, and never compared against a scan fingerprint.
pub fn live_schema_evidence() -> HistoryGenerationResult<(String, String)> {
    let (schema, _fields) = super::build_schema();
    let schema_bytes = serde_json::to_vec(&schema)
        .map_err(|error| corrupt(format!("index schema cannot be encoded: {error}")))?;
    let mut hasher = Sha256::new();
    hasher.update(SOURCE_FINGERPRINT_DOMAIN);
    put_field(&mut hasher, &schema_bytes);
    Ok((
        super::INDEX_SCHEMA_VERSION.to_string(),
        hex::encode(hasher.finalize()),
    ))
}

/// Stream the legacy index's commit documents, grouped by exact namespace.
///
/// Three shapes yield `Ok(None)` - an empty scan rather than a refusal -
/// because in each of them there is genuinely nothing to carry: a missing
/// index directory (a fresh v2 store with no legacy residue), and a directory
/// that holds no tantivy index at all (see the not-an-index arm below).
/// Everything else scans, including an index carrying no schema marker.
pub fn scan_commit_documents(
    index_path: &Path,
    limits: HistoryScanLimitsV1,
) -> HistoryGenerationResult<Option<HistoryIndexScanV1>> {
    if !strict_absolute_path(index_path) {
        return Err(unsafe_path(
            "index path must be absolute and free of traversal",
        ));
    }
    match fs::symlink_metadata(index_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(format!("cannot stat index path: {error}"))),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(unsafe_path("index path is a symlink"));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(unsafe_path("index path is not a directory"));
        }
        Ok(_) => {}
    }
    let marker = read_file(&index_path.join(SCHEMA_VERSION_FILE))?;
    if marker.is_none() {
        // NOT-AN-INDEX arm. Removing the old marker-absent early return
        // exposes `Index::open_in_dir` below to directories it never used to
        // see, and that call fails on any directory without tantivy's
        // `meta.json` (which is why `TranscriptIndex::open_or_create` wraps it
        // in open-else-create). The arm is production-reachable:
        // `reset_index_on_schema_mismatch` triggers when the marker is absent
        // AND the directory is non-empty, so a directory holding one stray
        // file and no marker both triggers the replacement and reaches this
        // guard. Letting that return `Err` would make both replacement guards
        // refuse and block boot.
        //
        // The distinction this draws, and it is the whole point: "no tantivy
        // index here" (no `meta.json`) is NOTHING TO CARRY, so `Ok(None)`;
        // "an index is here but it will not open or read" (`meta.json`
        // present, open or a later read fails) is genuinely corrupt state and
        // stays fail-closed as `Err`. Discriminating on the metadata file
        // BEFORE the open is deliberate: classifying tantivy's open errors
        // after the fact would couple this arm to that crate's error strings.
        if !index_path.join(TANTIVY_META_FILE).exists() {
            return Ok(None);
        }
    }
    // The observed marker is RECORDED, never required to equal the running
    // `INDEX_SCHEMA_VERSION`. This scan's entire purpose is to run BEFORE a
    // destructive schema replacement, so by construction it usually reads an
    // index whose marker is the outgoing version while the binary already
    // carries the incoming one. Requiring equality (as the Phase 1 capture
    // does, correctly, because it runs on an at-schema store) would refuse
    // exactly the case the materializer exists for. Fail-closed comes from
    // the field lookups below instead: a schema that cannot supply every
    // commit-document field refuses rather than emitting a partial
    // generation.
    // A marker-less index scans and records the reserved sentinel; a marker
    // that is present must still be well formed, so a corrupt or empty one
    // refuses exactly as before rather than silently degrading to the
    // sentinel.
    let schema_version = match marker {
        Some(bytes) => {
            let observed = String::from_utf8(bytes)
                .map_err(|_| corrupt("index schema marker is not valid utf-8"))?
                .trim()
                .to_string();
            if observed.is_empty() {
                return Err(corrupt("index schema marker is empty"));
            }
            observed
        }
        None => PRE_MARKER_SOURCE_SCHEMA.to_string(),
    };

    let index = Index::open_in_dir(index_path)
        .map_err(|error| io_error(format!("cannot open index: {error}")))?;
    let schema = index.schema();
    let schema_bytes = serde_json::to_vec(&schema)
        .map_err(|error| corrupt(format!("index schema cannot be encoded: {error}")))?;
    let mut schema_hasher = Sha256::new();
    schema_hasher.update(SOURCE_FINGERPRINT_DOMAIN);
    put_field(&mut schema_hasher, &schema_bytes);
    let schema_fingerprint = hex::encode(schema_hasher.finalize());

    let field = |name: &str| {
        schema
            .get_field(name)
            .map_err(|_| corrupt(format!("index schema has no {name} field")))
    };
    let entity_id = field("entity_id")?;
    let doc_type = field("doc_type")?;
    let chunk_kind = field("chunk_kind")?;
    let repo_id = field("repo_id")?;
    let commit_sha = field("commit_sha")?;
    let chunk_hash = field("chunk_hash")?;
    let content = field("content")?;
    let path_tokens = field("path_tokens")?;
    let parser_version = field("parser_version")?;
    let commit_author_name = field("commit_author_name")?;
    let commit_author_email = field("commit_author_email")?;
    let session_id = field("session_id")?;
    let account = field("account")?;
    let role = field("role")?;
    let byte_offset = field("byte_offset")?;
    let is_subagent = field("is_subagent")?;

    let reader = index
        .reader()
        .map_err(|error| io_error(format!("cannot open index reader: {error}")))?;
    let searcher = reader.searcher();
    if searcher.num_docs() > limits.max_documents {
        return Err(corrupt("index document count exceeds the scan limit"));
    }
    let addresses = searcher
        .search(&AllQuery, &DocSetCollector)
        .map_err(|error| io_error(format!("cannot scan index: {error}")))?;

    let mut by_namespace: BTreeMap<String, Vec<HistoryCommitDocumentV1>> = BTreeMap::new();
    let mut total_string_bytes = 0usize;
    for address in addresses {
        let document: TantivyDocument = searcher
            .doc(address)
            .map_err(|error| io_error(format!("cannot read index document: {error}")))?;
        if optional_text(&document, doc_type).as_deref() != Some("commit") {
            continue;
        }
        let namespace = optional_text(&document, repo_id).unwrap_or_default();
        let sha = optional_text(&document, commit_sha).unwrap_or_default();
        let entity = optional_text(&document, entity_id).unwrap_or_default();
        let hash = optional_text(&document, chunk_hash).unwrap_or_default();
        let body = optional_text(&document, content).unwrap_or_default();
        if namespace.is_empty() || sha.is_empty() || entity.is_empty() || hash.is_empty() {
            return Err(corrupt("index commit row is incomplete"));
        }
        if !matches!(
            EntityRef::parse(&entity),
            Ok(EntityRef::Commit { repo_id: parsed_ns, sha: parsed_sha })
                if parsed_ns == namespace && parsed_sha == sha
        ) {
            return Err(corrupt("index commit row has an invalid entity ref"));
        }
        total_string_bytes = total_string_bytes
            .checked_add(namespace.len() + sha.len() + entity.len() + hash.len() + body.len())
            .filter(|value| *value <= limits.max_total_string_bytes)
            .ok_or_else(|| corrupt("index commit scan exceeds its string byte limit"))?;
        by_namespace
            .entry(namespace.clone())
            .or_default()
            .push(HistoryCommitDocumentV1 {
                entity_id: entity,
                doc_type: "commit".to_string(),
                chunk_kind: optional_text(&document, chunk_kind).unwrap_or_default(),
                repo_id: namespace,
                commit_sha: sha,
                content: body,
                content_hash: hash,
                path_tokens: optional_text(&document, path_tokens).unwrap_or_default(),
                parser_version: optional_text(&document, parser_version).unwrap_or_default(),
                commit_author_name: optional_text(&document, commit_author_name)
                    .unwrap_or_default(),
                commit_author_email: optional_text(&document, commit_author_email)
                    .unwrap_or_default(),
                session_id: optional_text(&document, session_id).unwrap_or_default(),
                account: optional_text(&document, account).unwrap_or_default(),
                role: optional_text(&document, role).unwrap_or_default(),
                byte_offset: optional_u64(&document, byte_offset).unwrap_or_default(),
                is_subagent: optional_u64(&document, is_subagent).unwrap_or_default(),
            });
        if by_namespace.len() > limits.max_commit_namespaces {
            return Err(corrupt(
                "index commit namespace count exceeds the scan limit",
            ));
        }
    }

    let mut namespaces = BTreeMap::new();
    for (namespace, mut documents) in by_namespace {
        documents.sort_by(|left, right| {
            (
                &left.repo_id,
                &left.entity_id,
                &left.commit_sha,
                &left.content_hash,
            )
                .cmp(&(
                    &right.repo_id,
                    &right.entity_id,
                    &right.commit_sha,
                    &right.content_hash,
                ))
        });
        let truncated_message_count = documents
            .iter()
            .filter(|document| document.content.ends_with(TRUNCATED_COMMIT_MESSAGE_SUFFIX))
            .count() as u64;
        // One vector input per commit document: `git_history` emits
        // `emit_git_message` for every commit document it writes, so the two
        // sets are 1:1 by construction at the source.
        let vector_inputs = documents
            .iter()
            .map(|document| HistoryVectorInputV1 {
                entity_id: document.entity_id.clone(),
                content_hash: document.content_hash.clone(),
                message: document.content.clone(),
            })
            .collect::<Vec<_>>();
        let commitment = hash_commit_rows(&commit_rows_for(&documents));
        namespaces.insert(
            namespace.clone(),
            HistoryNamespaceCaptureV1 {
                namespace,
                commit_documents: documents,
                vector_inputs,
                truncated_message_count,
                commit_document_commitment_sha256: commitment,
            },
        );
    }

    let mut source = Sha256::new();
    source.update(SOURCE_FINGERPRINT_DOMAIN);
    put_field(&mut source, schema_version.as_bytes());
    put_field(&mut source, schema_fingerprint.as_bytes());
    for capture in namespaces.values() {
        put_field(&mut source, capture.namespace.as_bytes());
        put_field(
            &mut source,
            &(capture.commit_documents.len() as u64).to_be_bytes(),
        );
        put_field(
            &mut source,
            capture.commit_document_commitment_sha256.as_bytes(),
        );
    }

    Ok(Some(HistoryIndexScanV1 {
        schema_version,
        schema_fingerprint_sha256: schema_fingerprint,
        source_index_fingerprint_sha256: hex::encode(source.finalize()),
        namespaces,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index::INDEX_SCHEMA_VERSION;

    #[test]
    fn existing_only_open_never_initializes_a_missing_generation_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = root.join("index");
        let generations = generations_root_for_index(&index).unwrap();
        assert!(HistoryGenerationStore::open_existing_for_index(&index).is_err());
        assert!(!generations.exists());

        HistoryGenerationStore::open_for_index(&index).unwrap();
        HistoryGenerationStore::open_existing_for_index(&index).unwrap();
    }

    fn namespace(value: &str) -> CommitNamespace {
        CommitNamespace::parse(value).unwrap()
    }

    fn history_id(value: &str) -> RepoHistoryId {
        RepoHistoryId::parse(value).unwrap()
    }

    fn document(ns: &str, sha: &str, message: &str) -> HistoryCommitDocumentV1 {
        HistoryCommitDocumentV1 {
            entity_id: format!("commit:{ns}:{sha}"),
            doc_type: "commit".to_string(),
            chunk_kind: "git_message".to_string(),
            repo_id: ns.to_string(),
            commit_sha: sha.to_string(),
            content: message.to_string(),
            content_hash: hex::encode(Sha256::digest(message.as_bytes())),
            path_tokens: message.lines().next().unwrap_or_default().to_string(),
            parser_version: "p1".to_string(),
            commit_author_name: "Fixture".to_string(),
            commit_author_email: "fixture@example.invalid".to_string(),
            session_id: String::new(),
            account: "git".to_string(),
            role: "commit".to_string(),
            byte_offset: 0,
            is_subagent: 0,
        }
    }

    fn input(ns: &str, documents: Vec<HistoryCommitDocumentV1>) -> HistoryGenerationInputV1 {
        let vector_inputs = documents
            .iter()
            .map(|document| HistoryVectorInputV1 {
                entity_id: document.entity_id.clone(),
                content_hash: document.content_hash.clone(),
                message: document.content.clone(),
            })
            .collect();
        HistoryGenerationInputV1 {
            namespace: namespace(ns),
            owner: HistoryGenerationOwnerV1::Owned {
                repo_history_id: history_id("rh_00000000000000000000000000000001"),
            },
            commit_documents: documents,
            vector_inputs,
            truncated_message_count: 0,
            source_schema_version: INDEX_SCHEMA_VERSION.to_string(),
            source_schema_fingerprint_sha256: "0".repeat(64),
            source_index_fingerprint_sha256: "1".repeat(64),
        }
    }

    #[test]
    fn generations_root_is_a_sibling_of_the_index() {
        let root = generations_root_for_index(Path::new("/state/index")).unwrap();
        assert_eq!(root, PathBuf::from("/state/history-generations"));
        assert!(!root.starts_with("/state/index"));
    }

    #[test]
    fn generations_root_refuses_a_relative_index_path() {
        let error = generations_root_for_index(Path::new("state/index")).unwrap_err();
        assert_eq!(error.code(), "error.history_generation_unsafe_path");
    }

    #[test]
    fn create_is_idempotent_and_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let documents = vec![
            document("ns1", "a".repeat(40).as_str(), "first"),
            document("ns1", "b".repeat(40).as_str(), "second"),
        ];
        let first = store
            .create_or_open(input("ns1", documents.clone()))
            .unwrap();
        let second = store.create_or_open(input("ns1", documents)).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first, second);
        assert!(first.id.as_str().starts_with("rhg_"));
    }

    #[test]
    fn source_evidence_drift_re_derives_the_same_id_and_keeps_first_writer_evidence() {
        // D-039: the id is a pure content address. The same carried history
        // observed under a different schema marker, schema fingerprint, and
        // index fingerprint (the next schema bump's scan, or the live-refresh
        // constant marker) must re-derive the SAME id, open the existing
        // generation, and retain the first writer's evidence.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let documents = vec![
            document("ns1", "a".repeat(40).as_str(), "first"),
            document("ns1", "b".repeat(40).as_str(), "second"),
        ];
        let first = store
            .create_or_open(input("ns1", documents.clone()))
            .unwrap();

        let mut drifted = input("ns1", documents);
        drifted.source_schema_version = "v999".to_string();
        drifted.source_schema_fingerprint_sha256 = "f".repeat(64);
        drifted.source_index_fingerprint_sha256 =
            "blackbox.repo-history-generation.live-refresh.v1".to_string();
        let second = store.create_or_open(drifted).unwrap();

        assert_eq!(first.id, second.id);
        // First writer wins on provenance: the on-disk record is returned
        // unchanged, still carrying the original evidence.
        assert_eq!(second, first);
        assert_eq!(
            second.manifest.body.source_schema_version,
            INDEX_SCHEMA_VERSION.to_string()
        );
    }

    #[test]
    fn content_drift_still_changes_the_id() {
        // The evidence exclusion must not weaken content addressing: any
        // change to the carried rows still mints a different id.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let base = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let grown = store
            .create_or_open(input(
                "ns1",
                vec![
                    document("ns1", &"a".repeat(40), "first"),
                    document("ns1", &"b".repeat(40), "second"),
                ],
            ))
            .unwrap();
        assert_ne!(base.id, grown.id);
    }

    #[test]
    fn quarantine_dispositions_mint_quarantine_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let mut request = input("ns1", vec![document("ns1", &"a".repeat(40), "first")]);
        request.owner = HistoryGenerationOwnerV1::Unclaimed {
            inventory_diagnostic: "no catalog owner".to_string(),
        };
        let record = store.create_or_open(request).unwrap();
        assert!(record.id.as_str().starts_with("rhq_"));
        assert!(record.id.owned().is_none());
    }

    #[test]
    fn owner_change_reminting_yields_a_different_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let documents = vec![document("ns1", &"a".repeat(40), "first")];
        let owned = store
            .create_or_open(input("ns1", documents.clone()))
            .unwrap();
        let mut ambiguous_request = input("ns1", documents);
        ambiguous_request.owner = HistoryGenerationOwnerV1::Ambiguous {
            candidate_repo_history_ids: [
                history_id("rh_00000000000000000000000000000001"),
                history_id("rh_00000000000000000000000000000002"),
            ]
            .into_iter()
            .collect(),
        };
        let ambiguous = store.create_or_open(ambiguous_request).unwrap();
        assert_ne!(owned.id, ambiguous.id);
    }

    #[test]
    fn validate_refuses_a_tampered_manifest_body() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let mut tampered = record.clone();
        tampered.manifest.body.commit_document_count = 7;
        // Every body field is in the id preimage, so editing any one of them
        // is caught by the re-derive before any per-field clause runs.
        assert_eq!(
            tampered.validate().unwrap_err().code(),
            "error.history_generation_identity"
        );
    }

    #[test]
    fn validate_refuses_a_tampered_row_set() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![
                    document("ns1", &"a".repeat(40), "first"),
                    document("ns1", &"b".repeat(40), "second"),
                ],
            ))
            .unwrap();
        let mut tampered = record.clone();
        tampered.commit_documents.pop();
        assert_eq!(
            tampered.validate().unwrap_err().code(),
            "error.history_generation_corrupt"
        );
    }

    #[test]
    fn an_empty_generation_carries_the_empty_set_commitments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let empty = store.create_or_open(input("ns1", Vec::new())).unwrap();
        assert_eq!(empty.manifest.body.commit_document_count, 0);
        assert_eq!(
            empty.manifest.body.commit_document_commitment_sha256,
            hash_commit_rows(&[])
        );
        assert_eq!(empty.manifest.body.vector_input_count, 0);
        assert_eq!(
            empty.manifest.body.vector_input_commitment_sha256,
            hash_vector_inputs(&[])
        );
        empty.validate().unwrap();
    }

    #[test]
    fn load_refuses_a_generation_whose_documents_were_edited_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let documents = root
            .join(HISTORY_GENERATIONS_DIRNAME)
            .join(record.id.as_str())
            .join(COMMIT_DOCUMENTS_FILE);
        fs::write(&documents, b"").unwrap();
        assert_eq!(
            store.load(&record.id).unwrap_err().code(),
            "error.history_generation_corrupt"
        );
    }

    #[test]
    fn remove_refuses_a_pinned_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let roots = [record.id.as_str().to_string()].into_iter().collect();
        let error = store.remove_unreferenced(&record.id, &roots).unwrap_err();
        assert_eq!(error.code(), "error.history_generation_referenced");
        store
            .remove_unreferenced(&record.id, &BTreeSet::new())
            .unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn list_ignores_the_rebuild_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        store
            .write_prepared_rebuild_manifest(prepared_for(&record))
            .unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed.contains(&record.id));
    }

    fn prepared_for(record: &HistoryGenerationRecordV1) -> RepoHistoryRebuildPreparedV1 {
        RepoHistoryRebuildPreparedV1 {
            source_index_fingerprint_sha256: "1".repeat(64),
            source_schema_version: INDEX_SCHEMA_VERSION.to_string(),
            proof_mode: HistoryProofModeV1::Equality,
            recorded_source_index_fingerprint: None,
            observed_source_index_fingerprint: None,
            namespace_inventory: vec![RepoHistoryRebuildNamespaceV1 {
                namespace: record.manifest.body.namespace.clone(),
                generation_id: record.id.as_str().to_string(),
                commit_document_count: record.manifest.body.commit_document_count,
                commit_document_commitment_sha256: record
                    .manifest
                    .body
                    .commit_document_commitment_sha256
                    .clone(),
                disposition: RepoHistoryRebuildDispositionV1::Owned,
            }],
            catalog_epoch: 4,
            owned_generation_ids: [record.id.as_str().to_string()].into_iter().collect(),
            compatibility_generation_ids: BTreeSet::new(),
            ambiguous_generation_ids: BTreeSet::new(),
            unclaimed_generation_ids: BTreeSet::new(),
            planned_lexical_generation_label: "lexical-1".to_string(),
            planned_vector_generation_label: "vector-1".to_string(),
        }
    }

    /// F1: every write class publishes through a temp file that must not
    /// survive. A torn write cannot be simulated portably, so this asserts
    /// the MECHANISM instead: no `.tmp` remains anywhere under the
    /// generations tree, and every final name still decodes.
    fn assert_no_temporaries(root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().to_string();
                assert!(
                    !name.ends_with(".tmp"),
                    "temporary survived publication: {}",
                    entry.path().display()
                );
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }

    #[test]
    fn every_write_class_publishes_atomically_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();

        // Class 1 and 2: the generation row files and the generation manifest.
        let record = store
            .create_or_open(input(
                "ns1",
                vec![
                    document("ns1", &"a".repeat(40), "first"),
                    document("ns1", &"b".repeat(40), "second"),
                ],
            ))
            .unwrap();
        assert_no_temporaries(store.root());
        assert_eq!(store.load(&record.id).unwrap(), record);

        // Class 3: the prepared rebuild manifest.
        let prepared = store
            .write_prepared_rebuild_manifest(prepared_for(&record))
            .unwrap();
        assert_no_temporaries(store.root());
        assert_eq!(store.read_rebuild_manifest().unwrap().unwrap(), prepared);

        // Class 4: the committed rebuild manifest.
        let committed = store
            .commit_rebuild_manifest(RepoHistoryRebuildCommittedV1 {
                verified_lexical_view: "lexical-1".to_string(),
                verified_vector_view: "vector-1".to_string(),
                resulting_catalog_epoch: 6,
                vector_inventory: Vec::new(),
            })
            .unwrap();
        assert_no_temporaries(store.root());
        assert_eq!(store.read_rebuild_manifest().unwrap().unwrap(), committed);
        assert_eq!(committed.state, RepoHistoryRebuildStateV1::Committed);

        // A stray temporary must never be enumerated as a generation.
        fs::write(store.root().join("rebuild-manifest.json.tmp"), b"partial").unwrap();
        assert_eq!(
            store.list().unwrap(),
            [record.id.clone()].into_iter().collect()
        );
    }

    #[test]
    fn a_symlinked_generations_root_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        // The generations root is `<parent-of-index>/history-generations`.
        let index_path = root.join("state").join("index");
        fs::create_dir_all(root.join("state")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, root.join("state").join("history-generations"))
            .unwrap();
        #[cfg(unix)]
        {
            let error = HistoryGenerationStore::open_for_index(&index_path).unwrap_err();
            assert_eq!(error.code(), "error.history_generation_unsafe_path");
        }
        let _ = index_path;
    }

    #[test]
    fn a_symlinked_generation_directory_refuses_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let real = store.root().join(record.id.as_str());
        let moved = root.join("moved-generation");
        fs::rename(&real, &moved).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&moved, &real).unwrap();
            let error = store.load(&record.id).unwrap_err();
            assert_eq!(error.code(), "error.history_generation_unsafe_path");
            // And it is not enumerated as a generation either.
            assert!(store.list().unwrap().is_empty());
        }
        let _ = moved;
    }

    // --- pre-marker source scan -------------------------------------------
    //
    // `reset_index_on_schema_mismatch` triggers when the marker is ABSENT and
    // the directory is non-empty, so the replacement guards must cope with a
    // marker-less index rather than refusing it. These rows pin the three
    // decisions that made that possible: the scan proceeds, it records a
    // reserved sentinel deterministically, and a directory that holds no
    // index at all is still nothing to carry.

    /// Build a real, marker-less tantivy index carrying one commit document.
    fn marker_less_index_with_a_commit(root: &Path) -> PathBuf {
        let index_path = root.join("index");
        fs::create_dir_all(&index_path).unwrap();
        let (schema, fields) = crate::index::build_schema();
        let index = tantivy::Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_code_tokenizer(&index);
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        let sha = "c".repeat(40);
        let mut doc = TantivyDocument::new();
        doc.add_text(fields.doc_type, "commit");
        doc.add_text(fields.chunk_kind, "git_message");
        doc.add_text(fields.entity_id, format!("commit:premarker-ns:{sha}"));
        doc.add_text(fields.content, "pre-marker commit");
        doc.add_text(
            fields.chunk_hash,
            hex::encode(Sha256::digest(b"pre-marker commit")),
        );
        doc.add_text(fields.repo_id, "premarker-ns");
        doc.add_text(fields.commit_sha, &sha);
        doc.add_u64(fields.byte_offset, 0);
        doc.add_u64(fields.is_subagent, 0);
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert!(!index_path.join(SCHEMA_VERSION_FILE).exists());
        index_path
    }

    #[test]
    fn a_marker_less_index_scans_and_records_the_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = marker_less_index_with_a_commit(&root);
        let scan = scan_commit_documents(&index_path, HistoryScanLimitsV1::default())
            .unwrap()
            .expect("a marker-less index still scans");
        assert_eq!(scan.schema_version, PRE_MARKER_SOURCE_SCHEMA);
        assert_eq!(scan.namespaces.len(), 1);
        assert_eq!(scan.namespaces["premarker-ns"].commit_documents.len(), 1);
        // The sentinel can never be mistaken for an observed schema version.
        assert!(
            !PRE_MARKER_SOURCE_SCHEMA
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert_ne!(PRE_MARKER_SOURCE_SCHEMA, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn a_marker_less_scan_keeps_every_field_level_refusal() {
        // Field-level fail-closed is what replaced the marker gate, so it must
        // still fire on a marker-less index: an incomplete commit row refuses
        // rather than emitting a partial generation.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        fs::create_dir_all(&index_path).unwrap();
        let (schema, fields) = crate::index::build_schema();
        let index = tantivy::Index::create_in_dir(&index_path, schema).unwrap();
        crate::index::register_code_tokenizer(&index);
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        let mut doc = TantivyDocument::new();
        doc.add_text(fields.doc_type, "commit");
        doc.add_text(fields.repo_id, "premarker-ns");
        // No entity_id / commit_sha / chunk_hash: an incomplete row.
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        drop(writer);
        let error = scan_commit_documents(&index_path, HistoryScanLimitsV1::default()).unwrap_err();
        assert_eq!(error.code(), "error.history_generation_corrupt");
    }

    #[test]
    fn the_sentinel_keeps_pre_marker_generation_ids_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = marker_less_index_with_a_commit(&root);
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();

        let build = || {
            let scan = scan_commit_documents(&index_path, HistoryScanLimitsV1::default())
                .unwrap()
                .unwrap();
            let capture = scan.namespaces["premarker-ns"].clone();
            store
                .create_or_open(HistoryGenerationInputV1 {
                    namespace: namespace("premarker-ns"),
                    owner: HistoryGenerationOwnerV1::Owned {
                        repo_history_id: history_id("rh_00000000000000000000000000000001"),
                    },
                    commit_documents: capture.commit_documents,
                    vector_inputs: capture.vector_inputs,
                    truncated_message_count: capture.truncated_message_count,
                    source_schema_version: scan.schema_version.clone(),
                    source_schema_fingerprint_sha256: scan.schema_fingerprint_sha256.clone(),
                    source_index_fingerprint_sha256: scan.source_index_fingerprint_sha256.clone(),
                })
                .unwrap()
        };
        let first = build();
        let second = build();
        assert_eq!(first.id, second.id);
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(
            first.manifest.body.source_schema_version,
            PRE_MARKER_SOURCE_SCHEMA
        );
    }

    #[test]
    fn a_directory_holding_no_index_is_nothing_to_carry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // Empty and marker-less.
        let empty = root.join("empty-index");
        fs::create_dir_all(&empty).unwrap();
        assert!(
            scan_commit_documents(&empty, HistoryScanLimitsV1::default())
                .unwrap()
                .is_none()
        );

        // Non-empty but holding only a stray non-index file: the exact shape
        // that trips `reset_index_on_schema_mismatch` (marker absent, dir
        // non-empty) and therefore reaches the replacement guards.
        let stray = root.join("stray-index");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join("leftover.txt"), b"not an index").unwrap();
        assert!(
            scan_commit_documents(&stray, HistoryScanLimitsV1::default())
                .unwrap()
                .is_none()
        );

        // And an absent directory stays an empty scan.
        assert!(
            scan_commit_documents(&root.join("absent"), HistoryScanLimitsV1::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_marker_less_index_with_corrupt_metadata_stays_fail_closed() {
        // `meta.json` present means an index IS here, so a failure to open or
        // read it is corruption, not absence, and must not degrade to
        // `Ok(None)` - that would silently drop carried history.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = marker_less_index_with_a_commit(&root);
        fs::write(index_path.join(TANTIVY_META_FILE), b"{ truncated").unwrap();
        let error = scan_commit_documents(&index_path, HistoryScanLimitsV1::default())
            .expect_err("corrupt index metadata must not read as an empty scan");
        assert_eq!(error.code(), "error.history_generation_io");
    }

    #[test]
    fn a_pre_marker_prepared_manifest_always_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("index");
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let mut prepared = prepared_for(&record);
        prepared.source_schema_version = PRE_MARKER_SOURCE_SCHEMA.to_string();
        store.write_prepared_rebuild_manifest(prepared).unwrap();

        // Index dir intact (pre-drop world).
        fs::create_dir_all(&index_path).unwrap();
        assert!(matches!(
            store.classify_rebuild_recovery(&index_path).unwrap(),
            RepoHistoryRebuildRecoveryV1::ResumePrepared { .. }
        ));

        // Index dir gone (post-drop world). Same arm: the two are
        // indistinguishable without a marker, and resume converges in both.
        fs::remove_dir_all(&index_path).unwrap();
        assert!(matches!(
            store.classify_rebuild_recovery(&index_path).unwrap(),
            RepoHistoryRebuildRecoveryV1::ResumePrepared { .. }
        ));
    }

    #[test]
    fn rebuild_manifest_round_trips_and_pins_its_generations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let prepared = store
            .write_prepared_rebuild_manifest(prepared_for(&record))
            .unwrap();
        assert_eq!(prepared.state, RepoHistoryRebuildStateV1::Prepared);
        assert!(
            prepared
                .pinned_generation_ids()
                .contains(record.id.as_str())
        );
        let committed = store
            .commit_rebuild_manifest(RepoHistoryRebuildCommittedV1 {
                verified_lexical_view: "lexical-1".to_string(),
                verified_vector_view: "vector-1".to_string(),
                resulting_catalog_epoch: 5,
                vector_inventory: Vec::new(),
            })
            .unwrap();
        assert_eq!(committed.state, RepoHistoryRebuildStateV1::Committed);
        assert_eq!(committed.rebuild_id, prepared.rebuild_id);
        let reloaded = store.read_rebuild_manifest().unwrap().unwrap();
        assert_eq!(reloaded, committed);
        assert!(
            reloaded
                .pinned_generation_ids()
                .contains(record.id.as_str())
        );
    }

    #[test]
    fn rebuild_manifest_refuses_a_bucket_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let record = store
            .create_or_open(input(
                "ns1",
                vec![document("ns1", &"a".repeat(40), "first")],
            ))
            .unwrap();
        let mut prepared = prepared_for(&record);
        prepared.owned_generation_ids.clear();
        let error = store.write_prepared_rebuild_manifest(prepared).unwrap_err();
        assert_eq!(error.code(), "error.history_generation_corrupt");
    }
}
