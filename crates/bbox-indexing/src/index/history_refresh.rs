//! Live repo-history refresh (Phase 3 plan section 10 item 3).
//!
//! The pre-replacement materializer builds a generation by SCANNING an
//! outgoing index. This module builds one from a LIVE consolidated walk. Both
//! go through the same creation path
//! ([`super::history_materializer::create_history_generation`]) for the
//! reason documented there: generation identity is content-addressed, so a
//! second constructor forks the catalog's notion of what is materialized.
//!
//! GENERATIONS ARE IMMUTABLE. Nothing here appends to an existing generation.
//! A refresh that observes new commits loads the outgoing generation's
//! complete document set, merges the walk's rows into it, and creates a NEW
//! generation whose id differs precisely because its content does. The
//! outgoing generation stays on disk, still content-addressed, still
//! reproducible, and still pinned by anything that references it.
//!
//! D-037 VOCABULARY. Only the PRIMARY namespace's generation advances the
//! `RepoHistoryRecord.materialization` field through transact. Compatibility
//! namespaces are manifest-owned legacy lookup surfaces: `materialization` is
//! a single `Ready { generation_id }`, so a compatibility namespace has no
//! catalog slot to advance and gets none here.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bbox_corpus_core::project_catalog::{RepoHistoryId, RepoHistoryMaterialization};

use bbox_corpus_index::index::history_generations::{
    HistoryCommitDocumentV1, HistoryGenerationIdV1, HistoryGenerationInputV1,
    HistoryGenerationOwnerV1, HistoryGenerationRecordV1, HistoryGenerationStore,
    HistoryVectorInputV1, generation_rows_for_commit, live_schema_evidence,
};

use super::consolidated_history::{
    ConsolidatedWalkOutcomeV1, RepoHistoryCursorStoreV1, RepoHistoryCursorV1,
    RepoHistoryIngestGroupV1,
};
use super::history_materializer::{
    HistoryMaterializerError, HistoryMaterializerResult, create_history_generation,
};
use crate::project_catalog_store::ProjectCatalogStore;

/// Marker stored in a live-refresh generation's `source_index_fingerprint`
/// slot. See the construction site for why a constant is the honest value.
const LIVE_REFRESH_SOURCE_MARKER: &str = "blackbox.repo-history-generation.live-refresh.v1";

/// What one live refresh produced.
#[derive(Debug, Clone)]
pub struct HistoryRefreshOutcomeV1 {
    pub generation: HistoryGenerationRecordV1,
    /// The generation this one supersedes, when the record was already
    /// `Ready`. Retained on disk; only the catalog pointer moves.
    pub superseded_generation: Option<String>,
    /// `None` when the catalog already named this exact generation, which is
    /// what makes a no-change refresh a genuine no-op rather than an epoch
    /// bump.
    pub catalog_epoch_after: Option<u64>,
    /// Vector inputs for the commits this walk newly observed.
    ///
    /// ONCE PER REPO, not once per member project: the caller enqueues these
    /// exactly once. The per-project fan-out is edges only. Enqueuing per
    /// project is what the bridge lane did, and it re-embedded every commit
    /// message of a monorepo for every sibling.
    pub new_vector_inputs: Vec<HistoryVectorInputV1>,
    /// The cursor written after publication, or `None` when the walk observed
    /// no head to advance to.
    pub cursor: Option<RepoHistoryCursorV1>,
}

/// Build and publish the superseding generation for one repo-history record.
///
/// ORDERING IS THE CONTRACT, and it is the same ordering governing section 11
/// states for the first consolidated generation: create and verify the
/// generation, advance the catalog, and only THEN record the cursor. A cursor
/// written earlier would, on a crash in between, claim an interval was
/// ingested into a generation the catalog never adopted, and the next refresh
/// would start after commits nothing holds.
pub fn refresh_repo_history_generation(
    catalog_store: &ProjectCatalogStore,
    generation_store: &HistoryGenerationStore,
    cursors: &RepoHistoryCursorStoreV1,
    group: &RepoHistoryIngestGroupV1,
    walk: &ConsolidatedWalkOutcomeV1,
) -> HistoryMaterializerResult<HistoryRefreshOutcomeV1> {
    let state = catalog_store
        .snapshot()
        .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
    let pinned_epoch = state.epoch();
    let record = state
        .catalog()
        .repo_histories
        .get(&group.repo_history_id)
        .ok_or_else(|| {
            HistoryMaterializerError::commitment_mismatch(format!(
                "repo history {} vanished before its live refresh",
                group.repo_history_id
            ))
        })?
        .clone();
    if record.primary_namespace != group.primary_namespace {
        return Err(HistoryMaterializerError::commitment_mismatch(format!(
            "repo history {} changed its primary namespace during ingestion",
            group.repo_history_id
        )));
    }

    // The outgoing generation's COMPLETE document set is the base a
    // superseding generation is built on. Without it an incremental walk
    // would produce a generation holding only the new interval, and a
    // generation is a complete self-contained snapshot, never a cursor delta.
    let (mut documents, superseded) = match &record.materialization {
        RepoHistoryMaterialization::NotBuilt => (BTreeMap::new(), None),
        RepoHistoryMaterialization::Ready { generation_id } => {
            let id = HistoryGenerationIdV1::parse(generation_id.as_str())?;
            let existing = generation_store.load(&id)?;
            if existing.manifest.body.namespace != record.primary_namespace {
                return Err(HistoryMaterializerError::commitment_mismatch(format!(
                    "repo history {} is materialized under a foreign namespace",
                    group.repo_history_id
                )));
            }
            let carried = existing
                .commit_documents
                .iter()
                .map(|document| (document.entity_id.clone(), document.clone()))
                .collect::<BTreeMap<String, HistoryCommitDocumentV1>>();
            (carried, Some(generation_id.as_str().to_string()))
        }
    };

    let namespace = group.primary_namespace.as_str();
    let mut new_vector_inputs = Vec::new();
    for commit in &walk.commits {
        let (document, vector) = generation_rows_for_commit(commit, namespace);
        // A re-walked commit re-derives byte-identical rows, so an insert
        // that replaces an existing entry is a no-op in content terms. The
        // walk therefore stays idempotent under the complete-rewalk the
        // no-seed rule mandates.
        let previously_carried = documents.insert(document.entity_id.clone(), document);
        if previously_carried.is_none() {
            new_vector_inputs.push(vector);
        }
    }

    let commit_documents: Vec<HistoryCommitDocumentV1> = documents.into_values().collect();
    let truncated_message_count = commit_documents
        .iter()
        .filter(|document| {
            document
                .content
                .ends_with(bbox_corpus_index::index::git_history::TRUNCATED_COMMIT_MESSAGE_SUFFIX)
        })
        .count() as u64;
    // One vector input per commit document, exactly as the scan builds it:
    // the generation's vector inventory is its OWN complete set (what history
    // GC iterates to tombstone), not just this refresh's delta.
    let vector_inputs = commit_documents
        .iter()
        .map(|document| HistoryVectorInputV1 {
            entity_id: document.entity_id.clone(),
            content_hash: document.content_hash.clone(),
            message: document.content.clone(),
        })
        .collect::<Vec<_>>();

    let (schema_version, schema_fingerprint) = live_schema_evidence()?;
    let generation = create_history_generation(
        generation_store,
        HistoryGenerationInputV1 {
            namespace: record.primary_namespace.clone(),
            // A live refresh only ever runs for a record the catalog holds,
            // so its disposition is `Owned` by construction. The ambiguous
            // and unclaimed dispositions exist for namespaces observed in an
            // index with no (or several) catalog owners, which is a
            // pre-replacement scan concern, not a live-walk one.
            owner: HistoryGenerationOwnerV1::Owned {
                repo_history_id: group.repo_history_id.clone(),
            },
            commit_documents,
            vector_inputs,
            truncated_message_count,
            source_schema_version: schema_version,
            source_schema_fingerprint_sha256: schema_fingerprint,
            // A live refresh has no outgoing index population to fingerprint:
            // its source is the walk, not a document set someone else wrote.
            // A constant marker states that honestly. It cannot shift
            // identity, because source evidence is outside the id preimage
            // (D-039): the id pins the document and vector commitments, so a
            // scan of the same content and this refresh derive the SAME
            // generation id, and the marker is provenance only, never
            // compared against a scan fingerprint.
            source_index_fingerprint_sha256: LIVE_REFRESH_SOURCE_MARKER.to_string(),
        },
    )?;

    let catalog_epoch_after = advance_primary_materialization(
        catalog_store,
        pinned_epoch,
        &group.repo_history_id,
        &generation,
    )?;

    // AFTER publication and catalog advance, never before.
    let cursor = if walk.head.is_empty() {
        None
    } else {
        let cursor = RepoHistoryCursorV1 {
            version: 1,
            repo_history_id: group.repo_history_id.as_str().to_string(),
            commit_namespace: namespace.to_string(),
            last_ingested_sha: walk.head.clone(),
            generation_id: generation.id.as_str().to_string(),
            updated_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        cursors.save(&cursor).map_err(|error| {
            HistoryMaterializerError::new("error.history_cursor_write_failed", error.to_string())
        })?;
        Some(cursor)
    };

    Ok(HistoryRefreshOutcomeV1 {
        superseded_generation: superseded.filter(|previous| previous != generation.id.as_str()),
        generation,
        catalog_epoch_after,
        new_vector_inputs,
        cursor,
    })
}

/// Move `RepoHistoryRecord.materialization` onto the refreshed generation.
///
/// Unlike the pre-replacement materializer's advance, a DIFFERENT existing id
/// is expected here rather than a refusal: superseding is exactly what a live
/// refresh does. The materializer refuses that case because it is proving an
/// outgoing index against a recorded materialization, where a disagreement
/// means the two describe different content; here the disagreement IS the new
/// content.
pub fn advance_primary_materialization(
    store: &ProjectCatalogStore,
    expected_epoch: u64,
    repo_history_id: &RepoHistoryId,
    generation: &HistoryGenerationRecordV1,
) -> HistoryMaterializerResult<Option<u64>> {
    let Some(generation_id) = generation.id.owned().cloned() else {
        return Err(HistoryMaterializerError::commitment_mismatch(format!(
            "live refresh of {repo_history_id} produced a quarantine generation id"
        )));
    };
    {
        let state = store
            .snapshot()
            .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
        let record = state
            .catalog()
            .repo_histories
            .get(repo_history_id)
            .ok_or_else(|| {
                HistoryMaterializerError::commitment_mismatch(format!(
                    "repo history {repo_history_id} vanished before its materialization advance"
                ))
            })?;
        if matches!(
            &record.materialization,
            RepoHistoryMaterialization::Ready { generation_id: current } if current == &generation_id
        ) {
            return Ok(None);
        }
    }
    let commit = store
        .transact(expected_epoch, |catalog, _attachments| {
            // The pinned snapshot proved the record exists and the epoch CAS
            // proves nothing mutated since, so absence here is corruption
            // rather than a race. Same posture as the materializer's advance.
            let record = catalog
                .repo_histories
                .get_mut(repo_history_id)
                .expect("the pinned repo history exists in the transacted catalog");
            record.materialization = RepoHistoryMaterialization::Ready {
                generation_id: generation_id.clone(),
            };
            Ok(())
        })
        .map_err(|error| HistoryMaterializerError::new(error.code(), error.to_string()))?;
    Ok(Some(commit.epoch))
}
