use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Term};
use tantivy::{Index, IndexWriter, TantivyDocument};

use super::knowledge_docs;
pub use super::passes::*;
use super::project_files;
use super::tool_edges::{ToolEdgeContext, ToolEdgeProjectAccess};
use super::writer_actor::IndexWriterActor;
use super::{FieldHandles, FileMetaSource, ReindexConfig};
use crate::checkout_access::CheckoutAccessBroker;
#[cfg(test)]
use crate::projects::ProjectRegistry;
use bbox_corpus_core::project_record::ProjectRecordsProvider;
use bbox_corpus_index::transcripts::adapters::{TranscriptAdapterRegistry, TranscriptScanTarget};

// At the default 120s interval this is one full refresh per day. Full
// project refreshes rewrite managed derived sidecars and trigger legacy
// sidecar compaction, preventing append-only graph edges from growing forever.
const DEFAULT_BACKGROUND_FULL_REINDEX_EVERY_TICKS: u64 = 720;

fn background_startup_delay(interval: Duration) -> Duration {
    std::env::var("BLACKBOX_REINDEX_STARTUP_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(interval)
}

/// Check if any source files have changed since last index.
/// Returns true if reindexing is needed (cheap — stat only, no I/O on file contents).
pub(super) fn needs_reindex(
    config: &ReindexConfig,
    records_provider: &Arc<dyn ProjectRecordsProvider>,
    checkout_access: &Arc<CheckoutAccessBroker>,
    assignments: Option<&Arc<dyn super::writer_actor::ProducerAssignmentSource>>,
) -> Result<bool> {
    let meta = load_meta(&config.meta_path).unwrap_or_default();
    let plans = super::writer_actor::plan_project_sources(
        config,
        records_provider,
        checkout_access,
        assignments,
        super::writer_actor::ProjectLeasePurpose::SpeculativeScan,
        &meta,
        // The speculative scan never consumes an operator acknowledgement:
        // acknowledgements are scoped to the pass the operator invoked.
        &std::collections::BTreeSet::new(),
    )?;
    let lower = plans
        .iter()
        .filter_map(|plan| plan.lowered())
        .collect::<Vec<_>>();
    let mut files = scan_non_project_source_files(config);
    files.extend(project_files::scan_project_files_with_access(
        config, &lower,
    )?);
    for access in plans.iter().filter_map(|plan| plan.access.as_ref()) {
        if access.git.is_none() && access.git_denial.is_some() {
            let source_key = super::git_history::git_source_key(&access.project.project_id);
            if let Some(previous) = meta.get(&source_key) {
                files.push((source_key, previous.mtime, previous.size));
            }
        }
    }
    super::writer_actor::revalidate_planned_leases(checkout_access, &plans)?;
    let current_paths: std::collections::HashSet<&str> =
        files.iter().map(|(p, _, _)| p.as_str()).collect();
    // Check for new or changed files
    for (path, mtime, size) in &files {
        match meta.get(path.as_str()) {
            Some(prev) if prev.mtime == *mtime && prev.size == *size => continue,
            _ => return Ok(true),
        }
    }
    // Check for deleted files (in meta but not on disk). Rows belonging to a
    // purge-exempt project are skipped: their absence from the scan is
    // expected (the pass does not walk them), so counting them here would
    // schedule a pass on every tick forever for a detached, collected, or
    // empty-root-refused project.
    let exempt = super::writer_actor::purge_exempt_project_ids(&plans);
    for (path, row) in meta.iter() {
        if current_paths.contains(path.as_str()) {
            continue;
        }
        if matches!(
            &row.source,
            FileMetaSource::LocalProjectFile { project_id, .. } if exempt.contains(project_id)
        ) {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// WHY a full rebuild is running, which selects whether the preservation gates
/// apply (Phase 3 milestone P3-E).
///
/// The asymmetry: preservation exists to stop a DESTRUCTIVE pass from losing
/// what the index currently holds, so on an ordinary full rebuild its strict
/// live-count and inventory verification is load-bearing and must stay
/// byte-unchanged. On the rebuild that follows a schema replacement the index
/// was already legitimately emptied - under the pre-replacement guard's
/// authority, which proved the recovery sources first - so there is nothing to
/// preserve and verifying against the emptied index is meaningless. The
/// authority there is re-staging: `index_active_collected_project` rebuilds the
/// collected generation from verified store blobs later in the same pass
/// (through the materialization-migration arm when the suffix is outgoing).
///
/// This is threaded from the caller, never inferred from observed emptiness. An
/// empty index on an ORDINARY pass must still fail the gate: that is precisely
/// the property the gate enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FullRebuildCause {
    #[default]
    Ordinary,
    SchemaMigration,
}

impl FullRebuildCause {
    /// Preservation applies only to an ordinary pass.
    fn preserves_existing_documents(self) -> bool {
        matches!(self, Self::Ordinary)
    }
}

/// Restores the reindex-dirty flag when a *triggered* pass exits before
/// committing (writer lock busy, or any phase/commit error via `?`/panic).
/// Disarmed on a committed pass or a genuine no-op, so a satisfied trigger is
/// not replayed forever. Events arriving *during* a pass re-set the flag
/// independently (after the gate's `swap(false)`) and are therefore preserved.
struct DirtyRestore<'a> {
    flag: &'a AtomicBool,
    armed: bool,
}

impl DirtyRestore<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirtyRestore<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Relaxed);
        }
    }
}

fn tool_edge_project_access(
    config: &ReindexConfig,
    records_provider: &Arc<dyn ProjectRecordsProvider>,
    plans: &[super::writer_actor::ProjectSourcePlan],
    transcript_namespaces: &std::collections::BTreeMap<String, String>,
    mut projects: Vec<ToolEdgeProjectAccess>,
) -> Result<Vec<ToolEdgeProjectAccess>> {
    let needs_collected = plans.iter().any(|plan| {
        records_provider.code_source_locality_governed(&plan.project_id)
            && matches!(
                plan.effective,
                super::writer_actor::EffectiveSource::Collected { .. }
            )
    });
    let collected_store = needs_collected
        .then(|| {
            bbox_code_source_store::CodeSourceStore::open_with_mode(
                &config.code_source_store_path,
                bbox_code_source_store::StoreLimits::default(),
                bbox_code_source_store::RuntimeRecordMode::CatalogV2,
            )
            .map(Arc::new)
        })
        .transpose()?;
    for plan in plans {
        let governed = records_provider.code_source_locality_governed(&plan.project_id);
        if !governed {
            continue;
        }
        let Some(access) = plan.access.as_ref() else {
            continue;
        };
        if access.local.is_some() {
            anyhow::bail!(
                "governed collected project {} retained a LocalProjectWalk lease",
                plan.project_id
            );
        }
        let super::writer_actor::EffectiveSource::Collected { generation } = &plan.effective else {
            anyhow::bail!(
                "governed code-source project {} has no active collected generation",
                plan.project_id
            );
        };
        let store = collected_store
            .as_ref()
            .expect("governed collected plans opened the code-source store");
        let activation = store
            .load_activation_mixed(&plan.project_id)?
            .with_context(|| {
                format!(
                    "governed code-source project {} has no activation",
                    plan.project_id
                )
            })?;
        let bbox_code_source_store::MixedActivationRecord::CurrentV2(activation) = activation
        else {
            anyhow::bail!("governed code-source project has a legacy activation");
        };
        if activation.generation_id != *generation {
            anyhow::bail!("governed code-source plan and activation generation disagree");
        }
        let stored = store.find_generation_mixed(generation)?;
        let bbox_code_source_store::MixedStoredGeneration::CurrentV2(stored) = stored else {
            anyhow::bail!("governed code-source project has a legacy generation");
        };
        activation.validate_against_generation(&stored)?;
        if stored.state != bbox_code_source::GenerationState::Active {
            anyhow::bail!("governed code-source generation is not active");
        }
        let entries = store.load_generation_entries(&activation.published_scope, generation)?;
        let transcript_namespace = transcript_namespaces
            .get(&plan.project_id)
            .context("governed attached project has no transcript namespace")?;
        projects.push(ToolEdgeProjectAccess::collected(
            &plan.project_id,
            std::path::PathBuf::from(transcript_namespace),
            activation.snapshot_id,
            stored.descriptor.head_commit,
            entries,
            Arc::clone(store),
        )?);
    }
    Ok(projects)
}

/// Gate + dispatch for one scheduled reindex tick. The cheap speculative
/// scan runs here on the scheduler thread; the pass itself executes inside
/// the IndexWriterActor (the daemon's only in-process tantivy writer), so
/// the in-process LockBusy/silent-skip class is gone — small ops queued
/// during a pass drain into the pass's own commit.
///
/// `reindex_dirty` lets out-of-band sources (the `.bbox/knowledge` watcher,
/// daemon startup) force one pass even when `needs_reindex` sees no tracked
/// source-file change — repo-owned `.bbox/knowledge` files are deliberately not
/// in the meta-tracked source set, so they can only be picked up this way.
fn scheduled_reindex_tick(
    actor: &IndexWriterActor,
    full: bool,
    reindex_dirty: &AtomicBool,
) -> Result<()> {
    // Speculative scan — cheap, no writer allocation. `dirty` forces a pass
    // (and is consumed here); a failed pass restores it via the guard.
    let dirty = reindex_dirty.swap(false, Ordering::Relaxed);
    let mut dirty_guard = DirtyRestore {
        flag: reindex_dirty,
        armed: dirty,
    };
    if !full && !actor.needs_reindex()? && !dirty {
        dirty_guard.disarm();
        tracing::debug!("auto-reindex: no changes detected");
        return Ok(());
    }
    actor.run_reindex_pass(full, dirty)?;
    // Committed (or a genuine no-op) — the trigger is satisfied; don't replay it.
    dirty_guard.disarm();
    Ok(())
}

/// Execute one reindex pass with a writer owned by the caller (the
/// IndexWriterActor). `drain` is invoked at phase boundaries so small ops
/// queued behind the pass land in the same commit instead of waiting for
/// the next cycle. Returns the human-readable summary line.
// the IndexWriterActor's own pass — the sanctioned single-writer site (concurrency-model §4.3).
#[allow(clippy::disallowed_methods)]
pub(super) fn execute_reindex_pass(
    index: &Index,
    config: &ReindexConfig,
    fields: FieldHandles,
    full: bool,
    dirty: bool,
    cause: FullRebuildCause,
    writer: &mut IndexWriter,
    drain: &mut dyn FnMut(&mut IndexWriter),
    records_provider: &Arc<dyn ProjectRecordsProvider>,
    checkout_access: &Arc<CheckoutAccessBroker>,
    assignments: Option<&Arc<dyn super::writer_actor::ProducerAssignmentSource>>,
    accept_empty_projects: &[String],
) -> Result<String> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let recovery_reader = index.reader()?;
    project_files::recover_pending_local_snapshot_activations(
        &recovery_reader.searcher(),
        fields,
        &edges_dir,
    )?;
    let provisional_documents = if full {
        collect_provisional_documents(index, fields)?
    } else {
        Vec::new()
    };
    let preserved_published_knowledge = collect_scoped_published_knowledge(index, fields)?;
    // Source planning walks the pinned catalog snapshot, not the attached-only
    // compatibility rows (Phase 3 plan section 7 item 1). `prior_meta` is the
    // freshness inventory the H3 empty-scan refusal and the detached
    // preservation arm both verify against, so it is loaded before planning.
    let prior_meta = load_meta(&config.meta_path).unwrap_or_default();
    let accept_empty_projects: std::collections::BTreeSet<String> =
        accept_empty_projects.iter().cloned().collect();
    let plans = super::writer_actor::plan_project_sources(
        config,
        records_provider,
        checkout_access,
        assignments,
        super::writer_actor::ProjectLeasePurpose::Reindex,
        &prior_meta,
        &accept_empty_projects,
    )?;
    let purge_exempt = super::writer_actor::purge_exempt_project_ids(&plans);
    let leased = plans
        .iter()
        .filter_map(|plan| plan.access.as_ref())
        .collect::<Vec<_>>();
    for access in &leased {
        if let Some(error) = &access.local_denial {
            tracing::warn!(
                project_id = %access.project.project_id,
                error_code = %error.split(':').next().unwrap_or("checkout_access_denied"),
                "LocalProjectWalk unavailable; retaining last-good project generation"
            );
        }
        if let Some(error) = &access.git_denial {
            tracing::warn!(
                project_id = %access.project.project_id,
                error_code = %error.split(':').next().unwrap_or("checkout_access_denied"),
                "GitHistory unavailable during project reindex"
            );
        }
        if let Some(error) = &access.publisher_config_denial {
            tracing::warn!(
                project_id = %access.project.project_id,
                error_code = %error.split(':').next().unwrap_or("checkout_access_denied"),
                "PublisherConfigTreeRead unavailable; retaining last-good published knowledge"
            );
        }
        if let Some(error) = &access.knowledge_overlay_denial {
            tracing::warn!(
                project_id = %access.project.project_id,
                error_code = %error.split(':').next().unwrap_or("checkout_access_denied"),
                "KnowledgeGapOverlayRead unavailable; retaining last-good published knowledge"
            );
        }
    }
    let unavailable_record_project_paths = leased
        .iter()
        .filter(|access| access.local.is_none())
        .map(|access| {
            (
                access.project.project_id.clone(),
                access.project.canonical_path.clone(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let unavailable_record_projects = unavailable_record_project_paths
        .values()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let unavailable_git_projects = leased
        .iter()
        .filter(|access| access.git.is_none() && access.git_denial.is_some())
        .map(|access| access.project.canonical_path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let unavailable_git_ids = leased
        .iter()
        .filter(|access| access.git.is_none() && access.git_denial.is_some())
        .map(|access| access.project.project_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let preserved_record_documents = if full {
        collect_unavailable_project_record_documents(index, fields, &unavailable_record_projects)?
    } else {
        Vec::new()
    };
    let preserved_git_documents = if full {
        collect_unavailable_git_documents(index, fields, &unavailable_git_projects)?
    } else {
        Vec::new()
    };
    let project_access = plans
        .iter()
        .filter_map(|plan| plan.lowered())
        .collect::<Vec<_>>();
    let unavailable_local = leased
        .iter()
        .filter(|access| access.local.is_none() && access.local_denial.is_some())
        .map(|access| access.project.project_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    // Preservation is gated on the CAUSE, not on `full` alone (P3-E). See
    // `FullRebuildCause` for the asymmetry; the short version is that this
    // collector's strict live-count verification is the whole point on an
    // ordinary pass and is meaningless against a just-emptied index, where
    // re-staging from store blobs is the authority instead.
    let preserve_existing = full && cause.preserves_existing_documents();
    let preserved_collected = if preserve_existing {
        project_files::collect_preserved_collected_documents(index, config, fields)?
    } else {
        project_files::PreservedCollectedDocuments::default()
    };
    let preserved_unavailable = if full {
        project_files::collect_project_documents(index, fields, &unavailable_local)?
    } else {
        Vec::new()
    };
    // The detached / no-attachment preservation arm (F2 H1). Every purge-exempt
    // project whose last-good source is LOCAL keeps its documents across the
    // rebuild, verified against its own freshness inventory. Collected projects
    // are excluded here (the strict collected arm above owns them) and so are
    // the lease-denied projects the legacy `unavailable_local` arm already
    // preserves, so no document is collected twice.
    let detached_preserved_ids = if preserve_existing {
        plans
            .iter()
            .filter(|plan| !plan.is_local_scanned())
            .filter(|plan| {
                !matches!(
                    plan.effective,
                    super::writer_actor::EffectiveSource::Collected { .. }
                )
            })
            .map(|plan| plan.project_id.clone())
            .filter(|project_id| !unavailable_local.contains(project_id))
            .filter(|project_id| !preserved_collected.project_ids.contains(project_id))
            .collect::<std::collections::BTreeSet<_>>()
    } else {
        std::collections::BTreeSet::new()
    };
    // Paired verify for the local lane. Same gate for the same reason: its
    // authority is the per-project freshness inventory, and the schema
    // replacement discarded those rows along with the documents they described
    // (the `FileMeta` version marker), so there is nothing to verify against.
    let preserved_detached = if preserve_existing {
        project_files::collect_verified_detached_documents(
            index,
            config,
            fields,
            &detached_preserved_ids,
            &prior_meta,
        )?
    } else {
        Vec::new()
    };
    // Reload meta at pass start (a prior pass may have committed since the
    // scheduler's speculative scan).
    let mut meta = if full {
        tracing::info!("auto-reindex: periodic full rebuild requested");
        writer.delete_all_documents()?;
        for document in provisional_documents {
            writer.add_document(document)?;
        }
        for document in &preserved_collected.documents {
            writer.add_document(document.clone())?;
        }
        for document in preserved_unavailable {
            writer.add_document(document)?;
        }
        for document in preserved_detached {
            writer.add_document(document)?;
        }
        for document in preserved_record_documents {
            writer.add_document(document)?;
        }
        for document in preserved_git_documents {
            writer.add_document(document)?;
        }
        // Don't commit yet — let the rebuild work and the trailing
        // writer.commit() atomically commit delete+adds together.
        // If we commit delete now and a later step fails, the index
        // is empty while _meta.json still says sources are current.
        // Freshness rows survive the rebuild for exactly the projects whose
        // documents did: the lease-denied set, the detached-preserved set,
        // and the git-unavailable source keys. Dropping a preserved project's
        // rows would strand its documents with no inventory to verify them
        // against on the next pass.
        prior_meta
            .clone()
            .into_iter()
            .filter(|(_path, row)| {
                matches!(
                    &row.source,
                    FileMetaSource::LocalProjectFile { project_id, .. }
                        if unavailable_local.contains(project_id)
                            || detached_preserved_ids.contains(project_id)
                ) || _path
                    .strip_prefix("git:")
                    .is_some_and(|project_id| unavailable_git_ids.contains(project_id))
            })
            .collect()
    } else {
        prior_meta.clone()
    };

    // 3b. Commit-document re-emission from the pinned history generations
    // (P3-E, plan section 9 item 2). Deliberately here: after the destructive
    // `delete_all_documents` and BEFORE any checkout walk below, so an
    // attachment-less project's history is restored from its immutable
    // generation regardless of whether any checkout turns out to be reachable.
    // A manifest naming an unloadable generation or one whose bytes no longer
    // re-derive their commitment fails the pass here, with the delete not yet
    // committed, so the last-good view survives.
    let history_reemission = if full {
        let owners = project_access
            .iter()
            .filter_map(|access| {
                let repo_id = access.project.and_then(|project| project.repo_id.clone())?;
                Some((
                    repo_id,
                    bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1 {
                        project_id: Some(access.project_id().to_string()),
                        project_display: access.identity.display_name.clone(),
                    },
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        super::schema_rebuild::reemit_prepared_history_generations(
            &index_path_from_config(config),
            writer,
            fields,
            &owners,
        )?
    } else {
        None
    };

    // 4. Index changed files
    let mut indexed_files = 0u64;
    let mut indexed_docs = history_reemission
        .as_ref()
        .map(|outcome| outcome.commit_documents)
        .unwrap_or_default();
    let mut skipped = 0u64;
    let local_tool_edge_access = leased
        .iter()
        .filter_map(|access| {
            // Local tool-edge attribution is constructed only from the live
            // LocalProjectWalk lease retained through publication.
            access.local.as_ref().map(|local| {
                ToolEdgeProjectAccess::local(
                    &access.project.project_id,
                    local.project_root().to_path_buf(),
                    access
                        .git
                        .as_ref()
                        .map(|git| git.checkout_root().to_path_buf()),
                )
            })
        })
        .collect();
    let tool_edges = ToolEdgeContext::with_project_access(
        tool_edge_project_access(
            config,
            records_provider,
            &plans,
            &unavailable_record_project_paths,
            local_tool_edge_access,
        )?,
        edges_dir.clone(),
        !full,
    );

    let transcript_phase = Instant::now();
    index_transcripts_via_adapters(
        config,
        fields,
        writer,
        &mut meta,
        &mut indexed_files,
        &mut indexed_docs,
        &mut skipped,
        &tool_edges,
        !full,
    )?;
    tracing::info!(
        full,
        elapsed_ms = transcript_phase.elapsed().as_millis(),
        indexed_files,
        indexed_docs,
        skipped,
        "auto-reindex: transcript phase complete"
    );

    drain(writer);

    let project_phase = Instant::now();
    let mut project_stats = project_files::index_projects_with_access(
        config,
        &project_access,
        fields,
        &mut *writer,
        &mut meta,
        full,
        &preserved_collected.project_ids,
    )?;
    indexed_files += project_stats.indexed_files;
    indexed_docs += project_stats.indexed_docs;
    skipped += project_stats.skipped;
    tracing::info!(
        full,
        elapsed_ms = project_phase.elapsed().as_millis(),
        indexed_files = project_stats.indexed_files,
        indexed_docs = project_stats.indexed_docs,
        skipped = project_stats.skipped,
        emitted_edges = project_stats.emitted_edges,
        indexed_commits = project_stats.indexed_commits,
        "auto-reindex: project phase complete"
    );
    if !project_stats.migrated_collected_selectors.is_empty() {
        // The pinned selector map was seeded from the pre-flip manifest, so a
        // reader would filter out the freshly staged documents until it is
        // refreshed. The boot path refreshes immediately after this pass; a
        // background pass converges on the next edge-index rebuild.
        tracing::info!(
            migrated = ?project_stats.migrated_collected_selectors,
            "auto-reindex: migrated collected generations to the current materialization version"
        );
    }
    if project_stats.emitted_edges > 0 {
        tracing::debug!(
            emitted_edges = project_stats.emitted_edges,
            indexed_commits = project_stats.indexed_commits,
            call_edges = project_stats.call_edges,
            resolved_call_edges = project_stats.resolved_call_edges,
            "auto-reindex: accumulated project-file edges"
        );
    }

    drain(writer);

    let stores_phase = Instant::now();
    let knowledge_access = leased
        .iter()
        .filter_map(|access| {
            let publisher = access.publisher_config.as_ref()?;
            let overlay = access.knowledge_overlay.as_ref()?;
            let scope = publisher.published_scope()?;
            Some(knowledge_docs::KnowledgeProjectAccess {
                project: &access.project,
                scope,
                publisher_checkout_root: publisher.checkout_root(),
                publisher_project_root: publisher.project_root(),
                knowledge_project_root: overlay.project_root(),
            })
        })
        .collect::<Vec<_>>();
    let publisher_refs_path = config
        .projects_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("publisher-refs.json");
    let current_knowledge_projects = leased
        .iter()
        .map(|access| access.project.canonical_path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let known_current_knowledge_scopes = leased
        .iter()
        .filter_map(|access| {
            access.publisher_config.as_ref().map(|publisher| {
                (
                    access.project.canonical_path.clone(),
                    publisher
                        .published_scope()
                        .map(bbox_knowledge::overlay::published_scope_hash),
                )
            })
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut publisher_ref_publication = knowledge_docs::PublisherRefPublicationBundle::default();
    let knowledge_docs = knowledge_docs::reindex_knowledge_store_with_access(
        &config.knowledge_path,
        &publisher_refs_path,
        &knowledge_access,
        &preserved_published_knowledge,
        &current_knowledge_projects,
        &known_current_knowledge_scopes,
        &mut publisher_ref_publication,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if knowledge_docs > 0 {
        indexed_files += 1;
        indexed_docs += knowledge_docs;
    }
    let thread_docs = super::thread_docs::reindex_threads_store_standalone(
        &config.threads_path,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if thread_docs > 0 {
        indexed_files += 1;
        indexed_docs += thread_docs;
    }
    let record_access = leased
        .iter()
        .filter_map(|access| {
            access
                .local
                .as_ref()
                .map(|local| super::thread_docs::ProjectRecordAccess {
                    project: &access.project,
                    root: local.project_root(),
                })
        })
        .collect::<Vec<_>>();
    let record_docs = super::thread_docs::reindex_project_records_with_access(
        &record_access,
        &config.threads_path,
        fields,
        &mut *writer,
    )?;
    indexed_docs += record_docs;
    let roadmap_docs = super::roadmap_docs::reindex_roadmap_store_standalone(
        &config.roadmap_path,
        fields,
        &mut *writer,
        &mut meta,
    )?;
    if roadmap_docs > 0 {
        indexed_files += 1;
        indexed_docs += roadmap_docs;
    }
    tracing::info!(
        full,
        elapsed_ms = stores_phase.elapsed().as_millis(),
        knowledge_docs,
        thread_docs,
        roadmap_docs,
        "auto-reindex: store-doc phase complete"
    );

    // 4b. Purge documents for deleted source files
    let purge_phase = Instant::now();
    let mut current_files = scan_non_project_source_files(config);
    current_files.extend(project_files::scan_project_files_with_access(
        config,
        &project_access,
    )?);
    for project_id in &unavailable_git_ids {
        let source_key = super::git_history::git_source_key(project_id);
        if let Some(previous) = meta.get(&source_key) {
            current_files.push((source_key, previous.mtime, previous.size));
        }
    }
    let current_paths: std::collections::HashSet<String> =
        current_files.iter().map(|(p, _, _)| p.clone()).collect();
    let mut purged = 0u64;
    let stale_paths: Vec<String> = meta
        .keys()
        .filter(|p| !current_paths.contains(p.as_str()))
        .cloned()
        .collect();
    let collected_project_ids = project_files::active_collected_sources(config)?
        .into_keys()
        .collect::<std::collections::BTreeSet<_>>();
    for path in &stale_paths {
        // F2: exemption is keyed on the pass's own plans, not on the
        // collected-selector map alone. Every non-locally-scanned project
        // keeps its documents; the ones whose last-good source is local also
        // keep the freshness rows the preservation arm verifies against.
        match project_files::classify_stale_meta_row(
            meta.get(path).map(|row| &row.source),
            &purge_exempt,
            &collected_project_ids,
        ) {
            project_files::StalePurgeAction::ExemptRetainRow => continue,
            project_files::StalePurgeAction::ExemptDropRow => {}
            project_files::StalePurgeAction::DeleteProjectEntry(entry_key) => {
                writer.delete_term(Term::from_field_text(
                    fields.code_source_entry_key,
                    &entry_key,
                ));
            }
            project_files::StalePurgeAction::DeleteByPath => {
                writer.delete_term(Term::from_field_text(fields.file_path, path));
            }
        }
        meta.remove(path.as_str());
        purged += 1;
    }
    tracing::info!(
        full,
        elapsed_ms = purge_phase.elapsed().as_millis(),
        current_files = current_files.len(),
        purged,
        "auto-reindex: purge phase complete"
    );

    drain(writer);

    // A dirty-triggered pass must still commit even when no *tracked* source
    // file changed: the knowledge reindex above may have deleted/re-added repo
    // entries (e.g. a deleted `.bbox/knowledge` file) whose delete_term must
    // land. Remember the no-change outcome for the summary, but still flow
    // through the one guarded commit path so drained ops and authority checks
    // can never be bypassed.
    let no_changes = !full
        && indexed_files == 0
        && purged == 0
        && !dirty
        && project_stats.pending_local_snapshots.is_empty()
        && project_stats.publication.is_empty();

    // 5. Commit + atomic meta/edge-view publication. The durable journal is
    // written before the Tantivy commit, and each project gets a marker in
    // that same commit. Startup can therefore tell whether it must finish the
    // manifest switch or discard an uncommitted journal.
    let commit_phase = Instant::now();
    let lease_refs = leased
        .iter()
        .flat_map(|access| {
            [
                access.publisher_config.as_ref(),
                access.knowledge_overlay.as_ref(),
                access.local.as_ref(),
                access.git.as_ref(),
            ]
            .into_iter()
            .flatten()
        })
        .collect::<Vec<_>>();
    let _publication_guard = if lease_refs.is_empty() {
        None
    } else {
        Some(checkout_access.publication_guard_for(lease_refs)?)
    };
    publisher_ref_publication.publish()?;
    let tool_edge_publication = tool_edges.take_publish_bundle();
    let mut project_publication_result = project_stats.publication.publish()?;
    project_stats.pending_local_snapshots =
        project_publication_result.take_pending_local_snapshots();
    let commit_attempt = (|| -> Result<_> {
        tool_edge_publication.publish()?;
        let pending_pins = if project_stats.pending_local_snapshots.is_empty() {
            Vec::new()
        } else {
            // R28F2: pins are per project and each carries its own commit
            // token, so the marker a project's recovery compares against is
            // stamped from that project's own pin.
            let pins = bbox_edge_sidecar::snapshot::write_pending_local_activation_pins(
                &edges_dir,
                &project_stats.pending_local_snapshots,
            )?;
            for pin in &pins {
                let marker = project_files::local_activation_marker(pin.project_id());
                writer.delete_term(Term::from_field_text(fields.entity_id, &marker));
                let mut document = TantivyDocument::new();
                document.add_text(fields.doc_type, "code_source_activation");
                document.add_text(fields.entity_id, &marker);
                document.add_text(fields.project_id, pin.project_id());
                document.add_text(fields.code_source_generation, pin.commit_token());
                writer.add_document(document)?;
            }
            pins
        };
        let prior_payload = index
            .load_metas()
            .context("loading prior index payload before snapshot commit")?
            .payload;
        let current = project_publication_result.pending_commitments();
        let commitments = bbox_edge_sidecar::snapshot::carry_forward_commitments(
            &edges_dir,
            prior_payload.as_deref(),
            &current,
        )?;
        let mut prepared = writer.prepare_commit()?;
        let payload = commitments.join(",");
        if !payload.is_empty() {
            prepared.set_payload(&payload);
        }
        prepared.commit()?;
        Ok((pending_pins, payload))
    })();
    let (pending_pins, commit_payload) = match commit_attempt {
        Ok(result) => result,
        Err(error) => {
            if let Err(cleanup) = project_publication_result.rollback_pending() {
                return Err(error).context(format!(
                    "snapshot commit failed and rollback left unresolved staging: {cleanup:#}"
                ));
            }
            return Err(error);
        }
    };
    project_publication_result.mark_commit_succeeded();
    let pending_handles = project_publication_result.take_pending_snapshot_finalizations();
    // R20F4: fail closed if any finalization fails.
    for handle in &pending_handles {
        if let Err(error) = bbox_edge_sidecar::snapshot::finalize_snapshot_publication(handle) {
            tracing::error!(
                project_id = %handle.project_id,
                snapshot_id = %handle.snapshot_id,
                txn_token = %handle.txn_token,
                error = %error,
                "failed to finalize snapshot publication after reindex commit"
            );
            return Err(error);
        }
    }
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    if !pending_pins.is_empty() {
        let activations = pending_pins
            .iter()
            .map(|pin| pin.activation().clone())
            .collect::<Vec<_>>();
        bbox_edge_sidecar::snapshot::activate_pending_local_snapshots(&edges_dir, &activations)?;
        bbox_edge_sidecar::snapshot::clear_pending_local_activation_pins(&edges_dir)?;
    }
    save_meta(&config.meta_path, &meta)?;
    // The manifest is promoted to COMMITTED only now: the population it
    // promises is durable in the index, so the committed evidence can name the
    // views that actually resulted. A crash before this point leaves the
    // manifest prepared and `classify_rebuild_recovery` resumes it at the next
    // open (the index is post-drop, so there is nothing to roll back to).
    if let Some(outcome) = &history_reemission {
        let committed = super::schema_rebuild::commit_prepared_rebuild_manifest(
            &index_path_from_config(config),
            format!("lexical:{}", bbox_corpus_index::index::INDEX_SCHEMA_VERSION),
            format!("vector:{}", bbox_vectors::VECTOR_SCHEMA_VERSION),
            records_provider.records_snapshot().authority_epoch,
            outcome.vector_inventory.clone(),
        )?;
        if let Some(manifest) = committed {
            tracing::info!(
                rebuild_id = %manifest.rebuild_id,
                namespaces = outcome.namespaces,
                commit_documents = outcome.commit_documents,
                vectors_verified = outcome.vectors_verified,
                vectors_reenqueued = outcome.vectors_reenqueued,
                "committed the repo-history rebuild manifest"
            );
        }
    }
    if full && cause == FullRebuildCause::Ordinary {
        let mode = if records_provider.catalog_authority() {
            bbox_code_source_store::RuntimeRecordMode::CatalogV2
        } else {
            bbox_code_source_store::RuntimeRecordMode::BridgeV1
        };
        let store = bbox_code_source_store::CodeSourceStore::open_with_mode(
            &config.code_source_store_path,
            bbox_code_source_store::StoreLimits::default(),
            mode,
        )?;
        let observations =
            crate::code_source_locality_observations::CodeSourceLocalityObservationsV1::open(
                crate::code_source_locality_observations::observation_path_from_code_source_root(
                    store.root(),
                )?,
            )?;
        observations.record_verified_activations(
            &store,
            &config.projects_path,
            crate::code_source_locality_observations::CodeSourceLocalityEvidenceKindV1::FullRebuild,
        )?;
    }
    tracing::info!(
        full,
        elapsed_ms = commit_phase.elapsed().as_millis(),
        "auto-reindex: commit phase complete"
    );

    if no_changes {
        let summary = "auto-reindex: no changes after re-check".to_string();
        tracing::debug!("{}", summary);
        return Ok(summary);
    }

    let segments = segment_count(index);
    let summary = format!(
        "auto-reindex: indexed {indexed_files} files ({indexed_docs} docs), skipped {skipped} unchanged, purged {purged} deleted, segments {segments}"
    );
    tracing::info!("{}", summary);
    Ok(summary)
}

/// The index directory, derived from the pass config's `_meta.json` path. The
/// history generations root and the commit spill root are both siblings of it,
/// so the derivation has to agree with `TranscriptIndex`'s own or the rebuild
/// would look for its manifest in the wrong family root.
fn index_path_from_config(config: &ReindexConfig) -> std::path::PathBuf {
    config
        .meta_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn collect_provisional_documents(
    index: &Index,
    fields: FieldHandles,
) -> Result<Vec<TantivyDocument>> {
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(fields.knowledge_visibility, "provisional"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    searcher
        .search(&query, &TopDocs::with_limit(count))?
        .into_iter()
        .map(|(_, address)| searcher.doc::<TantivyDocument>(address).map_err(Into::into))
        .collect()
}

fn collect_scoped_published_knowledge(
    index: &Index,
    fields: FieldHandles,
) -> Result<Vec<knowledge_docs::PreservedPublishedKnowledgeDocument>> {
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(fields.knowledge_visibility, "published"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for (_, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
        let document = searcher.doc::<TantivyDocument>(address)?;
        let scope_hash =
            document
                .get_all(fields.knowledge_scope_hash)
                .find_map(|value| match value {
                    tantivy::schema::OwnedValue::Str(value) if !value.is_empty() => {
                        Some(value.clone())
                    }
                    _ => None,
                });
        if let Some(scope_hash) = scope_hash {
            documents.push(knowledge_docs::PreservedPublishedKnowledgeDocument {
                scope_hash,
                project_path: first_document_text(&document, fields.project),
                document,
            });
        }
    }
    Ok(documents)
}

fn collect_unavailable_project_record_documents(
    index: &Index,
    fields: FieldHandles,
    unavailable_projects: &std::collections::BTreeSet<String>,
) -> Result<Vec<TantivyDocument>> {
    if unavailable_projects.is_empty() {
        return Ok(Vec::new());
    }
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(fields.doc_type, "thread"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for (_, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
        let document = searcher.doc::<TantivyDocument>(address)?;
        let project = first_document_text(&document, fields.project);
        let file_path = first_document_text(&document, fields.file_path);
        if unavailable_projects.contains(project.as_str()) && file_path.contains("/.bbox/record/") {
            documents.push(document);
        }
    }
    Ok(documents)
}

fn collect_unavailable_git_documents(
    index: &Index,
    fields: FieldHandles,
    unavailable_projects: &std::collections::BTreeSet<String>,
) -> Result<Vec<TantivyDocument>> {
    if unavailable_projects.is_empty() {
        return Ok(Vec::new());
    }
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = TermQuery::new(
        Term::from_field_text(fields.doc_type, "commit"),
        IndexRecordOption::Basic,
    );
    let count = searcher.search(&query, &Count)?;
    // `TopDocs::with_limit(0)` PANICS. Every sibling collector guards this; this
    // one did not, and the first full rebuild after the P3-E schema reset is
    // exactly the reachable case: the reset leaves a brand-new empty index, so
    // there are zero commit documents while any Git-lease-denied project puts a
    // non-empty set in `unavailable_projects`. The panic killed the writer
    // actor, which surfaced only as "index writer actor dropped the reindex ack".
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for (_, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
        let document = searcher.doc::<TantivyDocument>(address)?;
        if unavailable_projects.contains(first_document_text(&document, fields.project).as_str()) {
            documents.push(document);
        }
    }
    Ok(documents)
}

fn first_document_text(document: &TantivyDocument, field: tantivy::schema::Field) -> String {
    document
        .get_all(field)
        .find_map(|value| match value {
            tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Spawn the background reindex thread. Runs every `interval` seconds.
///
/// `reindex_dirty` is a shared out-of-band trigger: the `.bbox/knowledge`
/// watcher (and daemon startup) set it so repo-owned knowledge changes that
/// `needs_reindex` cannot see still drive one incremental pass.
pub fn spawn_reindex_thread(
    actor: super::writer_actor::IndexWriterActor,
    _config: ReindexConfig,
    interval: Duration,
    reindex_dirty: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("blackbox-reindex".into())
        .spawn(move || {
            tracing::info!(
                "background reindex thread started (interval: {:?})",
                interval
            );
            let full_reindex_every_ticks = std::env::var("BLACKBOX_BACKGROUND_FULL_REINDEX_TICKS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_BACKGROUND_FULL_REINDEX_EVERY_TICKS);
            if full_reindex_every_ticks == 0 {
                tracing::info!("background full reindex disabled");
            } else {
                tracing::info!(
                    ticks = full_reindex_every_ticks,
                    "background full reindex enabled"
                );
            }
            let startup_delay = background_startup_delay(interval);
            tracing::info!(
                delay_secs = startup_delay.as_secs(),
                "background reindex startup delay configured"
            );
            std::thread::sleep(startup_delay);
            let mut tick = 0_u64;
            loop {
                tick = tick.wrapping_add(1);
                let full =
                    full_reindex_every_ticks != 0 && tick.is_multiple_of(full_reindex_every_ticks);
                if let Err(e) = scheduled_reindex_tick(&actor, full, &reindex_dirty) {
                    tracing::error!("background reindex failed: {:#}", e);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn reindex thread");
}

/// Walk all indexed transcripts and retroactively emit observed tool-call edges
/// (EDITED_FILE / READ_FILE / RAN_BASH) for a newly registered project.
///
/// Idempotent: uses `append_edges_dedup` so re-running produces no duplicates.
/// Returns the number of new edges written.
pub fn backfill_tool_edges_for_project<G>(
    config: &ReindexConfig,
    project_id: &str,
    local_root: &std::path::Path,
    git_root: Option<&std::path::Path>,
    publication_guard: impl FnOnce() -> Result<G>,
) -> Result<usize> {
    let edges_dir =
        bbox_edge_index::edge_index::edges_dir_from_projects_path(&config.projects_path);
    let ctx = ToolEdgeContext::for_project_access(
        ToolEdgeProjectAccess::local(
            project_id,
            local_root.to_path_buf(),
            git_root.map(std::path::Path::to_path_buf),
        ),
        edges_dir.clone(),
    );
    let registry = TranscriptAdapterRegistry::from_reindex_config(config);
    let mut collected: Vec<bbox_edge_index::edge_index::Edge> = Vec::new();

    for adapter in registry.adapters() {
        for target in [
            TranscriptScanTarget::Sessions,
            TranscriptScanTarget::History,
        ] {
            let locations = match adapter.scan_locations(target) {
                Ok(locs) => locs,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "backfill: adapter scan failed, skipping"
                    );
                    continue;
                }
            };
            for location in locations {
                let source_label = location.source.label();
                let account = location.account.as_deref().unwrap_or(source_label);
                let snapshot = match adapter.load_snapshot(&location) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                for event in &snapshot.events {
                    let Some(parsed) = event.to_parsed_event() else {
                        continue;
                    };
                    let line_offset = event.raw.byte_offset.unwrap_or(0);
                    let event_idx = event.raw.event_idx.unwrap_or(0);
                    match ctx.build_event_edges(&parsed, account, line_offset, event_idx) {
                        Ok(Some(edge)) => collected.push(edge),
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!(
                                error = %err,
                                "backfill: skipping event edge build error"
                            );
                        }
                    }
                }
            }
        }
    }

    let _publication_guard = publication_guard()?;
    if collected.is_empty() {
        return Ok(0);
    }

    bbox_edge_index::edge_index::append_observed_edges_dedup(&edges_dir, project_id, &collected)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tantivy::schema::Field;

    use super::*;
    use bbox_corpus_core::entity_ref;
    use bbox_corpus_index::transcripts::projection::normalized_to_doc;
    use bbox_corpus_index::transcripts::types::{
        NormalizedTranscriptEvent, RawTranscriptRef, TranscriptStorage,
    };
    use bro_core::Provider;
    use bro_transcript::{MessageRole, ParsedEvent};
    use tantivy::TantivyDocument;

    #[test]
    fn full_rebuild_preserves_only_git_documents_for_denied_projects() {
        let (schema, fields) = crate::index::build_schema();
        let index = Index::create_in_ram(schema);
        crate::index::register_code_tokenizer(&index);
        let mut writer = index.writer(15_000_000).unwrap();
        for (project, entity) in [
            ("/projects/denied", "commit:denied"),
            ("/projects/available", "commit:available"),
        ] {
            let mut document = TantivyDocument::new();
            document.add_text(fields.doc_type, "commit");
            document.add_text(fields.project, project);
            document.add_text(fields.entity_id, entity);
            writer.add_document(document).unwrap();
        }
        writer.commit().unwrap();

        let preserved = collect_unavailable_git_documents(
            &index,
            fields,
            &std::collections::BTreeSet::from(["/projects/denied".to_string()]),
        )
        .unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            first_document_text(&preserved[0], fields.entity_id),
            "commit:denied"
        );
    }

    #[test]
    fn transcript_docs_include_doc_type_and_parser_version() {
        let (_schema, fields) = crate::index::build_schema();
        let parsed = ParsedEvent {
            role: MessageRole::User,
            content: "schema migration smoke".to_string(),
            session_id: "session-1".to_string(),
            timestamp: None,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: None,
            tool_call: None,
        };
        let raw = RawTranscriptRef::jsonl(
            bbox_corpus_index::transcripts::types::TranscriptSource::Harness(Provider::Brodex),
            TranscriptStorage::JsonlFile,
            "/tmp/session.jsonl",
            0,
            0,
            0,
        );
        let normalized = NormalizedTranscriptEvent::from_parsed_event(
            bbox_corpus_index::transcripts::types::TranscriptSource::Harness(Provider::Brodex),
            parsed,
            raw,
        );
        let doc = normalized_to_doc(
            &normalized,
            "codex",
            "/tmp/session.jsonl",
            false,
            "",
            None,
            fields,
        )
        .expect("normalized event is indexable");

        assert_eq!(first_text(&doc, fields.doc_type), "transcript");
        assert_eq!(
            first_text(&doc, fields.parser_version),
            entity_ref::PARSER_VERSION
        );
    }

    fn first_text(doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// End-to-end: a synthetic harness session event log under
    /// `harness_sessions_dir` is discovered by the adapter registry and
    /// indexed into tantivy with role/session/timestamp/project intact.
    #[test]
    fn harness_session_event_log_indexes_into_test_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sessions_dir = root.join("bro-home").join("harness-sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("sess-hx.events.jsonl"),
            concat!(
                r#"{"ts":"2026-06-10T01:00:00.000Z","event":{"type":"harness_milestone","milestone":"session_start","session_id":"sess-hx","transport":"openai-responses","model":"gpt-5.5","cwd":"/repo/hx","provider":"brodex"}}"#,
                "\n",
                r#"{"ts":"2026-06-10T01:00:01.000Z","event":{"type":"user","session_id":"sess-hx","message":{"role":"user","content":[{"type":"text","text":"investigate the flaky test"}]}}}"#,
                "\n",
                r#"{"ts":"2026-06-10T01:00:09.000Z","event":{"type":"assistant","session_id":"sess-hx","message":{"role":"assistant","content":[{"type":"text","text":"the test races the reindex thread"},{"type":"tool_use","id":"t1","name":"shell_run","input":{"command":"cargo test --lib reindex"}}]}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let config = ReindexConfig {
            roots: Vec::new(),
            codex_root: None,
            meta_path: root.join("_meta.json"),
            projects_path: root.join("projects.json"),
            code_source_store_path: root.join("code-sources"),
            code_source_record_mode: bbox_code_source_store::RuntimeRecordMode::BridgeV1,
            knowledge_path: root.join("kb.json"),
            threads_path: root.join("threads.json"),
            roadmap_path: root.join("roadmap.json"),
            harness_sessions_dir: Some(sessions_dir),
            gemini_tmp_root: None,
        };

        let (schema, fields) = crate::index::build_schema();
        let idx_dir = root.join("idx");
        std::fs::create_dir_all(&idx_dir).unwrap();
        let index = tantivy::Index::create_in_dir(&idx_dir, schema).unwrap();
        crate::index::register_code_tokenizer(&index);
        let mut writer = index.writer(50_000_000).unwrap();
        let mut meta = HashMap::new();
        let (mut files, mut docs, mut skipped) = (0u64, 0u64, 0u64);
        let tool_edges = ToolEdgeContext::with_project_access(
            Vec::new(),
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path),
            false,
        );

        index_transcripts_via_adapters(
            &config,
            fields,
            &mut writer,
            &mut meta,
            &mut files,
            &mut docs,
            &mut skipped,
            &tool_edges,
            false,
        )
        .unwrap();
        writer.commit().unwrap();

        assert_eq!(files, 1, "one session log discovered");
        // user + assistant text + tool_use transcript docs, plus the tool_call
        // doc projected from the shell_run tool_use.
        assert_eq!(docs, 4, "indexed docs");

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        let query = tantivy::query::TermQuery::new(
            Term::from_field_text(fields.session_id, "sess-hx"),
            tantivy::schema::IndexRecordOption::Basic,
        );
        let hits = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(10))
            .unwrap();
        assert_eq!(hits.len(), 4);

        let mut saw_user = false;
        for (_score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr).unwrap();
            assert_eq!(first_text(&doc, fields.account), "brodex");
            assert_eq!(first_text(&doc, fields.project), "/repo/hx");
            assert!(first_text(&doc, fields.timestamp).starts_with("2026-06-10T01:00:0"));
            if first_text(&doc, fields.role) == "user"
                && first_text(&doc, fields.doc_type) == "transcript"
            {
                saw_user = true;
                assert_eq!(
                    first_text(&doc, fields.timestamp),
                    "2026-06-10T01:00:01.000Z"
                );
            }
        }
        assert!(saw_user, "user prompt doc present");
    }

    /// Purge contract: every adapter-discovered transcript file must appear in
    /// `scan_all_source_files`, because the purge phase deletes index docs for
    /// any indexed `file_path` absent from that set. Regression for the live
    /// 2026-06-10 18:09 pass where two freshly indexed harness session logs
    /// were purged in the same pass ("purged 2 deleted") and their meta
    /// entries removed, leaving them permanently unindexed.
    #[test]
    fn adapter_sources_are_in_the_purge_scan_set_and_trigger_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let sessions_dir = root.join("bro-home").join("harness-sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let log_path = sessions_dir.join("sess-purge.events.jsonl");
        std::fs::write(
            &log_path,
            concat!(
                r#"{"ts":"2026-06-10T01:00:00.000Z","event":{"type":"harness_milestone","milestone":"session_start","session_id":"sess-purge","transport":"anthropic","model":"glm-5.1","cwd":"/repo/p","provider":"glm"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let config = ReindexConfig {
            roots: Vec::new(),
            codex_root: None,
            meta_path: root.join("_meta.json"),
            projects_path: root.join("projects.json"),
            code_source_store_path: root.join("code-sources"),
            code_source_record_mode: bbox_code_source_store::RuntimeRecordMode::BridgeV1,
            knowledge_path: root.join("kb.json"),
            threads_path: root.join("threads.json"),
            roadmap_path: root.join("roadmap.json"),
            harness_sessions_dir: Some(sessions_dir),
            gemini_tmp_root: None,
        };

        let files = scan_all_source_files(&config);
        let log_path_str = log_path.to_string_lossy().to_string();
        assert!(
            files.iter().any(|(p, _, _)| *p == log_path_str),
            "adapter-owned event log must be in the purge scan set; got {files:?}"
        );

        // The same scan drives change detection: an adapter session unknown to
        // meta must mark the index dirty.
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(&config.projects_path).unwrap(),
        ));
        let records_provider: Arc<dyn ProjectRecordsProvider> =
            Arc::new(crate::projects::BridgeProjectRecordsProvider::new(projects));
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        assert!(
            needs_reindex(&config, &records_provider, &broker, None).unwrap(),
            "new harness session log must trigger needs_reindex"
        );
    }
}
