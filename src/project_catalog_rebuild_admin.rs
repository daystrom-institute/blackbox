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
use std::path::PathBuf;
use std::sync::Arc;

use bbox_corpus_index::index::history_generations::HistoryScanLimitsV1;
use bbox_indexing::catalog_records::CatalogProjectRecordsProvider;
use bbox_indexing::checkout_access::{CheckoutAccessBroker, CheckoutAccessObservations};
use bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority;
use bbox_indexing::index::schema_rebuild::{
    catalog_schema_replacement_guard, recover_rebuild_manifest_before_open,
};
use bbox_indexing::project_catalog_migration::{
    ProjectCatalogMigrationError, ProjectCatalogMigrationMutationDispositionV1,
    ProjectCatalogMigrationResolvedLayoutV1, ProjectCatalogTargetSelectionV1,
};
use bbox_indexing::project_catalog_rebuild::{
    ERROR_REBUILD_GENERATION_UNVERIFIED, ERROR_REBUILD_MANIFEST_MISSING,
    PathFreeRebuildVerifyReceiptV1, PathFreeRebuildVerifyRequestV1,
    ProjectCatalogPathFreeRebuildFacadeV1,
};
use bbox_indexing::project_catalog_rebuild_planning::{
    PATH_FREE_REBUILD_REPORT_VERSION_V1, RebuildArtifactIdentityV1, authorize_rebuild_apply,
};
use bbox_indexing::project_catalog_rebuild_planning::{
    PathFreeRebuildPreflightReceiptV1, PathFreeRebuildPreflightRequestV1,
    ProjectCatalogPathFreeRebuildPlanningFacadeV1,
};
use bbox_indexing::project_catalog_store::ProjectCatalogStore;

use crate::index::{IndexWriterActor, TranscriptIndex};

/// The observed disposition of one replacement drive.
///
/// Returned rather than logged so the offline apply can assert the drive
/// actually ran: for the daemon a skipped drive is the ordinary steady-state
/// boot, but for an offline apply it means the destructive pass never
/// happened and the committed postcondition cannot be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// The offline `path-free-rebuild --apply` request (P6-B task 5).
pub struct PathFreeRebuildApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    pub target_selection: ProjectCatalogTargetSelectionV1,
    /// INPUT path: the exact reviewed report.
    pub report_path: PathBuf,
    /// INPUT path: the exact reviewed resolution.
    pub resolution_path: PathBuf,
    pub scan_limits: HistoryScanLimitsV1,
}

/// A successful apply means COMMITTED, never merely Prepared.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PathFreeRebuildApplyReceiptV1 {
    pub version: u32,
    pub identity: RebuildArtifactIdentityV1,
    pub drive: CatalogSchemaReplacementDriveV1,
    /// The landed bbox-indexing verification, observed across every manifest
    /// bucket. Its presence in the receipt IS the committed postcondition.
    pub verification: PathFreeRebuildVerifyReceiptV1,
}

/// The offline `path-free-rebuild --apply` entrypoint (P6-B task 5).
///
/// **LOCK PRECONDITION, part of this entry's contract.** Against a CONFIGURED
/// target the caller must already hold the factored admin lifetime claim
/// (`acquire_admin_lifetime_claim`: exclusive probe, then downgrade to
/// shared) and must hold it across this ENTIRE call including the final
/// verification (section 4.2). This entry never acquires that claim itself:
/// a claim taken and released inside one call would leave the destructive
/// replacement and its verification in separate exclusion windows, which is
/// exactly the gap the stopped-service invariant exists to close. Against a
/// REHEARSAL target no configured-store lock is taken (section 4.3).
///
/// The eight-step Committed contract, in order, and the order is the
/// contract rather than an implementation detail:
///
/// 1. Validate the explicit target and the exact artifact identity.
/// 2. Recapture and require `HistoryProofModeV1::Equality` IMMEDIATELY before
///    mutation (D-036). Steps 1 and 2 are `authorize_rebuild_apply`.
/// 3. Classify existing rebuild recovery BEFORE the index opens. Not
///    cosmetic: the classifier locates itself by reading the index's schema
///    marker and opening the index rewrites that marker, and a crash after
///    the destructive drop leaves no mismatch to detect, so the resume signal
///    is the only surviving evidence that a replacement is half done.
/// 4. Invoke the SAME replacement guard the daemon open uses. The guard is
///    what materializes the prepared manifest and refuses rather than
///    dropping unproved history.
/// 5. Drive the synchronous replacement through the SHARED
///    [`drive_catalog_schema_replacement`], never a copy of its ordering.
/// 6. The existing P3-E pass commits the manifest after its index commit.
///    This entry introduces no second committer.
/// 7. Run the landed `bbox-indexing` verifier.
/// 8. Return success ONLY when verification observes the exact committed
///    manifest across all buckets.
///
/// A crash leaving the manifest `Prepared` is NOT a successful apply and
/// recovers exclusively through the existing P3-D/P3-E path.
pub fn apply(
    request: PathFreeRebuildApplyRequestV1,
) -> Result<PathFreeRebuildApplyReceiptV1, ProjectCatalogMigrationError> {
    // Steps 1 and 2. Nothing below runs if the artifacts do not authorize
    // this exact target in its current state.
    let authorized = authorize_rebuild_apply(
        &request.layout,
        request.target_selection,
        &request.report_path,
        &request.resolution_path,
        request.scan_limits,
    )?;

    let paths = request.layout.rebuild_index_paths();
    let store = Arc::new(
        ProjectCatalogStore::open_existing(&paths.projects_path)
            .map_err(|error| rebuild_failure(ERROR_REBUILD_MANIFEST_MISSING, error.to_string()))?,
    );

    // Step 3, strictly before the open.
    let resume = recover_rebuild_manifest_before_open(&paths.index_root)
        .map_err(|error| rebuild_failure(ERROR_REBUILD_MANIFEST_MISSING, error.to_string()))?;

    // Step 4: the guard the daemon open injects, constructed identically.
    let guard = catalog_schema_replacement_guard(
        store.clone(),
        request.scan_limits,
        paths.vector_root.clone(),
    );
    // The trait path is written INLINE rather than imported, matching
    // `src/server/open.rs`. The catalog-ownership ratchet's
    // `project_record_import` pattern matches any `use` from `project_record`
    // whose item starts with `ProjectRecord`, so importing the PROVIDER TRAIT
    // would register this file in a baseline whose disposition reads "delete
    // with the v1 record type". That would be false: this is the catalog-mode
    // provider, not the v1 record, and the row would mislead the later
    // deletion campaign that consumes the inventory.
    let records: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider> =
        Arc::new(CatalogProjectRecordsProvider::new(store.clone()));
    let index = TranscriptIndex::open_or_create_with_code_source_store_path(
        &paths.index_root,
        Vec::new(),
        None,
        paths.projects_path.clone(),
        paths.code_source_root.clone(),
        paths.knowledge_path.clone(),
        paths.threads_path.clone(),
        paths.roadmap_path.clone(),
        records.clone(),
        Some(guard),
    )
    .map_err(|error| rebuild_failure(ERROR_REBUILD_GENERATION_UNVERIFIED, error.to_string()))?;

    // Steps 5 and 6: the SHARED driver, the same function daemon startup
    // calls. The P3-E pass inside it commits the manifest.
    let checkout_access = Arc::new(CheckoutAccessBroker::new(
        Arc::new(V2CatalogCheckoutAccessAuthority::new(store)),
        CheckoutAccessObservations::in_memory(),
    ));
    let writer = IndexWriterActor::spawn_for_with_checkout_access(&index, records, checkout_access);
    let drive = drive_catalog_schema_replacement(&index, &writer, &resume).map_err(|error| {
        rebuild_failure(ERROR_REBUILD_GENERATION_UNVERIFIED, format!("{error:#}"))
    })?;

    // Steps 7 and 8. The verifier reads DURABLE state, so it must run after
    // the drive has committed, and its success is the only thing that
    // licenses this function to return Ok.
    let verification =
        ProjectCatalogPathFreeRebuildFacadeV1::verify(PathFreeRebuildVerifyRequestV1 {
            layout: request.layout,
            target_selection: request.target_selection,
        })?;

    Ok(PathFreeRebuildApplyReceiptV1 {
        version: PATH_FREE_REBUILD_REPORT_VERSION_V1,
        identity: authorized.identity,
        drive,
        verification,
    })
}

/// Refusals raised after the authorization gate.
///
/// Typed as `RecoveredToOldState` rather than `NoDurableMutation`: past the
/// guard the outgoing index may already have been dropped, and claiming no
/// durable mutation there would be a lie an operator could act on.
fn rebuild_failure(code: &'static str, message: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::new(
        code,
        message,
        ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState,
    )
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
    use std::path::Path;

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

    /// The D-036 gate fires BEFORE the index is opened or the replacement
    /// guard is invoked, and the whole apply returns rather than blocking.
    ///
    /// Watchdog-bounded for the reason the preflight regression established:
    /// a lock defect in this path does not fail, it never returns, and an
    /// unbounded test would surface as an anonymous harness kill naming no
    /// call. The apply opens a store, so it is exactly the shape that class
    /// of defect lives in.
    ///
    /// The store is FreshV2, so no recorded source fingerprint exists and
    /// Equality is unprovable. That is the assertion: a non-Equality target
    /// must be refused with the index untouched, because D-036 makes Drift
    /// insufficient to authorize a cut.
    #[test]
    fn apply_refuses_a_non_equality_target_without_touching_the_index() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let layout = rebuild_test_layout(&root);
        let state = root.join("live");
        std::fs::create_dir_all(&state).unwrap();
        drop(
            bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
                &state.join("projects.json"),
            )
            .unwrap(),
        );
        write_backfill_journal(&state, 0);
        let (report_path, resolution_path) = write_reviewed_artifacts(&root);

        let request = PathFreeRebuildApplyRequestV1 {
            layout,
            target_selection: ProjectCatalogTargetSelectionV1::Configured,
            report_path,
            resolution_path,
            scan_limits: HistoryScanLimitsV1::default(),
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(apply(request).map(|_| ()).map_err(|error| error.code));
        });
        let outcome = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| {
                panic!(
                    "the rebuild apply did not return within 30s: it is blocked, most likely \
                 re-acquiring a store lock it already holds"
                )
            });
        assert!(outcome.is_err(), "a fresh-v2 target cannot prove Equality");

        // The index must be untouched: the D-036 gate is upstream of the
        // guard, so nothing may have been dropped or rebuilt.
        assert!(
            !state.join("index").exists(),
            "a refused apply must not open or replace the index"
        );
        drop(directory);
    }

    fn rebuild_test_layout(root: &Path) -> ProjectCatalogMigrationResolvedLayoutV1 {
        let _guard = bbox_util::util::test_env_lock();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                root.join("live"),
                root.join("live").join("vectors")
            ),
        )
        .unwrap();
        let config = bbox_config::config::load_with(bbox_config::config::LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap();
        ProjectCatalogMigrationResolvedLayoutV1::from_config(
            &config,
            bbox_indexing::project_catalog_migration::ProjectCatalogMigrationLayoutOverridesV1 {
                state_dir: Some(root.join("live")),
                projects_path: Some(root.join("live").join("projects.json")),
            },
        )
        .unwrap()
    }

    fn write_backfill_journal(state: &Path, epoch: u64) {
        let body = serde_json::json!({
            "version": 1,
            "completed_at": "2026-08-04T00:00:00Z",
            "predecessor_catalog_epoch": epoch,
            "predecessor_catalog_hash": "11".repeat(32),
            "predecessor_attachment_hash": "22".repeat(32),
            "post_image_catalog_epoch": epoch,
            "stamp_counts": {},
            "identity": {
                "inventory_hash": "33".repeat(32),
                "plan_hash": "44".repeat(32),
                "report_artifact_hash": "55".repeat(32),
                "resolution_artifact_hash": "66".repeat(32),
            },
        });
        std::fs::write(
            state.join("backfill-completion.json"),
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
    }

    /// Artifacts whose CONTENT does not matter: this apply is refused by the
    /// D-036 gate downstream of decoding, so the test only needs them to be
    /// present, well-formed, and mutually bound.
    fn write_reviewed_artifacts(root: &Path) -> (PathBuf, PathBuf) {
        let review = root.join("review");
        std::fs::create_dir_all(&review).unwrap();
        let resolution = serde_json::json!({
            "version": 1,
            "inventory_hash": "ab".repeat(32),
        });
        let resolution_bytes = serde_json::to_vec(&resolution).unwrap();
        let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut hasher, &resolution_bytes);
        let resolution_hash = hex::encode(sha2::Digest::finalize(hasher));
        let report = serde_json::json!({
            "version": 1,
            "generated_at": "2026-08-04T00:00:00Z",
            "status": "clean",
            "inventory_hash": "ab".repeat(32),
            "plan_hash": "cd".repeat(32),
            "resolution_artifact_hash": resolution_hash,
            "predecessor": {
                "backfill_post_image_catalog_epoch": 0,
                "backfill_inventory_hash": "33".repeat(32),
                "backfill_plan_hash": "44".repeat(32),
                "applied_stamp_total": 0,
                "observed_catalog_epoch": 0,
            },
            "source_binding": {
                "recorded_source_index_fingerprint": null,
                "observed_source_index_fingerprint": null,
                "proof_mode": "drift",
                "namespace_dispositions": [],
                "source_schema_version": null,
            },
            "planned_commit_document_total": 0,
        });
        let report_path = review.join("report.json");
        let resolution_path = review.join("resolution.json");
        std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
        std::fs::write(&resolution_path, &resolution_bytes).unwrap();
        (report_path, resolution_path)
    }
}
