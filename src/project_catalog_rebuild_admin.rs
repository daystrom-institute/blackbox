//! Root-crate administration for the Phase 6 path-free rebuild
//! (`durable-project-catalog-phase6-impl.md` section 3.4 and P6-B task 5).
//!
//! **Why this lives in the root crate (adjudication Q-D, ratified).** The
//! executable replacement sequence is COMPOSED here: guard injection at index
//! open, prepared-manifest materialization, the destructive full pass, the
//! sole P3-D committer, and the synchronous post-reset drive. `bbox-indexing`
//! owns each of those pieces, but nothing below this crate owns their
//! ordering, and the ordering is the part that a second implementation would
//! get wrong. So the rebuild's offline entrypoints live here, beside the one
//! function daemon startup already drives.
//!
//! **The single-driver invariant.** [`drive_catalog_schema_replacement`] is
//! the ONLY implementation of the post-reset drive. `src/server/open.rs`
//! calls it at boot; the offline apply calls the same function. It was
//! refactored out of the daemon open path rather than copied, because a copy
//! forks exactly the ordering that a torn replacement recovers through, and
//! the two copies would then disagree only in the crash cases nobody
//! exercises by hand.
//!
//! What this module does NOT own, and must not grow: no writable store
//! implementation, no alternative manifest encoder, no commit callback, and
//! no parallel recovery state machine. Manifest recovery classification, the
//! replacement guard, the P3-E committer, and the committed-manifest verifier
//! all stay in `bbox-indexing`, and this module calls them.

use anyhow::{Context, Result};
use bbox_indexing::index::schema_rebuild::SchemaRebuildResume;
use bbox_indexing::project_catalog_migration::ProjectCatalogMigrationError;
use bbox_indexing::project_catalog_rebuild_planning::{
    PathFreeRebuildPreflightReceiptV1, PathFreeRebuildPreflightRequestV1,
    ProjectCatalogPathFreeRebuildPlanningFacadeV1,
};

use crate::index::{IndexWriterActor, TranscriptIndex};

/// The observed disposition of one replacement drive.
///
/// Returned rather than logged so the offline apply can assert the drive
/// actually ran: for the daemon a skipped drive is the ordinary steady-state
/// boot, but for an offline apply it means the destructive pass never
/// happened and the committed postcondition cannot be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogSchemaReplacementDriveV1 {
    /// The index carried no reset marker and no interrupted rebuild, so
    /// there was nothing to drive.
    NotRequired,
    /// The synchronous replacement ran to completion and the schema-migration
    /// version marker was committed.
    Completed,
}

/// Drive the synchronous schema-replacement rebuild to completion.
///
/// THE shared driver (P6-B task 5): daemon startup and the offline
/// path-free-rebuild apply both call this function, and neither carries its
/// own copy of the ordering below.
///
/// The four steps are ordered for reasons that are not cosmetic:
///
/// 1. `run_reindex_pass_for_schema_migration` re-stages from the proved
///    sources. It names its cause explicitly rather than passing
///    `run_reindex_pass(true, true)`, because this pass runs against the
///    index the replacement guard just authorized emptying and the
///    preservation gates must not verify against it.
/// 2. The reader reloads so the staged documents are visible to step 3.
/// 3. `refresh_active_code_selectors` re-reads the selector map from the
///    edge-sidecar manifest. The paired `INDEXER_VERSION` bump changes every
///    collected selector's materialization suffix, so the pass above may have
///    migrated projects onto a new selector and flipped the manifest; a
///    caller that skipped this would build a read view that filters out
///    exactly the documents the rebuild just staged.
/// 4. The version marker commits LAST. A crash before it leaves the
///    replacement detectable and recoverable through the existing
///    P3-D/P3-E path; committing earlier would erase the evidence.
///
/// The `Resume` arm forces the same drive a fresh schema mismatch does.
/// After a crash that already dropped the index there is no marker left to
/// mismatch against, so `schema_was_reset()` is false and the prepared
/// manifest is the only surviving evidence that a replacement is half done.
pub fn drive_catalog_schema_replacement(
    idx: &TranscriptIndex,
    index_writer: &IndexWriterActor,
    rebuild_resume: &SchemaRebuildResume,
) -> Result<CatalogSchemaReplacementDriveV1> {
    let resume_interrupted_rebuild = matches!(rebuild_resume, SchemaRebuildResume::Resume { .. });
    if !idx.schema_was_reset() && !resume_interrupted_rebuild {
        return Ok(CatalogSchemaReplacementDriveV1::NotRequired);
    }
    tracing::info!(
        schema = crate::index::INDEX_SCHEMA_VERSION,
        resume_interrupted_rebuild,
        "running synchronous full rebuild after index schema migration"
    );
    index_writer
        .run_reindex_pass_for_schema_migration()
        .context("synchronous schema-migration rebuild failed")?;
    idx.reader_reload_for_test();
    idx.refresh_active_code_selectors()
        .context("refreshing active code selectors after the schema-migration rebuild")?;
    idx.complete_schema_migration()
        .context("committing schema-migration version marker failed")?;
    Ok(CatalogSchemaReplacementDriveV1::Completed)
}

/// The offline `path-free-rebuild --preflight` entrypoint (P6-B task 5).
///
/// The root crate owns the rebuild's entrypoints because it owns the
/// executable composition (Q-D). Preflight is the read-only half, so this
/// entry is deliberately thin: it resolves nothing the CLI has not already
/// resolved and adds no authority of its own. It exists here rather than in
/// the CLI so that offline preflight and offline apply are reached through
/// ONE module, and so the apply entry beside it cannot quietly acquire a
/// different set of preconditions than the preflight that authorized it.
///
/// STRICTLY READ-ONLY. It scans, proves Equality (D-036), consumes
/// `BackfillCompletionJournalV1` as its predecessor binding, and writes the
/// two reviewed artifacts. It never invokes the replacement guard, writes a
/// prepared manifest, creates a generation, or opens the destructive
/// replacement path; the shared lifetime lock it takes does not exclude a
/// live daemon's own shared handle (section 4.1).
pub fn preflight(
    request: PathFreeRebuildPreflightRequestV1,
) -> Result<PathFreeRebuildPreflightReceiptV1, ProjectCatalogMigrationError> {
    ProjectCatalogPathFreeRebuildPlanningFacadeV1::preflight(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bbox_indexing::checkout_access::{CheckoutAccessBroker, CheckoutAccessObservations};
    use bbox_indexing::checkout_access_v1::V1CheckoutAccessAuthority;
    use bbox_indexing::checkout_registry::CheckoutRegistry;
    use bbox_indexing::index::writer_actor::IndexWriterActor as WriterActor;
    use bbox_indexing::projects::{BridgeProjectRecordsProvider, ProjectRegistry};

    /// A freshly created index at the CURRENT schema, plus the writer actor
    /// the driver dispatches its pass through.
    fn fresh_index(root: &std::path::Path) -> (TranscriptIndex, WriterActor) {
        let projects_path = root.join("projects.json");
        let idx = TranscriptIndex::open_or_create_with_records(
            &root.join("index"),
            Vec::new(),
            None,
            projects_path.clone(),
            root.join("blackbox-knowledge.json"),
            root.join("blackbox-threads.json"),
            root.join("blackbox-roadmap.json"),
            Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(&projects_path).unwrap(),
        ));
        let checkouts = Arc::new(parking_lot::RwLock::new(
            CheckoutRegistry::open(&root.join("checkout-registry.json")).unwrap(),
        ));
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(V1CheckoutAccessAuthority::new(projects.clone(), checkouts)),
            CheckoutAccessObservations::in_memory(),
        ));
        let writer = WriterActor::spawn_for_with_checkout_access(
            &idx,
            Arc::new(BridgeProjectRecordsProvider::new(projects)),
            broker,
        );
        (idx, writer)
    }

    /// The driver's predicate, pinned. An index that was not reset and has no
    /// interrupted rebuild must not be driven: the daemon reaches this call on
    /// EVERY boot, so a predicate that drifted open would turn each ordinary
    /// restart into a destructive full rebuild.
    #[test]
    fn a_steady_state_index_is_not_driven() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (idx, writer) = fresh_index(&root);
        assert!(
            !idx.schema_was_reset(),
            "a freshly created index is already at the current schema"
        );
        assert_eq!(
            drive_catalog_schema_replacement(&idx, &writer, &SchemaRebuildResume::None).unwrap(),
            CatalogSchemaReplacementDriveV1::NotRequired
        );
    }
}
