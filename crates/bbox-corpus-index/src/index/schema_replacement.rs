//! The pre-replacement guard boundary and commit-document carryover
//! (Phase 3 milestone P3-E, plan section 9 item 2).
//!
//! # Why a callback and not a direct call
//!
//! Inventory-materialize-verify-replace needs the catalog transact, which
//! lives in `bbox-indexing`. This crate cannot depend on `bbox-indexing`, so
//! `reset_index_on_schema_mismatch` never calls the materializer. Instead the
//! open path takes an injected [`SchemaReplacementGuard`] and invokes it
//! BEFORE any destructive step; the guard composes the primitives that live
//! on both sides of the boundary (this crate owns generation scanning and
//! creation in `history_generations`, `bbox-indexing` owns the catalog
//! advancement).
//!
//! # Fail-closed
//!
//! An ABSENT guard refuses the reset. Before this milestone the drop was
//! unconditional for every caller; after it, no open path can reach
//! `remove_dir_all(index_path)` without an authorization. A guard that
//! returns an error aborts the reset and leaves the last-good lexical and
//! vector views selected, because nothing has been replaced yet.
//!
//! # The two production guards
//!
//! - catalog mode: the P3-D materializer orchestration, which drives every
//!   observed namespace to `Ready` and writes the prepared rebuild manifest;
//! - bridge mode: the commit spill lane in this module, which carries commit
//!   documents across the reset instead of dropping them and re-walking Git
//!   (a bridge project whose checkout is unavailable would otherwise lose its
//!   history at every schema bump).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use super::FieldHandles;
use super::history_generations::HistoryCommitDocumentV1;

/// Directory name of the bridge spill lane. A SIBLING of `index_path`, never
/// inside it: the reset removes `index_path` wholesale, so a spill written
/// inside it would be destroyed by the very step it exists to survive.
const COMMIT_SPILL_DIRNAME: &str = "commit-spill";
const COMMIT_SPILL_FILENAME: &str = "commit-spill.json";
const COMMIT_SPILL_VERSION_V1: u32 = 1;

/// WHY the index is being replaced.
///
/// **Adjudication Q-F, ratified: the marker contradiction.** Before Q-F the
/// only trigger was a marker mismatch, and execution proved that made
/// "Equality AND Completed" unreachable for the Phase 6 cut. The migration
/// refuses to capture a marker-mismatched index as `Corrupt`, so a recorded
/// Equality fingerprint requires the marker to MATCH; the replacement ran only
/// when it did NOT. One pinned binary means one marker value, so the marker
/// could never satisfy both at once and the two requirements were structurally
/// exclusive.
///
/// The resolution is a second, explicitly named cause rather than a loosened
/// predicate: the Phase 6 replacement is OPERATOR-TRIGGERED against an
/// UNCHANGED marker. Carrying the cause here keeps the two triggers
/// distinguishable everywhere downstream - in the guard's audit line, in the
/// drive state, and in the crash-recovery reasoning - instead of collapsing
/// them into one boolean that no reader can tell apart after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogIndexReplacementCause {
    /// The observed marker differs from the running `INDEX_SCHEMA_VERSION`, or
    /// a non-empty index carries no marker at all. The daemon-upgrade trigger,
    /// and the ONLY cause daemon startup can produce.
    SchemaMismatch,
    /// The Phase 6 offline `path-free-rebuild --apply`, after artifact
    /// authorization and the immediate D-036 Equality recapture both succeeded.
    /// `observed_schema_version` EQUALS `target_schema_version` here by
    /// construction: that equality is the precondition, not an anomaly.
    OperatorPathFreeRebuild,
}

/// What the open path knows about the replacement it is about to perform.
#[derive(Debug, Clone)]
pub struct SchemaReplacementRequest<'a> {
    pub index_path: &'a Path,
    pub projects_path: &'a Path,
    pub code_source_store_path: &'a Path,
    /// The marker read off the index that is about to be dropped. `None` when
    /// the index directory is non-empty and carries no marker at all, which is
    /// the "pre-marker index" arm of the mismatch trigger. Under
    /// [`CatalogIndexReplacementCause::OperatorPathFreeRebuild`] this is always
    /// `Some` and always equal to `target_schema_version`.
    pub observed_schema_version: Option<String>,
    pub target_schema_version: &'static str,
    /// Which trigger authorized this replacement (Q-F).
    pub cause: CatalogIndexReplacementCause,
}

/// What the open path is authorized to do when it reaches the replacement
/// boundary.
///
/// The caller decides this, because only the caller has classified rebuild
/// recovery: that classification reads the index's schema marker to locate
/// itself relative to the destructive drop, and opening the index rewrites
/// the marker, so it can only be done BEFORE the open. Q-F makes honoring it
/// at this boundary mandatory rather than advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CatalogReplacementIntentV1 {
    /// The ordinary open. A mismatched marker, or a non-empty index with no
    /// marker, replaces under [`CatalogIndexReplacementCause::SchemaMismatch`].
    /// Every caller that has no manifest evidence and no operator
    /// authorization uses this, daemon startup included.
    #[default]
    MismatchOnly,
    /// The Q-F operator force, reachable ONLY from the offline
    /// `path-free-rebuild --apply` after authorization. It requires the
    /// outgoing marker to EQUAL the running `INDEX_SCHEMA_VERSION`: a missing
    /// or mismatched marker means the index is not the predecessor the
    /// operator authorized against, and it refuses rather than replacing one.
    ForceSameSchema,
    /// A durable Prepared or Committed rebuild manifest survives past the
    /// destructive boundary, so the index on disk IS the replacement and its
    /// marker is WITHHELD by design.
    ///
    /// This flips exactly one arm: a marker-less index is no longer read as a
    /// pre-marker legacy index to drop. Without it, crash states (3) and (4)
    /// would re-enter the guard, mint a SECOND prepared manifest over a
    /// population the first one already pins, and in state (4) drop an index
    /// whose manifest is already `Committed`. A MISMATCHED marker still
    /// replaces normally: that is a real daemon upgrade, not an interrupted
    /// replacement, and suppressing it would make a legitimate schema bump
    /// unbootable.
    PreserveInterrupted,
}

/// What the open path did at the replacement boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReplacementOutcomeV1 {
    /// `Some` when THIS open performed the destructive replacement.
    pub performed: Option<CatalogIndexReplacementCause>,
    /// Whether the open deliberately did not publish the schema marker.
    ///
    /// The marker is the LAST thing published in a replacement, so its absence
    /// is the evidence a later recovery reads. Withholding it is therefore not
    /// an omission to tidy up: publishing it early erases the only signal that
    /// distinguishes "manifest committed, marker pending" from an ordinary
    /// steady-state boot.
    pub marker_withheld: bool,
}

/// A guard's authorization to proceed. Carrying the authority's own label
/// keeps the audit trail in the open log rather than requiring the reader to
/// infer which lane ran.
#[derive(Debug, Clone)]
pub struct SchemaReplacementAuthorization {
    pub authorized_by: String,
}

impl SchemaReplacementAuthorization {
    pub fn new(authorized_by: impl Into<String>) -> Self {
        Self {
            authorized_by: authorized_by.into(),
        }
    }
}

/// Injected pre-replacement guard. `Arc` rather than a bare `fn` because both
/// production guards capture state (the catalog store handle, the project
/// records snapshot).
pub type SchemaReplacementGuard = Arc<
    dyn Fn(&SchemaReplacementRequest<'_>) -> Result<SchemaReplacementAuthorization> + Send + Sync,
>;

/// Who a carried-over commit namespace belongs to at re-emission time.
///
/// `HistoryCommitDocumentV1` deliberately stores neither `project` nor
/// `file_path`: the first was a raw host canonical path and the second the
/// per-project `git:<project_id>` source key, and freezing either into an
/// immutable artifact would defeat the path-free cut. Both are re-derived
/// from this owner instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitDocumentOwnerV1 {
    /// `None` for a quarantined or unclaimed namespace: it has no owning
    /// project, therefore no `git:<project_id>` purge key. Such a document is
    /// reachable by entity id and by every non-path lane, and its generation
    /// (pinned by the rebuild manifest) is its durable owner.
    pub project_id: Option<String>,
    /// The value emitted as `project`: the identity's display name when the
    /// namespace has an owner, else the namespace itself. Never a host path.
    pub project_display: String,
}

impl CommitDocumentOwnerV1 {
    /// Owner for a namespace nothing claims. `project` falls back to the
    /// namespace so the field is never empty and never a path.
    pub fn unclaimed(namespace: &str) -> Self {
        Self {
            project_id: None,
            project_display: namespace.to_string(),
        }
    }
}

/// Rebuild one commit document from its generation/spill row plus the owner
/// resolved at re-emission time.
///
/// Every field the scan captured is written back byte-identically, so the
/// namespace/sha identity (`repo_id`, `commit_sha`, `entity_id`) and the
/// content hash are preserved exactly. That is what lets vectors ride through
/// untouched: commit entity ids and content hashes are stable across the
/// replacement.
pub fn build_commit_doc_from_row(
    row: &HistoryCommitDocumentV1,
    owner: &CommitDocumentOwnerV1,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, &row.doc_type);
    doc.add_text(f.chunk_kind, &row.chunk_kind);
    doc.add_text(f.entity_id, &row.entity_id);
    doc.add_text(f.content, &row.content);
    if !row.path_tokens.is_empty() {
        doc.add_text(f.path_tokens, &row.path_tokens);
    }
    doc.add_text(f.chunk_hash, &row.content_hash);
    doc.add_text(f.parser_version, &row.parser_version);
    doc.add_text(f.repo_id, &row.repo_id);
    doc.add_text(f.commit_sha, &row.commit_sha);
    doc.add_text(f.commit_author_name, &row.commit_author_name);
    doc.add_text(f.commit_author_email, &row.commit_author_email);
    doc.add_text(f.session_id, &row.session_id);
    doc.add_text(f.account, &row.account);
    doc.add_text(f.project, &owner.project_display);
    doc.add_text(f.role, &row.role);
    if let Some(project_id) = &owner.project_id {
        doc.add_text(f.project_id, project_id);
        doc.add_text(f.file_path, super::git_history::git_source_key(project_id));
    }
    doc.add_u64(f.byte_offset, row.byte_offset);
    doc.add_u64(f.is_subagent, row.is_subagent);
    doc
}

/// Re-emit a namespace's commit documents, delete-term-then-add so a partial
/// previous attempt cannot duplicate anything. This is what makes both the
/// catalog resume arm and the spill consumption idempotent.
// Sanctioned single-writer contexts only: the IndexWriterActor rebuild pass
// (catalog lane) and the one-time boot open before the runtime serves traffic
// (spill lane). Both hold the sole writer for the whole call.
#[allow(clippy::disallowed_methods)]
pub fn reemit_commit_documents(
    writer: &IndexWriter,
    f: FieldHandles,
    rows: &[HistoryCommitDocumentV1],
    owner: &CommitDocumentOwnerV1,
) -> Result<u64> {
    let mut emitted = 0u64;
    for row in rows {
        writer.delete_term(Term::from_field_text(f.entity_id, &row.entity_id));
        writer.add_document(build_commit_doc_from_row(row, owner, f))?;
        emitted += 1;
    }
    Ok(emitted)
}

// ---------------------------------------------------------------------------
// Bridge spill lane
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitSpillNamespaceV1 {
    pub namespace: String,
    pub owner: CommitDocumentOwnerV1,
    pub commit_documents: Vec<HistoryCommitDocumentV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitSpillFileV1 {
    pub version: u32,
    /// The marker of the index the spill was taken from. Recorded for
    /// diagnostics only: consumption is unconditional (see
    /// [`consume_commit_spill_if_present`]), because gating it on a version
    /// comparison would reintroduce exactly the one-shot trigger the crash
    /// lifecycle has to survive.
    pub source_schema_version: Option<String>,
    pub namespaces: Vec<CommitSpillNamespaceV1>,
}

impl CommitSpillFileV1 {
    pub fn new(
        source_schema_version: Option<String>,
        namespaces: Vec<CommitSpillNamespaceV1>,
    ) -> Self {
        Self {
            version: COMMIT_SPILL_VERSION_V1,
            source_schema_version,
            namespaces,
        }
    }

    pub fn commit_document_count(&self) -> u64 {
        self.namespaces
            .iter()
            .map(|entry| entry.commit_documents.len() as u64)
            .sum()
    }
}

/// The spill root for an index: `<family root>/commit-spill/`, a sibling of
/// `index_path` and of the history generations root.
pub fn commit_spill_root_for_index(index_path: &Path) -> Result<PathBuf> {
    if !strict_absolute_path(index_path) {
        anyhow::bail!("index path must be absolute and free of traversal");
    }
    let family_root = index_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("index path has no family root"))?;
    let root = family_root.join(COMMIT_SPILL_DIRNAME);
    if root.starts_with(index_path) {
        anyhow::bail!("commit spill root must be a sibling of the index, never inside it");
    }
    Ok(root)
}

fn strict_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Write and fsync the spill BEFORE the drop. Both the file and its directory
/// are synced: an unsynced directory entry can lose the whole file to a crash,
/// which would put us back in the history-loss window this lane closes.
// one-time boot path before the runtime serves traffic.
#[allow(clippy::disallowed_methods)]
pub fn write_commit_spill(index_path: &Path, spill: &CommitSpillFileV1) -> Result<()> {
    use std::io::Write as _;

    let root = commit_spill_root_for_index(index_path)?;
    std::fs::create_dir_all(&root).context("creating the commit spill root")?;
    let path = root.join(COMMIT_SPILL_FILENAME);
    let temporary = root.join(format!("{COMMIT_SPILL_FILENAME}.tmp"));
    let bytes = serde_json::to_vec(spill).context("encoding the commit spill")?;
    {
        let mut file = std::fs::File::create(&temporary).context("creating the commit spill")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, &path).context("publishing the commit spill")?;
    std::fs::File::open(&root)
        .and_then(|directory| directory.sync_all())
        .context("syncing the commit spill root")?;
    // The spill root itself may have been created just above; on a
    // non-journaled filesystem the parent's directory entry must also be
    // durable or a crash can lose the whole spill directory with the
    // synced file inside it.
    if let Some(parent) = root.parent() {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("syncing the commit spill root parent")?;
    }
    Ok(())
}

// one-time boot path before the runtime serves traffic.
#[allow(clippy::disallowed_methods)]
pub fn read_commit_spill(index_path: &Path) -> Result<Option<CommitSpillFileV1>> {
    let path = commit_spill_root_for_index(index_path)?.join(COMMIT_SPILL_FILENAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading the commit spill"),
    };
    let spill: CommitSpillFileV1 =
        serde_json::from_slice(&bytes).context("decoding the commit spill")?;
    if spill.version != COMMIT_SPILL_VERSION_V1 {
        anyhow::bail!("commit spill version {} is not supported", spill.version);
    }
    Ok(Some(spill))
}

// one-time boot path before the runtime serves traffic.
#[allow(clippy::disallowed_methods)]
pub fn delete_commit_spill(index_path: &Path) -> Result<()> {
    let path = commit_spill_root_for_index(index_path)?.join(COMMIT_SPILL_FILENAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("removing the consumed commit spill"),
    }
}

/// Consume a leftover spill, if one is present, at EVERY open.
///
/// Three properties are load-bearing and none of them is optional:
///
/// - consumption is NOT gated on the schema-mismatch trigger. That trigger
///   fires exactly once; a crash after the drop leaves no mismatch to detect
///   on the next open, so a mismatch-gated consumer would never run and the
///   carried population would be lost;
/// - the re-add is delete-term-then-add per commit entity id, so consuming a
///   spill that was already partly applied cannot duplicate a document;
/// - the spill file is deleted ONLY after the re-add commits. A crash between
///   the add and the commit therefore replays the whole spill next open
///   instead of losing it.
///
/// The caller must invoke this before any read view binds, mirroring the
/// ordering the P3-D manifest recovery pins for the catalog lane, so a reader
/// never observes the carried-over population as incomplete.
// one-time boot path before the runtime serves traffic: this runs inside
// `TranscriptIndex::open_or_create*`, before any reader binds, so it is the sole
// writer by construction.
#[allow(clippy::disallowed_methods)]
pub fn consume_commit_spill_if_present(
    index_path: &Path,
    index: &Index,
    f: FieldHandles,
) -> Result<u64> {
    let Some(spill) = read_commit_spill(index_path)? else {
        return Ok(0);
    };
    let expected = spill.commit_document_count();
    tracing::info!(
        namespaces = spill.namespaces.len(),
        commit_documents = expected,
        source_schema_version = spill.source_schema_version.as_deref().unwrap_or("<absent>"),
        "consuming a leftover commit spill before read views bind"
    );
    let writer: IndexWriter = index
        .writer(50_000_000)
        .context("opening a writer to consume the commit spill")?;
    let mut emitted = 0u64;
    for entry in &spill.namespaces {
        emitted += reemit_commit_documents(&writer, f, &entry.commit_documents, &entry.owner)?;
    }
    let mut writer = writer;
    writer.commit().context("committing the commit spill")?;
    // Only now: the population is durable in the index.
    delete_commit_spill(index_path)?;
    debug_assert_eq!(emitted, expected);
    Ok(emitted)
}

/// Group scanned commit rows into spill namespaces, resolving each namespace's
/// owner through `owners` (namespace -> owner) and falling back to
/// [`CommitDocumentOwnerV1::unclaimed`] for a namespace nothing claims.
pub fn spill_namespaces_from_rows(
    rows_by_namespace: BTreeMap<String, Vec<HistoryCommitDocumentV1>>,
    owners: &BTreeMap<String, CommitDocumentOwnerV1>,
) -> Vec<CommitSpillNamespaceV1> {
    rows_by_namespace
        .into_iter()
        .map(|(namespace, commit_documents)| CommitSpillNamespaceV1 {
            owner: owners
                .get(&namespace)
                .cloned()
                .unwrap_or_else(|| CommitDocumentOwnerV1::unclaimed(&namespace)),
            namespace,
            commit_documents,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(namespace: &str, sha: &str) -> HistoryCommitDocumentV1 {
        HistoryCommitDocumentV1 {
            entity_id: format!("commit:{namespace}:{sha}"),
            doc_type: "commit".into(),
            chunk_kind: "git_message".into(),
            repo_id: namespace.into(),
            commit_sha: sha.into(),
            content: format!("subject for {sha}"),
            content_hash: format!("hash-{sha}"),
            path_tokens: format!("subject for {sha}"),
            parser_version: "pv".into(),
            commit_author_name: "Author".into(),
            commit_author_email: "author@example.test".into(),
            session_id: String::new(),
            account: "git".into(),
            role: "commit".into(),
            byte_offset: 0,
            is_subagent: 0,
        }
    }

    #[test]
    fn spill_root_is_a_sibling_of_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("idx");
        let spill_root = commit_spill_root_for_index(&index_path).unwrap();
        assert_eq!(spill_root, root.join("commit-spill"));
        assert!(!spill_root.starts_with(&index_path));
    }

    #[test]
    fn spill_root_refuses_a_relative_index_path() {
        assert!(commit_spill_root_for_index(Path::new("idx")).is_err());
    }

    #[test]
    fn spill_round_trips_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index_path = root.join("idx");
        let spill = CommitSpillFileV1::new(
            Some("outgoing-schema".into()),
            vec![CommitSpillNamespaceV1 {
                namespace: "repo-a".into(),
                owner: CommitDocumentOwnerV1 {
                    project_id: Some("project-a".into()),
                    project_display: "alpha".into(),
                },
                commit_documents: vec![row("repo-a", "aaa"), row("repo-a", "bbb")],
            }],
        );
        write_commit_spill(&index_path, &spill).unwrap();
        assert_eq!(read_commit_spill(&index_path).unwrap().unwrap(), spill);
        assert_eq!(spill.commit_document_count(), 2);
        delete_commit_spill(&index_path).unwrap();
        assert!(read_commit_spill(&index_path).unwrap().is_none());
        // Idempotent: deleting an absent spill is not an error, which is what
        // lets the consumer run unconditionally at every open.
        delete_commit_spill(&index_path).unwrap();
    }

    #[test]
    fn unclaimed_namespace_owner_carries_the_namespace_and_no_project_id() {
        let owner = CommitDocumentOwnerV1::unclaimed("repo-z");
        assert_eq!(owner.project_id, None);
        assert_eq!(owner.project_display, "repo-z");
    }

    #[test]
    fn spill_namespaces_fall_back_to_unclaimed_owners() {
        let mut rows = BTreeMap::new();
        rows.insert("repo-a".to_string(), vec![row("repo-a", "aaa")]);
        rows.insert("repo-z".to_string(), vec![row("repo-z", "zzz")]);
        let mut owners = BTreeMap::new();
        owners.insert(
            "repo-a".to_string(),
            CommitDocumentOwnerV1 {
                project_id: Some("project-a".into()),
                project_display: "alpha".into(),
            },
        );
        let namespaces = spill_namespaces_from_rows(rows, &owners);
        assert_eq!(namespaces.len(), 2);
        assert_eq!(namespaces[0].owner.project_display, "alpha");
        assert_eq!(namespaces[1].owner.project_id, None);
        assert_eq!(namespaces[1].owner.project_display, "repo-z");
    }
}
