//! Pre-replacement repo-history materializer (Phase 3 milestone P3-D).
//!
//! Orchestration only. It composes primitives that already exist on both
//! sides of a crate boundary:
//!
//! - `bbox_corpus_index::index::history_generations` owns the index scan,
//!   the immutable generation format, its content-addressed identity, and
//!   the rebuild manifest;
//! - `crate::project_catalog_store::ProjectCatalogStore::transact` owns
//!   catalog advancement under epoch CAS.
//!
//! This module decides WHICH generations exist and proves them against the
//! Phase 1 evidence before any of it is allowed to authorize a destructive
//! index replacement. Nothing here is wired into the replacement boundary at
//! this milestone; the wiring is P3-E.
//!
//! # What the proof binds to
//!
//! For a `MigratedV1` catalog the observed per-namespace count and
//! commitment must equal the persisted
//! `LegacyCommitNamespaceInventoryAssetV1` row for that namespace
//! (Phase 3 plan section 4.2). A migrated store whose marker predates the
//! asset refuses with `history_inventory_missing`; a disagreement refuses
//! with `history_commitment_mismatch`. Either way the replacement stays
//! refused and the last-good lexical and vector views stay intact, because
//! this module never touches them.
//!
//! A `FreshV2` catalog has no Phase 1 evidence to prove against by
//! construction. A fresh store with no legacy residue produces no
//! generations at all and legally leaves every record `NotBuilt`.
//!
//! # Classification
//!
//! A namespace observed in the index is classified against the PINNED
//! catalog snapshot:
//!
//! - owned when it is a repo-history record's primary or compatibility
//!   namespace (`validate_catalog` makes that assignment globally unique, so
//!   at most one record can claim it);
//! - ambiguous when an `AmbiguousNamespaceRecord` quarantines it;
//! - unclaimed otherwise, per plan section 4.4. Post-migration drift,
//!   retirement, and a fresh v2 store's legacy residue all land here. An
//!   unclaimed namespace CANNOT be represented in the catalog at all:
//!   `validate_catalog` requires an ambiguous record to name at least two
//!   existing candidate histories, which an unclaimed namespace by
//!   definition has none of. Its generation is therefore owned solely by the
//!   rebuild manifest and never by a catalog mutation.
//!
//! # Concurrency
//!
//! Generations are prepared entirely off-lock (plan section 12): the scan is
//! a read-only tantivy open and the generation writes are to a fresh
//! content-addressed directory. Only the final advancement takes the catalog
//! mutation lock, through the ordinary `transact` CAS, exactly once for the
//! whole proved-and-ambiguous set.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bbox_corpus_core::project_catalog::{
    AmbiguousNamespaceRecord, CatalogOriginV2, CatalogSnapshotV2, CommitNamespace,
    RepoHistoryGenerationId, RepoHistoryId, RepoHistoryMaterialization,
    RepoHistoryQuarantineGenerationId, RepoHistoryQuarantineMaterialization,
};
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationError, HistoryGenerationIdV1, HistoryGenerationInputV1, HistoryGenerationIo,
    HistoryGenerationOwnerV1, HistoryGenerationRecordV1, HistoryGenerationStore,
    HistoryIndexScanV1, HistoryNamespaceCaptureV1, HistoryScanLimitsV1, RealHistoryGenerationIo,
    RepoHistoryRebuildDispositionV1, RepoHistoryRebuildManifestV1, RepoHistoryRebuildNamespaceV1,
    RepoHistoryRebuildPreparedV1, RepoHistoryRebuildRecoveryV1, scan_commit_documents,
};

use crate::project_catalog_migration::{
    LegacyCommitNamespaceInventoryAssetV1, load_legacy_commit_namespace_inventory_asset,
};
use crate::project_catalog_store::ProjectCatalogStore;

/// Typed refusals this milestone adds, plus the pass-throughs it forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMaterializerError {
    code: String,
    message: String,
}

impl HistoryMaterializerError {
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

    fn inventory_missing(message: impl Into<String>) -> Self {
        Self::new("error.history_inventory_missing", message)
    }

    fn commitment_mismatch(message: impl Into<String>) -> Self {
        Self::new("error.history_commitment_mismatch", message)
    }
}

impl std::fmt::Display for HistoryMaterializerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HistoryMaterializerError {}

impl From<HistoryGenerationError> for HistoryMaterializerError {
    fn from(error: HistoryGenerationError) -> Self {
        Self::new(error.code(), error.message())
    }
}

pub type HistoryMaterializerResult<T> = Result<T, HistoryMaterializerError>;

/// How one observed namespace was classified against the pinned catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceClassificationV1 {
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

impl NamespaceClassificationV1 {
    fn into_owner(self) -> HistoryGenerationOwnerV1 {
        match self {
            Self::Owned { repo_history_id } => HistoryGenerationOwnerV1::Owned { repo_history_id },
            Self::Ambiguous {
                candidate_repo_history_ids,
            } => HistoryGenerationOwnerV1::Ambiguous {
                candidate_repo_history_ids,
            },
            Self::Unclaimed {
                inventory_diagnostic,
            } => HistoryGenerationOwnerV1::Unclaimed {
                inventory_diagnostic,
            },
        }
    }

    fn disposition(&self) -> RepoHistoryRebuildDispositionV1 {
        match self {
            Self::Owned { .. } => RepoHistoryRebuildDispositionV1::Owned,
            Self::Ambiguous { .. } => RepoHistoryRebuildDispositionV1::Ambiguous,
            Self::Unclaimed { .. } => RepoHistoryRebuildDispositionV1::Unclaimed,
        }
    }
}

/// Classify one observed namespace against a pinned catalog snapshot.
///
/// Shape is never used to infer kind. Only `local_` is a reliable prefix
/// marker in this vocabulary, and it is not consulted here: ownership comes
/// from the catalog's own namespace assignment and nothing else.
pub fn classify_namespace(
    catalog: &CatalogSnapshotV2,
    namespace: &CommitNamespace,
) -> NamespaceClassificationV1 {
    for (repo_history_id, record) in &catalog.repo_histories {
        if &record.primary_namespace == namespace
            || record.compatibility_namespaces.contains(namespace)
        {
            return NamespaceClassificationV1::Owned {
                repo_history_id: repo_history_id.clone(),
            };
        }
    }
    if let Some(record) = catalog.ambiguous_namespaces.get(namespace) {
        return NamespaceClassificationV1::Ambiguous {
            candidate_repo_history_ids: record.candidate_repo_history_ids.clone(),
        };
    }
    // The diagnostic is part of the generation id preimage, so it must be a
    // function of CONTENT only. It deliberately carries no catalog epoch,
    // timestamp, or host detail: any of those would remint a new id for
    // identical history on every pass and break the idempotence the whole
    // content-addressed scheme rests on.
    NamespaceClassificationV1::Unclaimed {
        inventory_diagnostic: format!(
            "namespace {namespace} has no owning repo-history record and no \
             ambiguous-namespace record"
        ),
    }
}

/// One materialized namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedNamespaceV1 {
    pub namespace: CommitNamespace,
    pub classification: NamespaceClassificationV1,
    pub generation: HistoryGenerationRecordV1,
}

/// Result of a materialization pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMaterializationOutcomeV1 {
    pub namespaces: Vec<MaterializedNamespaceV1>,
    /// `None` when the pass advanced nothing (no proved or ambiguous
    /// namespace, or every record was already `Ready` at the same id).
    pub catalog_epoch_after: Option<u64>,
    /// Namespaces carrying at least one truncated commit message.
    ///
    /// Re-emitting those namespaces produces vector keys whose content hash
    /// is over the truncated text, while the legacy key's hash was over the
    /// raw message. The legacy key is therefore superseded rather than
    /// reproduced, and this set is how a caller observes that instead of
    /// discovering it as silent vector churn.
    pub namespaces_with_truncated_messages: BTreeSet<CommitNamespace>,
}

impl HistoryMaterializationOutcomeV1 {
    pub fn generation_ids(&self) -> BTreeSet<String> {
        self.namespaces
            .iter()
            .map(|entry| entry.generation.id.as_str().to_string())
            .collect()
    }
}

/// Inputs to one materialization pass.
#[derive(Debug, Clone)]
pub struct HistoryMaterializerRequestV1 {
    /// The legacy tantivy index to stream commit documents from.
    pub index_path: PathBuf,
    /// The catalog store's `projects.json` path, used to locate the
    /// migration asset root that holds the Phase 1 namespace inventory.
    pub projects_path: PathBuf,
    pub scan_limits: HistoryScanLimitsV1,
}

/// Materialize every observed namespace and advance the catalog.
///
/// Ordering is deliberate and load-bearing:
///
/// 1. scan the index off-lock;
/// 2. pin ONE catalog snapshot and classify against it;
/// 3. prove every namespace against the persisted Phase 1 evidence, and
///    refuse the whole pass on the first disagreement (partial
///    materialization would leave a catalog naming a generation whose
///    siblings were never proved);
/// 4. create and verify every generation;
/// 5. advance `NotBuilt -> Ready` for the proved and ambiguous set in ONE
///    transact against the epoch pinned in step 2.
///
/// A concurrent catalog mutation between steps 2 and 5 makes step 5 fail the
/// epoch CAS; the generations already created stay on disk, are
/// content-addressed, and are re-derived identically on the retry.
pub fn materialize_history_generations(
    store: &ProjectCatalogStore,
    request: &HistoryMaterializerRequestV1,
) -> HistoryMaterializerResult<HistoryMaterializationOutcomeV1> {
    materialize_history_generations_with_io(store, request, Arc::new(RealHistoryGenerationIo))
}

pub fn materialize_history_generations_with_io(
    store: &ProjectCatalogStore,
    request: &HistoryMaterializerRequestV1,
    io: Arc<dyn HistoryGenerationIo>,
) -> HistoryMaterializerResult<HistoryMaterializationOutcomeV1> {
    let scan = scan_commit_documents(&request.index_path, request.scan_limits)?;
    let state = store
        .snapshot()
        .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
    let catalog = state.catalog();
    let pinned_epoch = state.epoch();

    let Some(scan) = scan else {
        // No index at all, or no schema marker: a fresh v2 store with no
        // legacy residue. Producing no generations here is the legal
        // outcome, not a refusal, and every record stays `NotBuilt`.
        return Ok(HistoryMaterializationOutcomeV1 {
            namespaces: Vec::new(),
            catalog_epoch_after: None,
            namespaces_with_truncated_messages: BTreeSet::new(),
        });
    };

    let asset = load_inventory_asset(catalog, &request.projects_path)?;
    let generation_store =
        HistoryGenerationStore::open_for_index_with_io(&request.index_path, io.clone())?;

    let mut namespaces = Vec::new();
    let mut truncated = BTreeSet::new();
    for capture in scan.namespaces.values() {
        let namespace = CommitNamespace::parse(capture.namespace.clone()).map_err(|error| {
            HistoryMaterializerError::new(
                "error.history_commitment_mismatch",
                format!(
                    "index carries an unparseable commit namespace {}: {error}",
                    capture.namespace
                ),
            )
        })?;
        if let Some(asset) = asset.as_ref() {
            prove_against_inventory(asset, &namespace, capture)?;
        }
        if capture.truncated_message_count > 0 {
            truncated.insert(namespace.clone());
        }
        let classification = classify_namespace(catalog, &namespace);
        let generation = generation_store.create_or_open(HistoryGenerationInputV1 {
            namespace: namespace.clone(),
            owner: classification.clone().into_owner(),
            commit_documents: capture.commit_documents.clone(),
            vector_inputs: capture.vector_inputs.clone(),
            truncated_message_count: capture.truncated_message_count,
            source_schema_version: scan.schema_version.clone(),
            source_schema_fingerprint_sha256: scan.schema_fingerprint_sha256.clone(),
            source_index_fingerprint_sha256: scan.source_index_fingerprint_sha256.clone(),
        })?;
        namespaces.push(MaterializedNamespaceV1 {
            namespace,
            classification,
            generation,
        });
    }

    let catalog_epoch_after = advance_catalog_materialization(store, pinned_epoch, &namespaces)?;
    Ok(HistoryMaterializationOutcomeV1 {
        namespaces,
        catalog_epoch_after,
        namespaces_with_truncated_messages: truncated,
    })
}

/// Load the Phase 1 namespace-inventory asset when the catalog demands one.
///
/// Only a `MigratedV1` catalog has Phase 1 evidence, and for such a catalog
/// the asset is mandatory: a migrated store whose marker predates the asset
/// refuses rather than materializing unproved legacy history.
fn load_inventory_asset(
    catalog: &CatalogSnapshotV2,
    projects_path: &Path,
) -> HistoryMaterializerResult<Option<LegacyCommitNamespaceInventoryAssetV1>> {
    let CatalogOriginV2::MigratedV1 { transaction_id } = &catalog.origin else {
        return Ok(None);
    };
    let asset = load_legacy_commit_namespace_inventory_asset(projects_path, transaction_id)
        .map_err(|error| HistoryMaterializerError::new(error.code, error.message.clone()))?;
    asset.map(Some).ok_or_else(|| {
        HistoryMaterializerError::inventory_missing(format!(
            "migrated catalog {transaction_id} has no persisted legacy commit-namespace \
             inventory asset; legacy history cannot be proved"
        ))
    })
}

/// Prove one observed namespace against its persisted Phase 1 row.
///
/// Three arms, in order:
///
/// 1. no row for the observed namespace: the recorded evidence and the live
///    index disagree about which namespaces exist, which is a commitment
///    mismatch, not a missing asset;
/// 2. count or document-set commitment disagreement:
///    `history_commitment_mismatch`. This arm is exact, because
///    `hash_commit_rows` is the very function Phase 1 committed with;
/// 3. vector-side completeness, as a COVERAGE check
///    (`vector_input_count >= vector_key_count`) rather than an equality.
///    Vector enqueue is asynchronous, so the vector store legitimately lags
///    the index, but it can never legitimately hold keys for commit
///    documents the generation does not carry.
///
/// The vector-side COMMITMENT hash is deliberately never recomputed here,
/// and no caller may add that comparison. Its preimage is
/// `(route, entity_ref, content_hash)` where `route` is the host-configured
/// embedding partition name and `content_hash` is over the RAW commit
/// message. The index carries neither: it stores no route, and above the
/// 16 KiB cap it stores only the truncated message. A recomputation would
/// therefore have to fabricate both, and would refuse correct data.
fn prove_against_inventory(
    asset: &LegacyCommitNamespaceInventoryAssetV1,
    namespace: &CommitNamespace,
    capture: &HistoryNamespaceCaptureV1,
) -> HistoryMaterializerResult<()> {
    let row = asset
        .rows
        .iter()
        .find(|row| &row.namespace == namespace)
        .ok_or_else(|| {
            HistoryMaterializerError::commitment_mismatch(format!(
                "namespace {namespace} is present in the index but absent from the recorded \
                 legacy commit-namespace inventory"
            ))
        })?;
    let observed_count = capture.commit_documents.len() as u64;
    if observed_count != row.commit_document_count {
        return Err(HistoryMaterializerError::commitment_mismatch(format!(
            "namespace {namespace} has {observed_count} commit documents but the recorded \
             inventory says {}",
            row.commit_document_count
        )));
    }
    if capture.commit_document_commitment_sha256 != row.commit_document_set_sha256.as_str() {
        return Err(HistoryMaterializerError::commitment_mismatch(format!(
            "namespace {namespace} commit-document commitment disagrees with the recorded \
             inventory"
        )));
    }
    let vector_inputs = capture.vector_inputs.len() as u64;
    if vector_inputs < row.vector_key_count {
        return Err(HistoryMaterializerError::commitment_mismatch(format!(
            "namespace {namespace} would carry {vector_inputs} vector inputs but the recorded \
             inventory holds {} vector keys",
            row.vector_key_count
        )));
    }
    Ok(())
}

/// Advance `NotBuilt -> Ready` for every proved and ambiguous namespace in
/// one regular catalog transact.
///
/// Unclaimed namespaces are deliberately absent: `validate_catalog` cannot
/// represent them (an ambiguous record must name at least two existing
/// candidates), so their only durable owner is the rebuild manifest.
///
/// An already-`Ready` record naming the SAME content-addressed id is a
/// no-op, which is what makes a re-run idempotent. An already-`Ready` record
/// naming a DIFFERENT id means the recorded materialization and the observed
/// index disagree about the namespace's content, so it refuses with
/// `history_commitment_mismatch` rather than silently overwriting durable
/// state a rebuild manifest may already pin.
fn advance_catalog_materialization(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    namespaces: &[MaterializedNamespaceV1],
) -> HistoryMaterializerResult<Option<u64>> {
    let mut owned: BTreeMap<RepoHistoryId, RepoHistoryGenerationId> = BTreeMap::new();
    let mut ambiguous: BTreeMap<CommitNamespace, RepoHistoryQuarantineGenerationId> =
        BTreeMap::new();
    for entry in namespaces {
        match &entry.classification {
            NamespaceClassificationV1::Owned { repo_history_id } => {
                let Some(id) = entry.generation.id.owned() else {
                    return Err(HistoryMaterializerError::commitment_mismatch(format!(
                        "namespace {} is owned but produced a quarantine generation id",
                        entry.namespace
                    )));
                };
                if let Some(existing) = owned.insert(repo_history_id.clone(), id.clone())
                    && &existing != id
                {
                    // Unreachable while `validate_catalog` keeps namespace
                    // assignment globally unique per record; a record with
                    // two observed namespaces would otherwise silently keep
                    // whichever one sorted last.
                    return Err(HistoryMaterializerError::commitment_mismatch(format!(
                        "repo history {repo_history_id} would be advanced to two different \
                         generations in one pass"
                    )));
                }
            }
            NamespaceClassificationV1::Ambiguous { .. } => {
                let Some(id) = entry.generation.id.quarantine() else {
                    return Err(HistoryMaterializerError::commitment_mismatch(format!(
                        "namespace {} is ambiguous but produced an owned generation id",
                        entry.namespace
                    )));
                };
                ambiguous.insert(entry.namespace.clone(), id.clone());
            }
            NamespaceClassificationV1::Unclaimed { .. } => {}
        }
    }
    if owned.is_empty() && ambiguous.is_empty() {
        return Ok(None);
    }

    let mut changed = false;
    {
        let state = store
            .snapshot()
            .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
        let catalog = state.catalog();
        for (repo_history_id, generation_id) in &owned {
            let record = catalog.repo_histories.get(repo_history_id).ok_or_else(|| {
                HistoryMaterializerError::commitment_mismatch(format!(
                    "repo history {repo_history_id} vanished between classification and advance"
                ))
            })?;
            if !materialization_is_current(&record.materialization, generation_id)? {
                changed = true;
            }
        }
        for (namespace, generation_id) in &ambiguous {
            let record = catalog.ambiguous_namespaces.get(namespace).ok_or_else(|| {
                HistoryMaterializerError::commitment_mismatch(format!(
                    "ambiguous namespace {namespace} vanished between classification and advance"
                ))
            })?;
            if !quarantine_materialization_is_current(&record.materialization, generation_id)? {
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(None);
    }

    let commit = store
        .transact(expected_epoch, |catalog, _attachments| {
            for (repo_history_id, generation_id) in &owned {
                let record = catalog
                    .repo_histories
                    .get_mut(repo_history_id)
                    .expect("classified repo history exists in the pinned catalog");
                record.materialization = RepoHistoryMaterialization::Ready {
                    generation_id: generation_id.clone(),
                };
            }
            for (namespace, generation_id) in &ambiguous {
                let record: &mut AmbiguousNamespaceRecord = catalog
                    .ambiguous_namespaces
                    .get_mut(namespace)
                    .expect("classified ambiguous namespace exists in the pinned catalog");
                record.materialization = RepoHistoryQuarantineMaterialization::Ready {
                    generation_id: generation_id.clone(),
                };
            }
            Ok(())
        })
        .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
    Ok(Some(commit.epoch))
}

fn materialization_is_current(
    materialization: &RepoHistoryMaterialization,
    expected: &RepoHistoryGenerationId,
) -> HistoryMaterializerResult<bool> {
    match materialization {
        RepoHistoryMaterialization::NotBuilt => Ok(false),
        RepoHistoryMaterialization::Ready { generation_id } if generation_id == expected => {
            Ok(true)
        }
        RepoHistoryMaterialization::Ready { generation_id } => {
            Err(HistoryMaterializerError::commitment_mismatch(format!(
                "repo history is already materialized at {generation_id} but the index \
                 re-derives {expected}"
            )))
        }
    }
}

fn quarantine_materialization_is_current(
    materialization: &RepoHistoryQuarantineMaterialization,
    expected: &RepoHistoryQuarantineGenerationId,
) -> HistoryMaterializerResult<bool> {
    match materialization {
        RepoHistoryQuarantineMaterialization::NotBuilt => Ok(false),
        RepoHistoryQuarantineMaterialization::Ready { generation_id }
            if generation_id == expected =>
        {
            Ok(true)
        }
        RepoHistoryQuarantineMaterialization::Ready { generation_id } => {
            Err(HistoryMaterializerError::commitment_mismatch(format!(
                "ambiguous namespace is already materialized at {generation_id} but the index \
                 re-derives {expected}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Rebuild manifest
// ---------------------------------------------------------------------------

/// Build the prepared manifest that authorizes a destructive replacement.
///
/// It binds the source index fingerprint and schema, the COMPLETE observed
/// namespace inventory (owned, ambiguous, and unclaimed alike), the catalog
/// epoch the classification was pinned at, every generation id in its
/// disposition bucket, and the planned target lexical and vector generation
/// labels.
pub fn prepare_rebuild_manifest(
    scan: &HistoryIndexScanV1,
    outcome: &HistoryMaterializationOutcomeV1,
    catalog_epoch: u64,
    planned_lexical_generation_label: impl Into<String>,
    planned_vector_generation_label: impl Into<String>,
) -> HistoryMaterializerResult<RepoHistoryRebuildPreparedV1> {
    // "Complete namespace inventory" is the manifest's load-bearing claim:
    // an unclaimed generation has no other durable owner, so a manifest that
    // silently omitted one would leave it unpinned and sweepable. Refuse a
    // scan and outcome that do not describe the same namespace set rather
    // than emitting a manifest that looks complete.
    let scanned = scan.namespaces.keys().cloned().collect::<BTreeSet<_>>();
    let materialized = outcome
        .namespaces
        .iter()
        .map(|entry| entry.namespace.as_str().to_string())
        .collect::<BTreeSet<_>>();
    if scanned != materialized {
        return Err(HistoryMaterializerError::commitment_mismatch(
            "rebuild manifest inputs describe different namespace sets",
        ));
    }
    let mut namespace_inventory = Vec::new();
    let mut owned_generation_ids = BTreeSet::new();
    let mut ambiguous_generation_ids = BTreeSet::new();
    let mut unclaimed_generation_ids = BTreeSet::new();
    for entry in &outcome.namespaces {
        let disposition = entry.classification.disposition();
        let id = entry.generation.id.as_str().to_string();
        match disposition {
            RepoHistoryRebuildDispositionV1::Owned => {
                owned_generation_ids.insert(id.clone());
            }
            RepoHistoryRebuildDispositionV1::Ambiguous => {
                ambiguous_generation_ids.insert(id.clone());
            }
            RepoHistoryRebuildDispositionV1::Unclaimed => {
                unclaimed_generation_ids.insert(id.clone());
            }
        }
        namespace_inventory.push(RepoHistoryRebuildNamespaceV1 {
            namespace: entry.namespace.clone(),
            generation_id: id,
            commit_document_count: entry.generation.manifest.body.commit_document_count,
            commit_document_commitment_sha256: entry
                .generation
                .manifest
                .body
                .commit_document_commitment_sha256
                .clone(),
            disposition,
        });
    }
    Ok(RepoHistoryRebuildPreparedV1 {
        source_index_fingerprint_sha256: scan.source_index_fingerprint_sha256.clone(),
        source_schema_version: scan.schema_version.clone(),
        namespace_inventory,
        catalog_epoch,
        owned_generation_ids,
        ambiguous_generation_ids,
        unclaimed_generation_ids,
        planned_lexical_generation_label: planned_lexical_generation_label.into(),
        planned_vector_generation_label: planned_vector_generation_label.into(),
    })
}

// ---------------------------------------------------------------------------
// GC roots
// ---------------------------------------------------------------------------

/// Every history generation that is a GC root, exactly as governing
/// section 16 lists them: those named by a catalog record (a `Ready`
/// repo-history or ambiguous-namespace materialization) and those named by a
/// prepared or committed `RepoHistoryRebuildManifestV1`.
///
/// The derived reference-manifest acceleration index that section 16 also
/// describes is deliberately NOT built here; it is deferred to P3-F, where
/// overlay references exist for it to accelerate. Until then this function
/// is the authority and it recomputes from durable inputs on every call.
///
/// Note the generations root is a sibling of `index_path` and of the edge
/// sidecar's `edges` directory, so the existing background storage sweep
/// (which plans only over edge-sidecar artifacts) can never reach a history
/// generation by accident. Nothing may sweep this root except a caller
/// holding a root set from this function.
pub fn history_generation_gc_roots(
    catalog: &CatalogSnapshotV2,
    manifests: &[RepoHistoryRebuildManifestV1],
) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    for record in catalog.repo_histories.values() {
        if let RepoHistoryMaterialization::Ready { generation_id } = &record.materialization {
            roots.insert(generation_id.as_str().to_string());
        }
    }
    for record in catalog.ambiguous_namespaces.values() {
        if let RepoHistoryQuarantineMaterialization::Ready { generation_id } =
            &record.materialization
        {
            roots.insert(generation_id.as_str().to_string());
        }
    }
    for manifest in manifests {
        roots.extend(manifest.pinned_generation_ids());
    }
    roots
}

/// Generations on disk that no root names. Sweeping them is the caller's
/// choice; `HistoryGenerationStore::remove_unreferenced` re-checks the root
/// set and refuses with `history_generation_referenced` regardless.
pub fn plan_history_generation_gc(
    store: &HistoryGenerationStore,
    roots: &BTreeSet<String>,
) -> HistoryMaterializerResult<Vec<HistoryGenerationIdV1>> {
    Ok(store
        .list()?
        .into_iter()
        .filter(|id| !roots.contains(id.as_str()))
        .collect())
}

/// Classify what a restart must do about an observed rebuild manifest.
///
/// ORDERING CONTRACT: recovery runs before any read view binds, so a resume
/// arm never races a reader against a half-replaced index. P3-D owns the
/// classifier and its proof; the call at daemon open is P3-E.
pub fn classify_rebuild_recovery(
    store: &HistoryGenerationStore,
    index_path: &Path,
) -> HistoryMaterializerResult<RepoHistoryRebuildRecoveryV1> {
    Ok(store.classify_rebuild_recovery(index_path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::project_catalog_inventory::{
        LegacyCommitNamespaceAttributionV1, LegacyCommitNamespaceInventoryV1, Sha256ValueV1,
    };
    use bbox_corpus_core::project_catalog::{
        AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, CatalogSnapshotV2,
        ProjectCatalogTransactionId, RepoHistoryAuthority, RepoHistoryRecord,
    };
    use bbox_corpus_index::index::history_generations::{
        HistoryCommitDocumentV1, HistoryVectorInputV1,
    };

    fn namespace(value: &str) -> CommitNamespace {
        CommitNamespace::parse(value).unwrap()
    }

    fn history(seed: u8) -> RepoHistoryId {
        let mut hex = format!("{seed:02x}");
        while hex.len() < 32 {
            hex.push('0');
        }
        RepoHistoryId::parse(format!("rh_{hex}")).unwrap()
    }

    fn catalog_with(
        primary: &str,
        compatibility: &[&str],
        ambiguous: Option<&str>,
    ) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let id = history(1);
        catalog.repo_histories.insert(
            id.clone(),
            RepoHistoryRecord {
                repo_history_id: id.clone(),
                authority: RepoHistoryAuthority::LegacyNamespace(namespace(primary)),
                primary_namespace: namespace(primary),
                compatibility_namespaces: compatibility
                    .iter()
                    .map(|value| namespace(value))
                    .collect(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        let second = history(2);
        catalog.repo_histories.insert(
            second.clone(),
            RepoHistoryRecord {
                repo_history_id: second.clone(),
                authority: RepoHistoryAuthority::LegacyNamespace(namespace("second-primary")),
                primary_namespace: namespace("second-primary"),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        if let Some(value) = ambiguous {
            catalog.ambiguous_namespaces.insert(
                namespace(value),
                AmbiguousNamespaceRecord {
                    namespace: namespace(value),
                    candidate_repo_history_ids: [id, second].into_iter().collect(),
                    status: AmbiguousNamespaceStatus::Quarantined,
                    materialization: RepoHistoryQuarantineMaterialization::NotBuilt,
                },
            );
        }
        catalog.validate().unwrap();
        catalog
    }

    #[test]
    fn a_compatibility_namespace_is_owned_by_its_record() {
        let catalog = catalog_with("primary-ns", &["compat-ns"], None);
        assert_eq!(
            classify_namespace(&catalog, &namespace("compat-ns")),
            NamespaceClassificationV1::Owned {
                repo_history_id: history(1)
            }
        );
    }

    #[test]
    fn an_ambiguous_namespace_beats_the_unclaimed_fallback() {
        let catalog = catalog_with("primary-ns", &[], Some("quarantined-ns"));
        assert!(matches!(
            classify_namespace(&catalog, &namespace("quarantined-ns")),
            NamespaceClassificationV1::Ambiguous { .. }
        ));
        assert!(matches!(
            classify_namespace(&catalog, &namespace("drifted-ns")),
            NamespaceClassificationV1::Unclaimed { .. }
        ));
    }

    fn capture(
        ns: &str,
        documents: usize,
        truncated_message_count: u64,
    ) -> HistoryNamespaceCaptureV1 {
        let commit_documents = (0..documents)
            .map(|index| {
                let sha = format!("{index:040x}");
                HistoryCommitDocumentV1 {
                    entity_id: format!("commit:{ns}:{sha}"),
                    doc_type: "commit".to_string(),
                    chunk_kind: "git_message".to_string(),
                    repo_id: ns.to_string(),
                    commit_sha: sha,
                    content: format!("message {index}"),
                    content_hash: format!("{index:064x}"),
                    path_tokens: String::new(),
                    parser_version: "p".to_string(),
                    commit_author_name: String::new(),
                    commit_author_email: String::new(),
                    session_id: String::new(),
                    account: "git".to_string(),
                    role: "commit".to_string(),
                    byte_offset: 0,
                    is_subagent: 0,
                }
            })
            .collect::<Vec<_>>();
        let vector_inputs = commit_documents
            .iter()
            .map(|document| HistoryVectorInputV1 {
                entity_id: document.entity_id.clone(),
                content_hash: document.content_hash.clone(),
                message: document.content.clone(),
            })
            .collect();
        let commitment = bbox_corpus_index::index::migration_inventory::hash_commit_rows(
            &commit_documents
                .iter()
                .map(
                    |document| bbox_corpus_index::index::migration_inventory::CommitRowV1 {
                        namespace: document.repo_id.clone(),
                        entity_ref: document.entity_id.clone(),
                        commit_sha: document.commit_sha.clone(),
                        content_hash: document.content_hash.clone(),
                    },
                )
                .collect::<Vec<_>>(),
        );
        HistoryNamespaceCaptureV1 {
            namespace: ns.to_string(),
            commit_documents,
            vector_inputs,
            truncated_message_count,
            commit_document_commitment_sha256: commitment,
        }
    }

    fn asset_for(
        capture: &HistoryNamespaceCaptureV1,
        vector_key_count: u64,
    ) -> LegacyCommitNamespaceInventoryAssetV1 {
        LegacyCommitNamespaceInventoryAssetV1 {
            version: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            source_index_fingerprint: Sha256ValueV1::digest(b"source"),
            rows: vec![LegacyCommitNamespaceInventoryV1 {
                observation_id: "obs".to_string(),
                namespace: namespace(&capture.namespace),
                commit_document_count: capture.commit_documents.len() as u64,
                commit_document_set_sha256: Sha256ValueV1::parse(
                    capture.commit_document_commitment_sha256.clone(),
                )
                .unwrap(),
                vector_key_count,
                vector_key_set_sha256: Sha256ValueV1::digest(b"vector"),
                attribution: LegacyCommitNamespaceAttributionV1::Unclaimed,
            }],
        }
    }

    #[test]
    fn a_matching_namespace_proves() {
        let capture = capture("ns", 3, 0);
        let asset = asset_for(&capture, 3);
        prove_against_inventory(&asset, &namespace("ns"), &capture).unwrap();
    }

    #[test]
    fn vector_side_completeness_refuses_a_short_input_set() {
        let capture = capture("ns", 3, 0);
        // The vector store holds keys for four commits; a generation that
        // carries only three would silently drop one entity's embedding
        // input across the replacement.
        let asset = asset_for(&capture, 4);
        let error = prove_against_inventory(&asset, &namespace("ns"), &capture).unwrap_err();
        assert_eq!(error.code(), "error.history_commitment_mismatch");
    }

    #[test]
    fn vector_side_completeness_accepts_a_lagging_vector_store() {
        // Vector enqueue is asynchronous, so fewer recorded keys than commit
        // documents is legitimate and must not refuse.
        let capture = capture("ns", 3, 0);
        let asset = asset_for(&capture, 1);
        prove_against_inventory(&asset, &namespace("ns"), &capture).unwrap();
    }

    #[test]
    fn truncated_messages_do_not_change_the_proof_arms() {
        // The raw-vs-truncated hash divergence is reported, never proved
        // away: the count arms still hold and nothing compares hashes.
        let capture = capture("ns", 2, 2);
        let asset = asset_for(&capture, 2);
        prove_against_inventory(&asset, &namespace("ns"), &capture).unwrap();
    }

    #[test]
    fn a_namespace_absent_from_the_asset_refuses_as_a_mismatch() {
        let capture = capture("ns", 1, 0);
        let asset = asset_for(&capture, 1);
        let error = prove_against_inventory(&asset, &namespace("other-ns"), &capture).unwrap_err();
        assert_eq!(error.code(), "error.history_commitment_mismatch");
    }

    #[test]
    fn a_document_commitment_disagreement_refuses() {
        let capture = capture("ns", 2, 0);
        let mut asset = asset_for(&capture, 2);
        asset.rows[0].commit_document_set_sha256 = Sha256ValueV1::digest(b"different");
        let error = prove_against_inventory(&asset, &namespace("ns"), &capture).unwrap_err();
        assert_eq!(error.code(), "error.history_commitment_mismatch");
    }

    #[test]
    fn a_fresh_catalog_needs_no_inventory_asset() {
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        assert!(
            load_inventory_asset(&catalog, Path::new("/nonexistent/projects.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_migrated_catalog_without_its_asset_refuses() {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.origin = CatalogOriginV2::MigratedV1 {
            transaction_id: ProjectCatalogTransactionId::mint(),
        };
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let error = load_inventory_asset(&catalog, &root.join("projects.json")).unwrap_err();
        assert_eq!(error.code(), "error.history_inventory_missing");
    }

    #[test]
    fn one_repo_history_observed_at_two_namespaces_refuses() {
        // Built at the function boundary rather than through a real catalog:
        // `validate_catalog` keeps namespace assignment globally unique per
        // repo-history record, so this state is unrepresentable on disk and a
        // catalog-built fixture could never reach the guard. The test pins the
        // GUARD, not its reachability. If that uniqueness invariant ever
        // regresses, the failure surfaces here as a typed refusal instead of a
        // silent last-writer-wins advancement that would strand one of the two
        // generations unreferenced.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let generations = HistoryGenerationStore::open_for_index(&root.join("index")).unwrap();
        let repo_history_id = history(1);

        let mut namespaces = Vec::new();
        for name in ["first-ns", "second-ns"] {
            let captured = capture(name, 1, 0);
            let generation = generations
                .create_or_open(HistoryGenerationInputV1 {
                    namespace: namespace(name),
                    owner: HistoryGenerationOwnerV1::Owned {
                        repo_history_id: repo_history_id.clone(),
                    },
                    commit_documents: captured.commit_documents,
                    vector_inputs: captured.vector_inputs,
                    truncated_message_count: 0,
                    source_schema_version: "fixture-schema".to_string(),
                    source_schema_fingerprint_sha256: "0".repeat(64),
                    source_index_fingerprint_sha256: "1".repeat(64),
                })
                .unwrap();
            namespaces.push(MaterializedNamespaceV1 {
                namespace: namespace(name),
                classification: NamespaceClassificationV1::Owned {
                    repo_history_id: repo_history_id.clone(),
                },
                generation,
            });
        }
        // The namespace is in the id preimage, so the two records genuinely
        // disagree; a same-id pair would legitimately be a no-op instead.
        assert_ne!(namespaces[0].generation.id, namespaces[1].generation.id);

        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        let error = advance_catalog_materialization(&store, epoch, &namespaces).unwrap_err();
        assert_eq!(error.code(), "error.history_commitment_mismatch");
        assert!(
            error.message().contains("two different"),
            "unexpected refusal message: {}",
            error.message()
        );
        // Fail-closed before the store is touched: nothing advanced.
        assert_eq!(store.snapshot().unwrap().epoch(), epoch);
    }
}
