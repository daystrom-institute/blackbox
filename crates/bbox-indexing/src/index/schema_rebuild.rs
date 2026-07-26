//! Wiring of the P3-D history substrate into the live replacement boundary
//! (Phase 3 milestone P3-E, plan section 9 item 2).
//!
//! `bbox-corpus-index` owns the guard CONTRACT and the generation primitives;
//! it cannot depend on this crate, so it never calls the materializer. This
//! module supplies the two production guards it injects, plus the post-drop
//! steps the rebuild pass runs:
//!
//! 1. catalog guard: materialize every observed namespace to `Ready` and write
//!    the PREPARED rebuild manifest. Only its success authorizes the reset.
//! 2. bridge guard: write and fsync the commit spill before the drop.
//! 3. after the drop, inside the rebuild pass and BEFORE any checkout walk:
//!    re-emit commit documents from the pinned generations, verify each
//!    generation's vector-input rows against live vector coverage and
//!    re-enqueue the misses, then COMMIT the manifest naming the resulting
//!    views.
//!
//! Ordering in step 3 is load-bearing: re-emission precedes the checkout walks
//! so an attachment-less project's history is present regardless of whether any
//! checkout turns out to be reachable, and the manifest is committed only after
//! the population it promises is in the writer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_corpus_core::project_record::ProjectRecordsProvider;
use bbox_corpus_index::index::FieldHandles;
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationIdV1, HistoryGenerationStore, HistoryScanLimitsV1,
    RepoHistoryRebuildCommittedV1, RepoHistoryRebuildManifestV1, RepoHistoryRebuildRecoveryV1,
    RepoHistoryRebuildStateV1, RepoHistoryRebuildVectorRowV1, scan_commit_documents,
};
use bbox_corpus_index::index::schema_replacement::{
    CommitDocumentOwnerV1, CommitSpillFileV1, SchemaReplacementAuthorization,
    SchemaReplacementGuard, SchemaReplacementRequest, reemit_commit_documents,
    spill_namespaces_from_rows, write_commit_spill,
};
use tantivy::IndexWriter;

use crate::index::history_materializer::{
    HistoryMaterializerRequestV1, materialize_history_generations, prepare_rebuild_manifest,
};
use crate::project_catalog_store::ProjectCatalogStore;

/// Label bound into the prepared manifest as the planned lexical view: the
/// schema the replacement targets.
fn planned_lexical_label(request: &SchemaReplacementRequest<'_>) -> String {
    format!("lexical:{}", request.target_schema_version)
}

/// Label bound into the prepared manifest as the planned vector view. Commit
/// vectors key on `(entity_id, content_hash)` and this milestone changes
/// neither for the commit lane, so the target view is the same vector schema
/// the outgoing index used.
fn planned_vector_label() -> String {
    format!("vector:{}", bbox_vectors::VECTOR_SCHEMA_VERSION)
}

/// Catalog-mode guard: the P3-D materializer orchestration.
///
/// Drives every namespace observed in the outgoing index to `Ready`, proves
/// each against the persisted Phase 1 commitments, and writes the prepared
/// rebuild manifest. Any refusal (missing generation, corrupt manifest,
/// commitment mismatch) propagates out of the guard, which aborts the reset
/// with the outgoing index and its lexical and vector views untouched.
pub fn catalog_schema_replacement_guard(
    store: Arc<ProjectCatalogStore>,
    scan_limits: HistoryScanLimitsV1,
) -> SchemaReplacementGuard {
    Arc::new(move |request: &SchemaReplacementRequest<'_>| {
        let scan = scan_commit_documents(request.index_path, scan_limits)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let outcome = materialize_history_generations(
            &store,
            &HistoryMaterializerRequestV1 {
                index_path: request.index_path.to_path_buf(),
                projects_path: request.projects_path.to_path_buf(),
                scan_limits,
            },
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let Some(scan) = scan else {
            // No index or no marker: nothing to carry and nothing to prove.
            // The reset is authorized with no manifest, exactly as a fresh
            // store's first open is.
            return Ok(SchemaReplacementAuthorization::new(
                "catalog-materializer: no legacy history observed",
            ));
        };
        let catalog_epoch = outcome.catalog_epoch_after.unwrap_or_else(|| {
            store
                .snapshot()
                .map(|state| state.epoch())
                .unwrap_or_default()
        });
        let prepared = prepare_rebuild_manifest(
            &scan,
            &outcome,
            catalog_epoch,
            planned_lexical_label(request),
            planned_vector_label(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let generation_store = HistoryGenerationStore::open_for_index(request.index_path)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let manifest = generation_store
            .write_prepared_rebuild_manifest(prepared)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if !outcome.namespaces_with_truncated_messages.is_empty() {
            // Not a refusal: re-emitted vector keys for these namespaces hash
            // the truncated text the index carried, so the legacy key is
            // superseded rather than reproduced. Surfaced instead of being
            // discovered later as silent vector churn.
            tracing::warn!(
                namespaces = ?outcome.namespaces_with_truncated_messages,
                "history namespaces carry truncated commit messages; their vector keys are superseded"
            );
        }
        Ok(SchemaReplacementAuthorization::new(format!(
            "catalog-materializer: rebuild manifest {} prepared over {} namespaces",
            manifest.rebuild_id,
            manifest.prepared.namespace_inventory.len()
        )))
    })
}

/// Bridge-mode guard: the commit spill lane.
///
/// Writes and fsyncs every commit document observed in the outgoing index to
/// `<family root>/commit-spill/` BEFORE the drop, so a bridge project whose
/// checkout is unavailable keeps its history across the schema reset instead of
/// losing it and waiting for a Git re-walk that may never be possible.
pub fn bridge_schema_replacement_guard(
    records_provider: Arc<dyn ProjectRecordsProvider>,
) -> SchemaReplacementGuard {
    Arc::new(move |request: &SchemaReplacementRequest<'_>| {
        let Some(scan) = scan_commit_documents(request.index_path, HistoryScanLimitsV1::default())
            .map_err(|error| anyhow::anyhow!("{error}"))?
        else {
            return Ok(SchemaReplacementAuthorization::new(
                "bridge-spill: no legacy history observed",
            ));
        };
        if scan.namespaces.is_empty() {
            // An index with no commit documents has nothing to carry. Writing an
            // empty spill would leave a file every subsequent open has to
            // consume for no reason.
            return Ok(SchemaReplacementAuthorization::new(
                "bridge-spill: no commit documents to carry",
            ));
        }
        let owners = bridge_commit_owners(records_provider.as_ref());
        let rows = scan
            .namespaces
            .into_iter()
            .map(|(namespace, capture)| (namespace, capture.commit_documents))
            .collect::<BTreeMap<_, _>>();
        let spill = CommitSpillFileV1::new(
            request.observed_schema_version.clone(),
            spill_namespaces_from_rows(rows, &owners),
        );
        let count = spill.commit_document_count();
        write_commit_spill(request.index_path, &spill)?;
        Ok(SchemaReplacementAuthorization::new(format!(
            "bridge-spill: {count} commit documents fsynced across {} namespaces",
            spill.namespaces.len()
        )))
    })
}

/// Namespace (`repo_id`) to owner, from the bridge project records.
///
/// Several registered projects can share one `repo_id` (a repo plus its
/// worktrees), and commit documents are keyed by `commit:<repo_id>:<sha>` with
/// no project component, so the namespace's document set is already
/// project-ambiguous in the outgoing index. The lexicographically first
/// project id wins so the choice is deterministic across boots rather than
/// dependent on record iteration order.
fn bridge_commit_owners(
    records_provider: &dyn ProjectRecordsProvider,
) -> BTreeMap<String, CommitDocumentOwnerV1> {
    let mut owners: BTreeMap<String, CommitDocumentOwnerV1> = BTreeMap::new();
    for record in records_provider.records_snapshot().records.iter() {
        let Some(repo_id) = record.repo_id.clone() else {
            continue;
        };
        let display = record
            .aliases
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| record.project_id.clone());
        match owners.get(&repo_id) {
            Some(existing)
                if existing
                    .project_id
                    .as_deref()
                    .is_some_and(|current| current <= record.project_id.as_str()) => {}
            _ => {
                owners.insert(
                    repo_id,
                    CommitDocumentOwnerV1 {
                        project_id: Some(record.project_id.clone()),
                        project_display: display,
                    },
                );
            }
        }
    }
    owners
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HistoryReemissionOutcome {
    pub namespaces: usize,
    pub commit_documents: u64,
    pub vectors_verified: u64,
    pub vectors_reenqueued: u64,
    pub generation_ids: BTreeSet<String>,
    /// Per-generation verified-or-re-enqueued inventory, bound into the
    /// COMMITTED manifest so the replacement's vector promise is durable
    /// evidence rather than a log line (governing section 10.3).
    pub vector_inventory: Vec<RepoHistoryRebuildVectorRowV1>,
}

/// Re-emit commit documents from the generations a PREPARED manifest pins.
///
/// Called inside the schema rebuild pass, before the checkout walks. Returns
/// `Ok(None)` when there is no prepared manifest (bridge mode, or a catalog
/// open with no legacy history): the spill lane and the ordinary Git walk cover
/// those cases.
///
/// A manifest that names a generation missing from disk, or one whose bytes no
/// longer re-derive their own commitments, fails here. That is deliberately
/// BEFORE the pass reaches `delete_all_documents`, matching the collected-code
/// preservation arm: a rebuild must never delete the live population and then
/// discover it cannot restore the history it promised.
pub fn reemit_prepared_history_generations(
    index_path: &Path,
    writer: &IndexWriter,
    fields: FieldHandles,
    owners: &BTreeMap<String, CommitDocumentOwnerV1>,
) -> Result<Option<HistoryReemissionOutcome>> {
    let store = HistoryGenerationStore::open_for_index(index_path)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let Some(manifest) = store
        .read_rebuild_manifest()
        .map_err(|error| anyhow::anyhow!("{error}"))?
    else {
        return Ok(None);
    };
    if manifest.state != RepoHistoryRebuildStateV1::Prepared {
        // Already committed: its population is in the index and re-emitting
        // would be a no-op at best. Leave it alone; it stays a GC root.
        return Ok(None);
    }
    let mut outcome = HistoryReemissionOutcome::default();
    for row in &manifest.prepared.namespace_inventory {
        let id = HistoryGenerationIdV1::parse(&row.generation_id)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let generation = store
            .load(&id)
            .with_context(|| {
                format!(
                    "rebuild manifest {} names history generation {} which cannot be loaded",
                    manifest.rebuild_id, row.generation_id
                )
            })
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if generation.manifest.body.commit_document_commitment_sha256
            != row.commit_document_commitment_sha256
            || generation.manifest.body.commit_document_count != row.commit_document_count
        {
            anyhow::bail!(
                "error.history_commitment_mismatch: generation {} disagrees with the rebuild \
                 manifest row for namespace {}",
                row.generation_id,
                row.namespace.as_str()
            );
        }
        let owner = owners
            .get(row.namespace.as_str())
            .cloned()
            .unwrap_or_else(|| CommitDocumentOwnerV1::unclaimed(row.namespace.as_str()));
        outcome.commit_documents +=
            reemit_commit_documents(writer, fields, &generation.commit_documents, &owner)?;
        // Vector coverage, verified-or-re-enqueued from the generation's own
        // vector-input rows (governing section 10.3): a replacement must not
        // commit a lexical-only history view whose vector view was promised.
        let mut verified = 0u64;
        let mut reenqueued = 0u64;
        for input in &generation.vector_inputs {
            match bbox_corpus_index::index::embed_hook::git_message_vector_is_active(
                &input.entity_id,
                &input.content_hash,
            ) {
                Some(true) => verified += 1,
                // `None` (no probe registered) cannot prove coverage, so it
                // re-enqueues. Enqueue is idempotent and dedup-skipped.
                Some(false) | None => {
                    bbox_corpus_index::index::embed_hook::emit_git_message(
                        &input.entity_id,
                        &input.content_hash,
                        &input.message,
                    );
                    reenqueued += 1;
                }
            }
        }
        outcome.vectors_verified += verified;
        outcome.vectors_reenqueued += reenqueued;
        outcome
            .vector_inventory
            .push(RepoHistoryRebuildVectorRowV1 {
                generation_id: row.generation_id.clone(),
                vector_inputs_verified: verified,
                vector_inputs_reenqueued: reenqueued,
            });
        outcome.namespaces += 1;
        outcome.generation_ids.insert(row.generation_id.clone());
    }
    tracing::info!(
        rebuild_id = %manifest.rebuild_id,
        namespaces = outcome.namespaces,
        commit_documents = outcome.commit_documents,
        vectors_verified = outcome.vectors_verified,
        vectors_reenqueued = outcome.vectors_reenqueued,
        "re-emitted commit documents from pinned history generations"
    );
    Ok(Some(outcome))
}

/// Promote the prepared manifest to committed, naming the views the
/// replacement actually produced. Called after the rebuild pass commits.
pub fn commit_prepared_rebuild_manifest(
    index_path: &Path,
    verified_lexical_view: impl Into<String>,
    verified_vector_view: impl Into<String>,
    resulting_catalog_epoch: u64,
    vector_inventory: Vec<RepoHistoryRebuildVectorRowV1>,
) -> Result<Option<RepoHistoryRebuildManifestV1>> {
    let store = HistoryGenerationStore::open_for_index(index_path)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let Some(manifest) = store
        .read_rebuild_manifest()
        .map_err(|error| anyhow::anyhow!("{error}"))?
    else {
        return Ok(None);
    };
    if manifest.state != RepoHistoryRebuildStateV1::Prepared {
        return Ok(Some(manifest));
    }
    let committed = store
        .commit_rebuild_manifest(RepoHistoryRebuildCommittedV1 {
            verified_lexical_view: verified_lexical_view.into(),
            verified_vector_view: verified_vector_view.into(),
            resulting_catalog_epoch,
            vector_inventory,
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(Some(committed))
}

/// What a restart must do about an observed rebuild manifest, classified
/// BEFORE the index is opened and BEFORE any read view binds.
///
/// The ordering is not cosmetic: `classify_rebuild_recovery` reads the schema
/// marker off the index to locate itself relative to the drop, and opening the
/// index rewrites that marker. Classifying after the open would therefore
/// always read the incoming version and mistake a pre-drop crash for a
/// post-drop one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaRebuildResume {
    /// Nothing to do.
    None,
    /// A prepared manifest was rolled back: the outgoing index is intact, so
    /// the ordinary mismatch path will prepare a fresh one.
    RolledBack,
    /// A prepared manifest survives a crash that already dropped the index.
    /// There is no last-good index to return to, so the replacement must be
    /// re-executed from the pinned generations: the caller MUST force a
    /// synchronous full rebuild even though no schema mismatch is detectable
    /// any more (the marker is gone with the index).
    Resume {
        manifest: Box<RepoHistoryRebuildManifestV1>,
    },
    /// Already committed. Left in place; a committed manifest is a GC root.
    AlreadyCommitted,
}

pub fn recover_rebuild_manifest_before_open(index_path: &Path) -> Result<SchemaRebuildResume> {
    let store = HistoryGenerationStore::open_for_index(index_path)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let recovery =
        crate::index::history_materializer::classify_rebuild_recovery(&store, index_path)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    match recovery {
        RepoHistoryRebuildRecoveryV1::NoManifest => Ok(SchemaRebuildResume::None),
        RepoHistoryRebuildRecoveryV1::RollBackPrepared { manifest } => {
            tracing::info!(
                rebuild_id = %manifest.rebuild_id,
                "rolling back a prepared rebuild manifest: the source index is still intact"
            );
            store
                .roll_back_rebuild_manifest()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(SchemaRebuildResume::RolledBack)
        }
        RepoHistoryRebuildRecoveryV1::ResumePrepared { manifest } => {
            tracing::warn!(
                rebuild_id = %manifest.rebuild_id,
                namespaces = manifest.prepared.namespace_inventory.len(),
                "resuming an interrupted index replacement from its pinned history generations"
            );
            Ok(SchemaRebuildResume::Resume {
                manifest: Box::new(manifest),
            })
        }
        RepoHistoryRebuildRecoveryV1::AlreadyCommitted { manifest } => {
            tracing::debug!(
                rebuild_id = %manifest.rebuild_id,
                "observed a committed rebuild manifest"
            );
            Ok(SchemaRebuildResume::AlreadyCommitted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bbox_corpus_core::project_record::{ProjectRecord, ProjectRecordsSnapshot};

    #[derive(Debug)]
    struct StaticRecords(ProjectRecordsSnapshot);

    impl ProjectRecordsProvider for StaticRecords {
        fn records_snapshot(&self) -> ProjectRecordsSnapshot {
            self.0.clone()
        }
    }

    fn record(project_id: &str, repo_id: Option<&str>, aliases: &[&str]) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.into(),
            repo_id: repo_id.map(str::to_string),
            canonical_path: format!("/host/checkouts/{project_id}"),
            registered_at: "2026-07-25T00:00:00Z".into(),
            is_git_repo: repo_id.is_some(),
            languages: Default::default(),
            aliases: aliases.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn snapshot(records: Vec<ProjectRecord>) -> ProjectRecordsSnapshot {
        ProjectRecordsSnapshot::from_bridge_records(records, 1)
    }

    #[test]
    fn bridge_owners_prefer_the_first_alias_and_never_a_path() {
        let provider = StaticRecords(snapshot(vec![record(
            "project-a",
            Some("repo-a"),
            &["alpha"],
        )]));
        let owners = bridge_commit_owners(&provider);
        let owner = owners.get("repo-a").expect("owner for repo-a");
        assert_eq!(owner.project_display, "alpha");
        assert_eq!(owner.project_id.as_deref(), Some("project-a"));
        assert!(
            !owner.project_display.contains('/'),
            "the display value must never be a host path"
        );
    }

    #[test]
    fn bridge_owners_fall_back_to_the_project_id_without_aliases() {
        let provider = StaticRecords(snapshot(vec![record("project-b", Some("repo-b"), &[])]));
        let owners = bridge_commit_owners(&provider);
        assert_eq!(owners["repo-b"].project_display, "project-b");
    }

    #[test]
    fn bridge_owners_pick_the_first_project_id_for_a_shared_repo_id() {
        // Worktrees share one repo_id and commit entity ids carry no project
        // component, so the choice must be deterministic rather than
        // iteration-order dependent.
        let provider = StaticRecords(snapshot(vec![
            record("project-z", Some("repo-shared"), &["zed"]),
            record("project-a", Some("repo-shared"), &["alpha"]),
        ]));
        let owners = bridge_commit_owners(&provider);
        assert_eq!(
            owners["repo-shared"].project_id.as_deref(),
            Some("project-a")
        );
        assert_eq!(owners["repo-shared"].project_display, "alpha");
    }

    #[test]
    fn bridge_owners_skip_non_git_records() {
        let provider = StaticRecords(snapshot(vec![record("project-c", None, &["gamma"])]));
        assert!(bridge_commit_owners(&provider).is_empty());
    }
}
