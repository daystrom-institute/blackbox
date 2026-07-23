use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
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
use crate::projects::{ProjectRecord, ProjectRegistry};
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
    projects: &Arc<parking_lot::RwLock<ProjectRegistry>>,
    checkout_access: &Arc<CheckoutAccessBroker>,
) -> Result<bool> {
    let meta = load_meta(&config.meta_path).unwrap_or_default();
    let leased = super::writer_actor::acquire_project_leases(
        config,
        projects,
        checkout_access,
        super::writer_actor::ProjectLeasePurpose::SpeculativeScan,
    )?;
    let lower = leased
        .iter()
        .map(|project| project.lower())
        .collect::<Vec<_>>();
    let mut files = scan_non_project_source_files(config);
    files.extend(project_files::scan_project_files_with_access(
        config, &lower,
    )?);
    for access in &leased {
        if access.git.is_none() && access.git_denial.is_some() {
            let source_key = super::git_history::git_source_key(&access.project.project_id);
            if let Some(previous) = meta.get(&source_key) {
                files.push((source_key, previous.mtime, previous.size));
            }
        }
    }
    super::writer_actor::revalidate_project_leases(checkout_access, &leased)?;
    let current_paths: std::collections::HashSet<&str> =
        files.iter().map(|(p, _, _)| p.as_str()).collect();
    // Check for new or changed files
    for (path, mtime, size) in &files {
        match meta.get(path.as_str()) {
            Some(prev) if prev.mtime == *mtime && prev.size == *size => continue,
            _ => return Ok(true),
        }
    }
    // Check for deleted files (in meta but not on disk)
    for path in meta.keys() {
        if !current_paths.contains(path.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
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
    writer: &mut IndexWriter,
    drain: &mut dyn FnMut(&mut IndexWriter),
    projects: &Arc<parking_lot::RwLock<ProjectRegistry>>,
    checkout_access: &Arc<CheckoutAccessBroker>,
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
    let leased = super::writer_actor::acquire_project_leases(
        config,
        projects,
        checkout_access,
        super::writer_actor::ProjectLeasePurpose::Reindex,
    )?;
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
    let unavailable_record_projects = leased
        .iter()
        .filter(|access| access.local.is_none())
        .map(|access| access.project.canonical_path.clone())
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
    let project_access = leased
        .iter()
        .map(|project| project.lower())
        .collect::<Vec<_>>();
    let unavailable_local = leased
        .iter()
        .filter(|access| access.local.is_none() && access.local_denial.is_some())
        .map(|access| access.project.project_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let preserved_collected = if full {
        project_files::collect_preserved_collected_documents(index, config, fields)?
    } else {
        project_files::PreservedCollectedDocuments::default()
    };
    let preserved_unavailable = if full {
        project_files::collect_project_documents(index, fields, &unavailable_local)?
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
        load_meta(&config.meta_path)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_path, row)| {
                matches!(
                    &row.source,
                    FileMetaSource::LocalProjectFile { project_id, .. }
                        if unavailable_local.contains(project_id)
                ) || _path
                    .strip_prefix("git:")
                    .is_some_and(|project_id| unavailable_git_ids.contains(project_id))
            })
            .collect()
    } else {
        load_meta(&config.meta_path).unwrap_or_default()
    };

    // 4. Index changed files
    let mut indexed_files = 0u64;
    let mut indexed_docs = 0u64;
    let mut skipped = 0u64;
    let tool_edges = ToolEdgeContext::with_project_access(
        leased
            .iter()
            .filter_map(|access| {
                access.local.as_ref().map(|local| ToolEdgeProjectAccess {
                    project: access.project.clone(),
                    local_root: local.project_root().to_path_buf(),
                    git_root: access
                        .git
                        .as_ref()
                        .map(|git| git.checkout_root().to_path_buf()),
                })
            })
            .collect(),
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
    let active_collected = project_files::active_collected_sources(config)?;
    for path in &stale_paths {
        match meta.get(path).map(|row| row.source.clone()) {
            Some(FileMetaSource::LocalProjectFile { project_id, .. })
                if active_collected.contains_key(&project_id)
                    || unavailable_local.contains(&project_id) => {}
            Some(FileMetaSource::LocalProjectFile { entry_key, .. }) => {
                writer.delete_term(Term::from_field_text(
                    fields.code_source_entry_key,
                    &entry_key,
                ));
            }
            _ => {
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
    project_stats.pending_local_snapshots = project_stats.publication.publish()?;
    tool_edge_publication.publish()?;
    let pending_journal = if project_stats.pending_local_snapshots.is_empty() {
        None
    } else {
        let journal = bbox_edge_sidecar::snapshot::write_pending_local_activation_journal(
            &edges_dir,
            &project_stats.pending_local_snapshots,
        )?;
        for activation in journal.activations() {
            let marker = project_files::local_activation_marker(activation.project_id());
            writer.delete_term(Term::from_field_text(fields.entity_id, &marker));
            let mut document = TantivyDocument::new();
            document.add_text(fields.doc_type, "code_source_activation");
            document.add_text(fields.entity_id, &marker);
            document.add_text(fields.project_id, activation.project_id());
            document.add_text(fields.code_source_generation, journal.commit_token());
            writer.add_document(document)?;
        }
        Some(journal)
    };
    writer.commit()?;
    if let Some(journal) = pending_journal {
        bbox_edge_sidecar::snapshot::activate_pending_local_snapshots(
            &edges_dir,
            journal.activations(),
        )?;
        bbox_edge_sidecar::snapshot::clear_pending_local_activation_journal(&edges_dir)?;
    }
    save_meta(&config.meta_path, &meta)?;
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
    project: &ProjectRecord,
    local_root: &std::path::Path,
    git_root: Option<&std::path::Path>,
    publication_guard: impl FnOnce() -> Result<G>,
) -> Result<usize> {
    let edges_dir =
        bbox_edge_index::edge_index::edges_dir_from_projects_path(&config.projects_path);
    let ctx = ToolEdgeContext::for_project_access(
        ToolEdgeProjectAccess {
            project: project.clone(),
            local_root: local_root.to_path_buf(),
            git_root: git_root.map(std::path::Path::to_path_buf),
        },
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

    let observed_dir = edges_dir.join("observed");
    bbox_edge_index::edge_index::append_edges_dedup(&observed_dir, &project.project_id, &collected)
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
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        assert!(
            needs_reindex(&config, &projects, &broker).unwrap(),
            "new harness session log must trigger needs_reindex"
        );
    }
}
