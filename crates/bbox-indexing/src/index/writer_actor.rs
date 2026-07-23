//! IndexWriterActor — single owner of tantivy writes (concurrency-model §4.3).
//!
//! Every in-process tantivy mutation flows through one queue consumed by one
//! dedicated OS thread. That removes the P3 competing-single-writer class:
//! hot store upserts no longer race the background reindexer for tantivy's
//! single-writer lock (the old paths opened a fresh `IndexWriter` per call on
//! a tokio worker, blocking behind a whole reindex pass while holding the
//! `state.idx` write guard — which stalled searches too).
//!
//! Design deltas from §4.3 as written:
//! - The actor creates its writer **per batch / per pass** (with retry on a
//!   busy cross-process lock) instead of holding it for the daemon lifetime.
//!   Serialization through the queue is what eliminates in-process LockBusy;
//!   releasing between batches keeps the boot-time initial index build and
//!   test helpers working, and frees the writer's arena + threads when idle.
//! - Small ops are fire-and-forget (`enqueue`): callers already treated index
//!   sync as best-effort (warn-and-continue), and the durable stores +
//!   periodic reindex reconcile any op lost to a writer error. Tests needing
//!   read-your-write determinism call [`IndexWriterActor::flush_blocking`].
//! - A reindex pass runs as a job inside the actor and drains queued small
//!   ops at phase boundaries into the same writer, so mid-pass writes land in
//!   the pass's own commit instead of waiting behind it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Term};
use tantivy::{Index, IndexReader, IndexWriter};

use bbox_corpus_core::project_record::ProjectRecord;
use bbox_knowledge::knowledge::KnowledgeEntry;
use bbox_stores::roadmap::RoadmapItem;
use bbox_threads::threads::Thread;

use super::knowledge_docs::KnowledgeIndexDocument;
use super::reindex::{conservative_log_merge_policy, execute_reindex_pass};
use super::{FieldHandles, ReindexConfig, StatsCache, TranscriptIndex};
use crate::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector, ValidatedCheckoutLease,
};
use crate::projects::ProjectRegistry;

/// One queued index mutation.
pub enum IndexWriteOp {
    UpsertKnowledge(Box<KnowledgeEntry>),
    DeleteKnowledge(String),
    ReplaceKnowledge(Vec<KnowledgeIndexDocument>),
    ReplaceKnowledgeLogical {
        logical_ref: String,
        documents: Vec<KnowledgeIndexDocument>,
    },
    ReplaceKnowledgeScope {
        scope_hash: String,
        documents: Vec<KnowledgeIndexDocument>,
    },
    UpsertRoadmap(Box<RoadmapItem>),
    DeleteRoadmap(String),
    UpsertThread(Box<Thread>),
    /// Full thread-store replacement from a point-in-time snapshot.
    UpsertThreadsStore(Vec<Thread>),
    /// Run a reindex pass inside the actor; ack carries the summary line.
    ReindexPass {
        full: bool,
        dirty: bool,
        ack: mpsc::SyncSender<Result<String>>,
    },
    StageCollectedGeneration {
        project: Box<ProjectRecord>,
        descriptor: Box<bbox_code_source::GenerationDescriptor>,
        generation_id: String,
        entries: Vec<bbox_code_source::ManifestEntry>,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
        ack: mpsc::SyncSender<Result<super::project_files::CollectedIndexResult>>,
        release: mpsc::Receiver<()>,
    },
    StageLocalGeneration {
        project: Box<ProjectRecord>,
        scope: Box<bbox_corpus_core::identity::PublishedScope>,
        ack: mpsc::SyncSender<Result<super::project_files::CollectedIndexResult>>,
        release: mpsc::Receiver<()>,
    },
    RetireCodeSelector {
        selector: String,
        ack: mpsc::SyncSender<Result<u64>>,
        release: mpsc::Receiver<()>,
    },
    /// Barrier: acked once every previously-enqueued op has been applied and
    /// committed. Drained-queue semantics, not durability.
    Flush(mpsc::SyncSender<()>),
}

/// Cloneable handle to the writer actor. Lives in `SharedState`.
#[derive(Clone)]
pub struct IndexWriterActor {
    tx: mpsc::Sender<IndexWriteOp>,
    reader: IndexReader,
    post_commit_hook: Arc<parking_lot::RwLock<Option<PostCommitHook>>>,
    checkout_access: Arc<CheckoutAccessBroker>,
    projects: Arc<parking_lot::RwLock<ProjectRegistry>>,
    config: ReindexConfig,
}

type PostCommitHook = Arc<dyn Fn(tantivy::Searcher) + Send + Sync>;

pub struct StagedIndexGeneration {
    result: super::project_files::CollectedIndexResult,
    release: Option<mpsc::SyncSender<()>>,
}

pub struct RetiredCodeSelector {
    pub document_count: u64,
    release: Option<mpsc::SyncSender<()>>,
}

impl Drop for RetiredCodeSelector {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl std::ops::Deref for StagedIndexGeneration {
    type Target = super::project_files::CollectedIndexResult;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

impl Drop for StagedIndexGeneration {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// Everything the actor thread needs to apply ops and publish commits.
struct ActorCtx {
    index: Index,
    fields: FieldHandles,
    config: ReindexConfig,
    reader: IndexReader,
    stats_cache: StatsCache,
    post_commit_hook: Arc<parking_lot::RwLock<Option<PostCommitHook>>>,
    checkout_access: Arc<CheckoutAccessBroker>,
    projects: Arc<parking_lot::RwLock<ProjectRegistry>>,
}

pub(super) struct LeasedProjectAccess {
    pub(super) project: ProjectRecord,
    pub(super) publisher_config: Option<ValidatedCheckoutLease>,
    pub(super) publisher_config_denial: Option<String>,
    pub(super) knowledge_overlay: Option<ValidatedCheckoutLease>,
    pub(super) knowledge_overlay_denial: Option<String>,
    pub(super) local: Option<ValidatedCheckoutLease>,
    pub(super) local_denial: Option<String>,
    pub(super) git: Option<ValidatedCheckoutLease>,
    pub(super) git_denial: Option<String>,
    pub(super) code_local_enabled: bool,
}

impl LeasedProjectAccess {
    pub(super) fn lower(&self) -> super::project_files::ProjectIndexAccess<'_> {
        super::project_files::ProjectIndexAccess {
            project: &self.project,
            local_root: self
                .code_local_enabled
                .then(|| self.local.as_ref())
                .flatten()
                .map(ValidatedCheckoutLease::project_root),
            git_root: self.git.as_ref().map(ValidatedCheckoutLease::checkout_root),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectLeasePurpose {
    SpeculativeScan,
    Reindex,
}

pub(super) fn acquire_project_leases(
    config: &ReindexConfig,
    projects: &Arc<parking_lot::RwLock<ProjectRegistry>>,
    broker: &Arc<CheckoutAccessBroker>,
    purpose: ProjectLeasePurpose,
) -> Result<Vec<LeasedProjectAccess>> {
    let collected = super::project_files::active_collected_sources(config)?;
    let records = projects.read().list();
    records
        .into_iter()
        .map(|project| {
            let publisher_config = broker.acquire(access_request(
                &project,
                None,
                CheckoutAccessKind::PublisherConfigTreeRead,
            ));
            let expected_scope = publisher_config
                .as_ref()
                .ok()
                .and_then(|lease| lease.published_scope().cloned());
            let publisher_config_denial = publisher_config.as_ref().err().map(ToString::to_string);
            let publisher_config = publisher_config.ok();
            let code_local_enabled = !collected.contains_key(&project.project_id);
            let needs_local = code_local_enabled || purpose == ProjectLeasePurpose::Reindex;
            let (local, local_denial) = if !needs_local {
                (None, None)
            } else {
                match broker.acquire(access_request(
                    &project,
                    expected_scope.clone(),
                    CheckoutAccessKind::LocalProjectWalk,
                )) {
                    Ok(lease) => (Some(lease), None),
                    Err(error) => (None, Some(error.to_string())),
                }
            };
            let (git, git_denial) = if project.is_git_repo {
                match broker.acquire(access_request(
                    &project,
                    expected_scope.clone(),
                    CheckoutAccessKind::GitHistory,
                )) {
                    Ok(lease) => (Some(lease), None),
                    Err(error) => (None, Some(error.to_string())),
                }
            } else {
                (None, None)
            };
            let (knowledge_overlay, knowledge_overlay_denial) =
                if purpose == ProjectLeasePurpose::Reindex {
                    match broker.acquire(access_request(
                        &project,
                        expected_scope,
                        CheckoutAccessKind::KnowledgeGapOverlayRead,
                    )) {
                        Ok(lease) => (Some(lease), None),
                        Err(error) => (None, Some(error.to_string())),
                    }
                } else {
                    (None, None)
                };
            Ok(LeasedProjectAccess {
                project,
                publisher_config,
                publisher_config_denial,
                knowledge_overlay,
                knowledge_overlay_denial,
                local,
                local_denial,
                git,
                git_denial,
                code_local_enabled,
            })
        })
        .collect()
}

pub(super) fn revalidate_project_leases(
    broker: &CheckoutAccessBroker,
    projects: &[LeasedProjectAccess],
) -> Result<()> {
    for access in projects {
        if let Some(publisher_config) = &access.publisher_config {
            broker.revalidate(publisher_config).with_context(|| {
                format!(
                    "PublisherConfigTreeRead authority changed for project {}",
                    access.project.project_id
                )
            })?;
        }
        if let Some(knowledge_overlay) = &access.knowledge_overlay {
            broker.revalidate(knowledge_overlay).with_context(|| {
                format!(
                    "KnowledgeGapOverlayRead authority changed for project {}",
                    access.project.project_id
                )
            })?;
        }
        if let Some(local) = &access.local {
            broker.revalidate(local).with_context(|| {
                format!(
                    "LocalProjectWalk authority changed for project {}",
                    access.project.project_id
                )
            })?;
        }
        if let Some(git) = &access.git {
            broker.revalidate(git).with_context(|| {
                format!(
                    "GitHistory authority changed for project {}",
                    access.project.project_id
                )
            })?;
        }
    }
    Ok(())
}

fn access_request(
    project: &ProjectRecord,
    expected_scope: Option<bbox_corpus_core::identity::PublishedScope>,
    kind: CheckoutAccessKind,
) -> CheckoutAccessRequest {
    CheckoutAccessRequest {
        project_id: project.project_id.clone(),
        attachment: CheckoutAttachmentSelector::Selected,
        expected_scope,
        kind,
        intent: CheckoutAccessIntent::Read,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
    }
}

/// Cap on ops folded into one writer/commit cycle. Bounds worst-case batch
/// latency; the queue itself is unbounded (ops are small).
const MAX_BATCH_OPS: usize = 256;
const STAGED_GENERATION_HOLD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

const WRITER_HEAP_SMALL_OPS: usize = 50_000_000;
const WRITER_HEAP_REINDEX: usize = 100_000_000;

impl IndexWriterActor {
    /// Spawn the daemon's single tantivy writer actor for `idx`
    /// (concurrency-model §4.3). All production index mutations and
    /// reindex passes flow through the returned handle.
    ///
    /// Also registers the engine's daemon hooks (embed enqueue + the
    /// manual-rebuild store-document pass): every spawn point is a daemon
    /// boot path, so this is the single place store-side wiring attaches
    /// to the engine.
    #[cfg(test)]
    pub fn spawn_for(idx: &TranscriptIndex) -> Self {
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(&idx.reindex_config().projects_path)
                .expect("opening standalone project registry for index writer"),
        ));
        let checkout_path = idx
            .reindex_config()
            .projects_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("checkout-registry.json");
        let checkouts = Arc::new(parking_lot::RwLock::new(
            crate::checkout_registry::CheckoutRegistry::open(&checkout_path)
                .expect("opening standalone checkout registry for index writer"),
        ));
        let authority =
            crate::checkout_access_v1::V1CheckoutAccessAuthority::new(projects.clone(), checkouts);
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(authority),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        Self::spawn_for_with_checkout_access(idx, projects, broker)
    }

    /// Spawn against the daemon's single shared registry and access broker.
    pub fn spawn_for_with_checkout_access(
        idx: &TranscriptIndex,
        projects: Arc<parking_lot::RwLock<ProjectRegistry>>,
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Self {
        register_index_store_hooks();
        Self::spawn(
            idx.index_handle(),
            idx.field_handles(),
            idx.reindex_config(),
            idx.reader_handle(),
            idx.stats_cache_handle(),
            projects,
            checkout_access,
        )
    }

    pub fn spawn(
        index: Index,
        fields: FieldHandles,
        config: ReindexConfig,
        reader: IndexReader,
        stats_cache: StatsCache,
        projects: Arc<parking_lot::RwLock<ProjectRegistry>>,
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<IndexWriteOp>();
        let post_commit_hook = Arc::new(parking_lot::RwLock::new(None));
        let ctx = ActorCtx {
            index,
            fields,
            config: config.clone(),
            reader: reader.clone(),
            stats_cache,
            post_commit_hook: post_commit_hook.clone(),
            checkout_access: checkout_access.clone(),
            projects: projects.clone(),
        };
        if let Err(err) = std::thread::Builder::new()
            .name("blackbox-index-writer".into())
            .spawn(move || {
                let _scope = bbox_util::util::BlockingScope::enter();
                run_actor(rx, ctx)
            })
        {
            tracing::error!(error = %err, "failed to spawn index writer actor thread");
        }
        Self {
            tx,
            reader,
            post_commit_hook,
            checkout_access,
            projects,
            config,
        }
    }

    /// Publish a fresh searcher after every successful commit. Installing the
    /// hook also publishes the reader's current searcher while holding the
    /// hook write lock, closing the startup race between actor spawn and hook
    /// registration.
    pub fn set_post_commit_searcher_hook(
        &self,
        hook: impl Fn(tantivy::Searcher) + Send + Sync + 'static,
    ) {
        let hook: PostCommitHook = Arc::new(hook);
        let mut installed = self.post_commit_hook.write();
        *installed = Some(hook.clone());
        hook(self.reader.searcher());
    }

    /// Queue a mutation, fire-and-forget. A send failure (actor thread dead)
    /// is logged; the periodic reindex pass reconciles the index from the
    /// durable stores, so a dropped op degrades freshness, not correctness.
    pub fn enqueue(&self, op: IndexWriteOp) {
        if self.tx.send(op).is_err() {
            tracing::error!("index writer actor unavailable; dropping index write op");
        }
    }

    /// Run a reindex pass on the actor thread and wait for its outcome.
    /// Returns the human-readable summary line on commit/no-op.
    pub fn run_reindex_pass(&self, full: bool, dirty: bool) -> Result<String> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::ReindexPass { full, dirty, ack })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the reindex ack"))?
    }

    pub fn needs_reindex(&self) -> Result<bool> {
        super::reindex::needs_reindex(&self.config, &self.projects, &self.checkout_access)
    }

    pub fn stage_collected_generation(
        &self,
        project: ProjectRecord,
        descriptor: bbox_code_source::GenerationDescriptor,
        generation_id: String,
        entries: Vec<bbox_code_source::ManifestEntry>,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
    ) -> Result<StagedIndexGeneration> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::StageCollectedGeneration {
                project: Box::new(project),
                descriptor: Box::new(descriptor),
                generation_id,
                entries,
                store,
                ack,
                release: release_rx,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let result = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the collected-stage ack"))??;
        Ok(StagedIndexGeneration {
            result,
            release: Some(release),
        })
    }

    pub fn stage_local_generation(
        &self,
        project: ProjectRecord,
        scope: bbox_corpus_core::identity::PublishedScope,
    ) -> Result<StagedIndexGeneration> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::StageLocalGeneration {
                project: Box::new(project),
                scope: Box::new(scope),
                ack,
                release: release_rx,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let result = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the local-stage ack"))??;
        Ok(StagedIndexGeneration {
            result,
            release: Some(release),
        })
    }

    pub fn retire_code_selector(&self, selector: String) -> Result<RetiredCodeSelector> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::RetireCodeSelector {
                selector,
                ack,
                release: release_rx,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let document_count = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the selector-retirement ack"))??;
        Ok(RetiredCodeSelector {
            document_count,
            release: Some(release),
        })
    }

    /// Block until every op enqueued before this call has been applied and
    /// committed. Test/shutdown determinism helper.
    pub fn flush_blocking(&self) -> Result<()> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::Flush(ack))
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the flush ack"))
    }
}

fn run_actor(rx: mpsc::Receiver<IndexWriteOp>, ctx: ActorCtx) {
    // A ReindexPass that arrives while a small-op batch is being collected is
    // deferred to its own iteration rather than folded into the batch.
    let mut deferred: Option<IndexWriteOp> = None;
    loop {
        let op = match deferred.take() {
            Some(op) => op,
            None => match rx.recv() {
                Ok(op) => op,
                Err(_) => break, // all senders dropped — daemon shutdown
            },
        };
        match op {
            IndexWriteOp::ReindexPass { full, dirty, ack } => {
                let result = run_pass(&ctx, &rx, full, dirty);
                let _ = ack.send(result);
            }
            IndexWriteOp::StageCollectedGeneration {
                project,
                descriptor,
                generation_id,
                entries,
                store,
                ack,
                release,
            } => {
                let result = run_collected_stage(
                    &ctx,
                    &project,
                    &descriptor,
                    &generation_id,
                    &entries,
                    &store,
                );
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_stage_release(release, "collected generation");
                }
            }
            IndexWriteOp::StageLocalGeneration {
                project,
                scope,
                ack,
                release,
            } => {
                let result = run_local_stage(&ctx, &project, &scope);
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_stage_release(release, "local generation");
                }
            }
            IndexWriteOp::RetireCodeSelector {
                selector,
                ack,
                release,
            } => {
                let result = run_selector_retirement(&ctx, &selector);
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_stage_release(release, "selector retirement");
                }
            }
            first => {
                let mut batch = vec![first];
                while batch.len() < MAX_BATCH_OPS {
                    match rx.try_recv() {
                        Ok(pass @ IndexWriteOp::ReindexPass { .. }) => {
                            deferred = Some(pass);
                            break;
                        }
                        Ok(stage @ IndexWriteOp::StageCollectedGeneration { .. }) => {
                            deferred = Some(stage);
                            break;
                        }
                        Ok(stage @ IndexWriteOp::StageLocalGeneration { .. }) => {
                            deferred = Some(stage);
                            break;
                        }
                        Ok(retirement @ IndexWriteOp::RetireCodeSelector { .. }) => {
                            deferred = Some(retirement);
                            break;
                        }
                        Ok(op) => batch.push(op),
                        Err(_) => break,
                    }
                }
                process_batch(&ctx, batch);
            }
        }
    }
    tracing::info!("index writer actor stopped");
}

fn await_stage_release(release: mpsc::Receiver<()>, operation: &str) {
    if let Err(mpsc::RecvTimeoutError::Timeout) =
        release.recv_timeout(STAGED_GENERATION_HOLD_TIMEOUT)
    {
        tracing::error!(
            operation,
            timeout_secs = STAGED_GENERATION_HOLD_TIMEOUT.as_secs(),
            "index writer stage hold timed out; releasing the writer lane"
        );
    }
}

fn run_collected_stage(
    ctx: &ActorCtx,
    project: &ProjectRecord,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    store: &bbox_code_source_store::CodeSourceStore,
) -> Result<super::project_files::CollectedIndexResult> {
    let mut git_lease = if project.repo_id.is_some() {
        match ctx.checkout_access.acquire(access_request(
            project,
            Some(descriptor.scope.clone()),
            CheckoutAccessKind::GitHistory,
        )) {
            Ok(lease) => {
                if let Err(error) =
                    store.clear_health_failure(&project.project_id, "git_history_unavailable")
                {
                    tracing::warn!(
                        project_id = %project.project_id,
                        error = %error,
                        "failed to clear GitHistory degradation record"
                    );
                }
                Some(lease)
            }
            Err(error) => {
                if let Err(record_error) = store.record_health_failure(
                    &project.project_id,
                    "git_history_unavailable",
                    &format!("GitHistory access unavailable: {}", error.code.as_str()),
                ) {
                    tracing::warn!(
                        project_id = %project.project_id,
                        error = %record_error,
                        "failed to persist GitHistory degradation record"
                    );
                }
                None
            }
        }
    } else {
        None
    };
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let stage_code =
        |writer: &mut IndexWriter,
         publication: &mut super::project_files::ProjectIndexPublicationBundle| {
            super::project_files::stage_collected_project_generation(
                project,
                descriptor,
                generation_id,
                entries,
                ctx.fields,
                writer,
                &edges_dir,
                publication,
                |entry| {
                    let mut file = store.verified_blob_file(&entry.content_sha256, entry.size)?;
                    let mut bytes = Vec::with_capacity(entry.size as usize);
                    std::io::Read::read_to_end(&mut file, &mut bytes)?;
                    Ok(bytes)
                },
            )
        };
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let mut publication = super::project_files::ProjectIndexPublicationBundle::default();
    let mut result = stage_code(&mut writer, &mut publication)?;
    stage_git_current_edges(
        ctx,
        project,
        git_lease
            .as_ref()
            .map(ValidatedCheckoutLease::checkout_root),
        &result,
        &mut writer,
        &edges_dir,
        &mut publication,
    )?;
    if let Some(active_git_lease) = &git_lease
        && let Err(error) = ctx.checkout_access.revalidate(active_git_lease)
    {
        tracing::warn!(
            project_id = %project.project_id,
            error_code = %error.code.as_str(),
            "GitHistory authority changed during collected staging; retrying path-free"
        );
        if let Err(record_error) = store.record_health_failure(
            &project.project_id,
            "git_history_unavailable",
            &format!("GitHistory authority changed: {}", error.code.as_str()),
        ) {
            tracing::warn!(
                project_id = %project.project_id,
                error = %record_error,
                "failed to persist GitHistory degradation record"
            );
        }
        drop(writer);
        drop(publication);
        writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
        writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
        publication = super::project_files::ProjectIndexPublicationBundle::default();
        result = stage_code(&mut writer, &mut publication)?;
        stage_git_current_edges(
            ctx,
            project,
            None,
            &result,
            &mut writer,
            &edges_dir,
            &mut publication,
        )?;
        git_lease = None;
    }
    let _publication_guard = git_lease
        .as_ref()
        .map(|lease| ctx.checkout_access.publication_guard(lease))
        .transpose()?;
    publication.publish()?;
    writer.commit()?;
    post_commit(ctx);
    Ok(result)
}

fn run_local_stage(
    ctx: &ActorCtx,
    project: &ProjectRecord,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> Result<super::project_files::CollectedIndexResult> {
    let local_lease = ctx.checkout_access.acquire(access_request(
        project,
        Some(scope.clone()),
        CheckoutAccessKind::LocalProjectWalk,
    ))?;
    let git_lease = ctx.checkout_access.acquire(access_request(
        project,
        Some(scope.clone()),
        CheckoutAccessKind::GitHistory,
    ))?;
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let mut publication = super::project_files::ProjectIndexPublicationBundle::default();
    let result = super::project_files::stage_local_project_generation(
        project,
        scope,
        local_lease.project_root(),
        git_lease.checkout_root(),
        ctx.fields,
        &mut writer,
        &edges_dir,
        &mut publication,
    )?;
    stage_git_current_edges(
        ctx,
        project,
        Some(git_lease.checkout_root()),
        &result,
        &mut writer,
        &edges_dir,
        &mut publication,
    )?;
    let _publication_guard = ctx
        .checkout_access
        .publication_guard_for([&local_lease, &git_lease])?;
    publication.publish()?;
    writer.commit()?;
    post_commit(ctx);
    Ok(result)
}

fn stage_git_current_edges(
    ctx: &ActorCtx,
    project: &ProjectRecord,
    git_root: Option<&Path>,
    result: &super::project_files::CollectedIndexResult,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    publication: &mut super::project_files::ProjectIndexPublicationBundle,
) -> Result<()> {
    let Some(root) = git_root else {
        publication.stage_snapshot_git_current(
            edges_dir,
            &project.project_id,
            &result.snapshot_id,
            project.repo_id.is_some(),
        );
        return Ok(());
    };
    let git_meta_dir =
        super::git_history::git_meta_dir_from_projects_path(&ctx.config.projects_path);
    let mut meta = HashMap::new();
    let mut git_ctx = super::git_history::GitIndexContext {
        f: ctx.fields,
        writer,
        meta: &mut meta,
        edges_dir,
        git_meta_dir: &git_meta_dir,
        force_full: true,
        publication,
    };
    super::git_history::index_git_history_for_project(
        project,
        root,
        &result.current_chunk_targets,
        &mut git_ctx,
    )?;
    drop(git_ctx);
    publication.stage_snapshot_git_current(
        edges_dir,
        &project.project_id,
        &result.snapshot_id,
        true,
    );
    Ok(())
}

fn run_selector_retirement(ctx: &ActorCtx, selector: &str) -> Result<u64> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
        if manifest
            .workspaces
            .values()
            .any(|entry| entry.code_source_selector.as_deref() == Some(selector))
        {
            anyhow::bail!("code-source selector became active before retirement");
        }
        ctx.reader.reload()?;
        let searcher = ctx.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(ctx.fields.code_source_selector, selector),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        let vectors = bbox_vectors::try_global()
            .context("vector store is still warming up; retry selector retirement shortly")?;
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<tantivy::TantivyDocument>(address)?;
            if let Some(tantivy::schema::OwnedValue::Str(entity_id)) =
                document.get_first(ctx.fields.entity_id)
            {
                vectors.delete_entity_all_routes(entity_id)?;
            }
        }
        let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
        writer.delete_term(Term::from_field_text(
            ctx.fields.code_source_selector,
            selector,
        ));
        writer.commit()?;
        post_commit(ctx);
        Ok(count as u64)
    })
}

/// Apply a batch of small ops under one writer and one commit.
fn process_batch(ctx: &ActorCtx, batch: Vec<IndexWriteOp>) {
    let mut flush_acks: Vec<mpsc::SyncSender<()>> = Vec::new();
    let mut ops: Vec<IndexWriteOp> = Vec::new();
    for op in batch {
        match op {
            IndexWriteOp::Flush(ack) => flush_acks.push(ack),
            other => ops.push(other),
        }
    }

    if !ops.is_empty() {
        match create_writer(&ctx.index, WRITER_HEAP_SMALL_OPS) {
            Ok(mut writer) => {
                writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
                for op in ops {
                    apply_small_op(ctx, &mut writer, op);
                }
                match writer.commit() {
                    Ok(_) => post_commit(ctx),
                    Err(err) => tracing::error!(
                        error = %err,
                        "index writer actor: batch commit failed; reindex pass will reconcile"
                    ),
                }
            }
            Err(err) => tracing::error!(
                error = %err,
                dropped_ops = ops.len(),
                "index writer actor: writer unavailable; dropping batch (reindex pass will reconcile)"
            ),
        }
    }

    for ack in flush_acks {
        let _ = ack.send(());
    }
}

/// Execute a reindex pass with this actor's writer, draining queued small
/// ops into the same writer at phase boundaries so they land in the pass's
/// commit. Concurrent pass requests are refused (callers retry); flushes
/// drained mid-pass are acked after the commit.
fn run_pass(
    ctx: &ActorCtx,
    rx: &mpsc::Receiver<IndexWriteOp>,
    full: bool,
    dirty: bool,
) -> Result<String> {
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    if !full {
        writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    }
    let mut pending_flushes: Vec<mpsc::SyncSender<()>> = Vec::new();
    let outcome = {
        let mut drain = |writer: &mut IndexWriter| {
            while let Ok(op) = rx.try_recv() {
                match op {
                    IndexWriteOp::ReindexPass { ack, .. } => {
                        let _ = ack.send(Err(anyhow!(
                            "a reindex pass is already running; retry after it completes"
                        )));
                    }
                    IndexWriteOp::StageCollectedGeneration { ack, .. } => {
                        let _ = ack.send(Err(anyhow!(
                            "a reindex pass is already running; retry collected activation"
                        )));
                    }
                    IndexWriteOp::StageLocalGeneration { ack, .. } => {
                        let _ = ack.send(Err(anyhow!(
                            "a reindex pass is already running; retry local cutback"
                        )));
                    }
                    IndexWriteOp::RetireCodeSelector { ack, .. } => {
                        let _ = ack.send(Err(anyhow!(
                            "a reindex pass is already running; retry selector retirement"
                        )));
                    }
                    IndexWriteOp::Flush(ack) => pending_flushes.push(ack),
                    small => apply_small_op(ctx, writer, small),
                }
            }
        };
        execute_reindex_pass(
            &ctx.index,
            &ctx.config,
            ctx.fields,
            full,
            dirty,
            &mut writer,
            &mut drain,
            &ctx.projects,
            &ctx.checkout_access,
        )
    };
    if outcome.is_ok() {
        // Full rebuilds settle merge threads before publishing (the old
        // owned-writer path did this between commit and meta save; doing it
        // post-commit here preserves the settled-segments property).
        if full {
            if let Err(err) = writer.wait_merging_threads() {
                tracing::warn!(error = %err, "index writer actor: wait_merging_threads failed");
            }
        }
        post_commit(ctx);
    }
    for ack in pending_flushes {
        let _ = ack.send(());
    }
    outcome
}

fn apply_small_op(ctx: &ActorCtx, writer: &mut IndexWriter, op: IndexWriteOp) {
    let (kind, result): (&str, Result<()>) = match op {
        IndexWriteOp::UpsertKnowledge(entry) => (
            "upsert_knowledge",
            super::knowledge_docs::apply_knowledge_upsert(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &entry,
            ),
        ),
        IndexWriteOp::DeleteKnowledge(id) => (
            "delete_knowledge",
            super::knowledge_docs::apply_knowledge_delete(writer, ctx.fields, &id),
        ),
        IndexWriteOp::ReplaceKnowledge(documents) => (
            "replace_knowledge",
            super::knowledge_docs::apply_knowledge_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &documents,
            ),
        ),
        IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref,
            documents,
        } => (
            "replace_knowledge_logical",
            super::knowledge_docs::apply_knowledge_logical_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &logical_ref,
                &documents,
            ),
        ),
        IndexWriteOp::ReplaceKnowledgeScope {
            scope_hash,
            documents,
        } => (
            "replace_knowledge_scope",
            super::knowledge_docs::apply_knowledge_scope_replace(
                writer,
                ctx.fields,
                knowledge_path(&ctx.config),
                &scope_hash,
                &documents,
            ),
        ),
        IndexWriteOp::UpsertRoadmap(item) => (
            "upsert_roadmap",
            super::roadmap_docs::apply_roadmap_upsert(
                writer,
                ctx.fields,
                &ctx.config.roadmap_path,
                &item,
            ),
        ),
        IndexWriteOp::DeleteRoadmap(id) => (
            "delete_roadmap",
            super::roadmap_docs::apply_roadmap_delete(writer, ctx.fields, &id),
        ),
        IndexWriteOp::UpsertThread(thread) => (
            "upsert_thread",
            super::thread_docs::apply_thread_upsert(
                writer,
                ctx.fields,
                &ctx.config.threads_path,
                &thread,
            ),
        ),
        IndexWriteOp::UpsertThreadsStore(threads) => (
            "upsert_threads_store",
            super::thread_docs::apply_threads_store_upsert(
                writer,
                ctx.fields,
                &ctx.config.threads_path,
                &threads,
            ),
        ),
        IndexWriteOp::ReindexPass { .. }
        | IndexWriteOp::StageCollectedGeneration { .. }
        | IndexWriteOp::StageLocalGeneration { .. }
        | IndexWriteOp::RetireCodeSelector { .. }
        | IndexWriteOp::Flush(_) => {
            debug_assert!(false, "control ops are routed before apply_small_op");
            return;
        }
    };
    if let Err(err) = result {
        tracing::warn!(error = %err, op = kind, "index writer actor: op failed; reindex pass will reconcile");
    }
}

fn knowledge_path(config: &ReindexConfig) -> &Path {
    &config.knowledge_path
}

/// Embed-bootstrap trampoline: the daemon registers
/// `embed_queue::register_index_embed_hooks` here at SharedState
/// construction (same inversion as the threads/notes embed hooks), and
/// every writer-actor spawn fires it. Keeps this module below the embed
/// pipeline in the crate DAG; unregistered means embed enqueue stays a
/// no-op, matching the uninstalled-queue behavior.
static EMBED_BOOTSTRAP: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Register the embed-hook bootstrap. Idempotent; first registration wins.
pub fn register_embed_bootstrap(hook: fn()) {
    let _ = EMBED_BOOTSTRAP.set(hook);
}

/// Wire the store side into the engine's daemon hooks: embed enqueue for
/// project-file/git chunks (via the registered bootstrap), and the
/// knowledge store-document pass for manual rebuilds
/// (`TranscriptIndex::build_index`). Idempotent; called from every
/// writer-actor spawn and directly by store-coupled tests that drive
/// `build_index` without an actor.
pub fn register_index_store_hooks() {
    if let Some(bootstrap) = EMBED_BOOTSTRAP.get() {
        bootstrap();
    }
    super::embed_hook::register_manual_store_pass(manual_knowledge_store_pass);
}

fn manual_knowledge_store_pass(
    config: &ReindexConfig,
    fields: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut std::collections::HashMap<String, super::FileMeta>,
) -> Result<u64> {
    super::knowledge_docs::reindex_knowledge_store_standalone(
        &config.knowledge_path,
        fields,
        writer,
        meta,
    )
}

/// Make the committed segments visible to searches and invalidate the
/// stats TTL cache — the same post-write publication the old inline
/// facade methods performed under the `state.idx` write guard.
fn post_commit(ctx: &ActorCtx) {
    match ctx.reader.reload() {
        Ok(()) => {
            if let Some(hook) = ctx.post_commit_hook.read().clone() {
                hook(ctx.reader.searcher());
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "index writer actor: reader reload failed");
        }
    }
    *ctx.stats_cache.lock() = None;
}

/// Create a writer, retrying briefly when the cross-process lock is busy
/// (e.g. a previous daemon instance still winding down at boot). In-process
/// contention cannot occur — this actor is the only in-process writer.
fn create_writer(index: &Index, heap: usize) -> Result<IndexWriter> {
    let mut delay = Duration::from_millis(100);
    let attempts = 5;
    for attempt in 1..=attempts {
        match index.writer(heap) {
            Ok(w) => return Ok(w),
            Err(tantivy::TantivyError::LockFailure(e, hint)) => {
                if attempt == attempts {
                    return Err(tantivy::TantivyError::LockFailure(e, hint).into());
                }
                tracing::warn!(
                    attempt,
                    "index writer actor: tantivy writer lock busy (cross-process); retrying"
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("retry loop returns on success or final attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{SearchParams, TranscriptIndex};
    use std::process::Command;

    fn test_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: "actor test entry".into(),
            content: content.into(),
            cluster: None,
            variants: Default::default(),
            category: bbox_knowledge::knowledge::Category::Memory,
            scope: bbox_knowledge::knowledge::Scope::Global,
            project: None,
            providers: Vec::new(),
            priority: bbox_knowledge::knowledge::Priority::Standard,
            weight: 100,
            status: bbox_knowledge::knowledge::Status::Active,
            approval: bbox_knowledge::knowledge::Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-06-10T00:00:00Z".into(),
            updated_at: "2026-06-10T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn test_index(dir: &std::path::Path) -> TranscriptIndex {
        TranscriptIndex::open_or_create(
            &dir.join("idx"),
            Vec::new(),
            None,
            dir.join("projects.json"),
            dir.join("kb.json"),
            dir.join("threads.json"),
            dir.join("roadmap.json"),
        )
        .unwrap()
    }

    #[test]
    fn reindex_access_plan_counts_each_local_and_git_lease_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project_root = root.join("project");
        std::fs::create_dir_all(project_root.join(".bbox")).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.test"],
            vec!["config", "user.name", "Test"],
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&project_root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        std::fs::write(
            project_root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-family\"\n",
        )
        .unwrap();
        std::fs::write(project_root.join("README.md"), "fixture\n").unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .args(["add", "."])
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = Command::new("git")
            .arg("-C")
            .arg(&project_root)
            .args(["commit", "-m", "fixture"])
            .output()
            .unwrap();
        assert!(output.status.success());

        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(root.join("projects.json")).unwrap(),
        ));
        projects.write().register_path(&project_root).unwrap();
        let checkouts = Arc::new(parking_lot::RwLock::new(
            crate::checkout_registry::CheckoutRegistry::open(&root.join("checkout-registry.json"))
                .unwrap(),
        ));
        let observations = crate::checkout_access::CheckoutAccessObservations::in_memory();
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access_v1::V1CheckoutAccessAuthority::new(
                projects.clone(),
                checkouts,
            )),
            observations,
        ));
        let index = test_index(&root);
        let leased = acquire_project_leases(
            &index.reindex_config(),
            &projects,
            &broker,
            ProjectLeasePurpose::Reindex,
        )
        .unwrap();
        assert_eq!(leased.len(), 1);
        assert!(leased[0].local.is_some());
        assert!(leased[0].git.is_some());

        let health = broker.health();
        for kind in [
            CheckoutAccessKind::PublisherConfigTreeRead,
            CheckoutAccessKind::LocalProjectWalk,
            CheckoutAccessKind::GitHistory,
            CheckoutAccessKind::KnowledgeGapOverlayRead,
        ] {
            let operation = health
                .operations
                .iter()
                .find(|operation| operation.kind == kind)
                .unwrap();
            assert_eq!(operation.granted, 1, "{} grants", kind.as_str());
            assert_eq!(operation.denied, 0, "{} denials", kind.as_str());
        }
    }

    #[test]
    fn full_reindex_with_denied_local_access_never_reads_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project_root = root.join("remote-shaped-project");
        std::fs::create_dir(&project_root).unwrap();
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(root.join("projects.json")).unwrap(),
        ));
        projects.write().register_path(&project_root).unwrap();
        std::fs::remove_dir(&project_root).unwrap();
        let observations = crate::checkout_access::CheckoutAccessObservations::in_memory();
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            observations,
        ));
        let index = test_index(&root);
        let actor =
            IndexWriterActor::spawn_for_with_checkout_access(&index, projects, broker.clone());

        actor.run_reindex_pass(true, true).unwrap();
        let local = broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .unwrap();
        assert_eq!(local.granted, 0);
        assert_eq!(local.denied, 1);
    }

    #[test]
    fn collected_stage_degrades_git_without_requesting_local_access() {
        use sha2::{Digest, Sha256};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(root.join("projects.json")).unwrap(),
        ));
        let observations = crate::checkout_access::CheckoutAccessObservations::in_memory();
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            observations,
        ));
        let index = test_index(&root);
        let actor =
            IndexWriterActor::spawn_for_with_checkout_access(&index, projects, broker.clone());
        let store = Arc::new(
            bbox_code_source_store::CodeSourceStore::open(
                root.join("code-sources"),
                bbox_code_source_store::StoreLimits::default(),
            )
            .unwrap(),
        );
        let bytes = b"pub fn collected() {}\n";
        let hash = hex::encode(Sha256::digest(bytes));
        let entries = vec![bbox_code_source::ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hash.clone(),
            size: bytes.len() as u64,
        }];
        let head_commit = "b".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: bbox_corpus_core::identity::PublishedScope::try_new("repo-family", ".").unwrap(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: entries.len() as u64,
            logical_bytes: bytes.len() as u64,
        };
        let upload = store.begin_upload("host-a", descriptor.clone()).unwrap();
        store
            .put_manifest_page("host-a", &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest("host-a", &upload.upload_id)
            .unwrap();
        store
            .install_blob(
                "host-a",
                &upload.upload_id,
                &hash,
                bytes.len() as u64,
                &bytes[..],
            )
            .unwrap();
        let generation = store.finalize_upload("host-a", &upload.upload_id).unwrap();
        let project = ProjectRecord {
            project_id: "collected-project".into(),
            repo_id: Some("repo-family".into()),
            canonical_path: "/unavailable/remote/project".into(),
            registered_at: "2026-07-22T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };

        let staged = actor
            .stage_collected_generation(
                project.clone(),
                descriptor,
                generation.generation_id,
                entries,
                store.clone(),
            )
            .unwrap();
        let snapshot_id = staged.snapshot_id.clone();
        drop(staged);

        let health = broker.health();
        let local_attempts = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .map(|operation| operation.granted + operation.denied)
            .unwrap_or(0);
        assert_eq!(local_attempts, 0);
        let git = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::GitHistory)
            .unwrap();
        assert_eq!(git.granted, 0);
        assert_eq!(git.denied, 1);
        assert!(store.health_records().unwrap().iter().any(|record| {
            record.project_id == project.project_id && record.code == "git_history_unavailable"
        }));
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &root.join("projects.json"),
        );
        let git_overlay = bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            &project.project_id,
            &snapshot_id,
        )
        .join("git-current.jsonl");
        assert_eq!(std::fs::read_to_string(git_overlay).unwrap(), "");
    }

    /// Write entries into the on-disk central kb store. Reindex passes
    /// reconcile knowledge docs from this file, so test entries that must
    /// survive a pass need to live here, not just in the index.
    fn persist_kb_entries(kb_path: &std::path::Path, entries: &[KnowledgeEntry]) {
        use bbox_stores::store_persister::StoreSnapshot;
        let mut kb = bbox_knowledge::knowledge::Knowledge::open(kb_path).unwrap();
        for entry in entries {
            kb.upsert_generated(entry.clone()).unwrap();
        }
        let snapshot = kb.snapshot().unwrap();
        bbox_corpus_core::json_store::atomic_write_json_locked(kb_path, &snapshot).unwrap();
    }

    fn search(index: &TranscriptIndex, q: &str) -> String {
        index
            .search(&SearchParams {
                query: q.into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                exclude_self: None,
            })
            .unwrap()
    }

    fn visibility_count(index: &TranscriptIndex, visibility: &str) -> usize {
        use tantivy::collector::Count;
        use tantivy::query::TermQuery;
        use tantivy::schema::{IndexRecordOption, Term};

        let reader = index.index_handle().reader().unwrap();
        let searcher = reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(index.field_handles().knowledge_visibility, visibility),
            IndexRecordOption::Basic,
        );
        searcher.search(&query, &Count).unwrap()
    }

    #[test]
    fn batched_ops_commit_once_and_become_searchable_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // A burst of ops lands in one batch: upserts, a delete of one of
        // them, and a roadmap-style overwrite of the same entity id.
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "aaaa1111",
            "ferrocene compliance baseline",
        ))));
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "bbbb2222",
            "quux transient entry",
        ))));
        actor.enqueue(IndexWriteOp::DeleteKnowledge("bbbb2222".into()));
        actor.flush_blocking().unwrap();

        let hits = search(&index, "ferrocene compliance");
        assert!(hits.contains("ferrocene"), "{hits}");
        let hits = search(&index, "quux transient");
        assert!(
            !hits.contains("quux"),
            "delete in the same batch must win: {hits}"
        );
    }

    #[test]
    fn logical_replace_removes_every_prior_visibility_variant() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let logical_ref = super::super::knowledge_docs::knowledge_entity_id("scope0001");
        let published =
            KnowledgeIndexDocument::published(test_entry("scope0001", "published obsolete marker"));
        let mut provisional = published.clone();
        provisional.entity_id = "provisional_knowledge:scope:checkout:scope0001".into();
        provisional.entry.content = "provisional obsolete marker".into();
        provisional.visibility = "provisional".into();

        actor.enqueue(IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref: logical_ref.clone(),
            documents: vec![published, provisional],
        });
        actor.flush_blocking().unwrap();
        assert!(search(&index, "published obsolete").contains("published"));
        assert!(!search(&index, "provisional obsolete").contains("provisional"));
        assert_eq!(visibility_count(&index, "provisional"), 1);

        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(test_entry(
            "scope0001",
            "published variant update",
        ))));
        actor.flush_blocking().unwrap();
        assert!(search(&index, "published variant update").contains("published"));
        assert_eq!(
            visibility_count(&index, "provisional"),
            1,
            "variant-precise upsert must not delete the provisional document"
        );

        actor.enqueue(IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref,
            documents: vec![KnowledgeIndexDocument::published(test_entry(
                "scope0001",
                "replacement current marker",
            ))],
        });
        actor.flush_blocking().unwrap();

        assert!(!search(&index, "published obsolete").contains("published"));
        assert!(!search(&index, "provisional obsolete").contains("provisional"));
        assert_eq!(visibility_count(&index, "provisional"), 0);
        assert!(search(&index, "replacement current").contains("replacement"));
    }

    #[test]
    fn reindex_passes_preserve_provisional_documents() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let logical_ref = super::super::knowledge_docs::knowledge_entity_id("scope0002");
        let published =
            KnowledgeIndexDocument::published(test_entry("scope0002", "published generation"));
        let mut provisional = published.clone();
        provisional.entity_id = "provisional_knowledge:scope:checkout:scope0002".into();
        provisional.entry.content = "provisional generation".into();
        provisional.visibility = "provisional".into();
        actor.enqueue(IndexWriteOp::ReplaceKnowledgeLogical {
            logical_ref,
            documents: vec![published, provisional],
        });
        actor.flush_blocking().unwrap();
        assert_eq!(visibility_count(&index, "provisional"), 1);

        actor.run_reindex_pass(false, true).unwrap();
        assert_eq!(visibility_count(&index, "provisional"), 1);

        actor.run_reindex_pass(true, true).unwrap();
        assert_eq!(visibility_count(&index, "provisional"), 1);
    }

    #[test]
    fn scope_replace_preserves_globals_and_unrelated_projects() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let mut old_scope = KnowledgeIndexDocument::published(test_entry(
            "scope-a-old",
            "obsolete scope alpha marker",
        ));
        old_scope.scope_hash = Some("scope-alpha".into());
        let mut other_scope =
            KnowledgeIndexDocument::published(test_entry("scope-b", "retained scope beta marker"));
        other_scope.scope_hash = Some("scope-beta".into());
        let global =
            KnowledgeIndexDocument::published(test_entry("global-entry", "retained global marker"));
        actor.enqueue(IndexWriteOp::ReplaceKnowledge(vec![
            old_scope,
            other_scope,
            global,
        ]));
        actor.flush_blocking().unwrap();

        let mut current_scope = KnowledgeIndexDocument::published(test_entry(
            "scope-a-new",
            "current scope alpha marker",
        ));
        current_scope.scope_hash = Some("scope-alpha".into());
        actor.enqueue(IndexWriteOp::ReplaceKnowledgeScope {
            scope_hash: "scope-alpha".into(),
            documents: vec![current_scope],
        });
        actor.flush_blocking().unwrap();

        assert!(!search(&index, "obsolete scope alpha").contains("obsolete"));
        assert!(search(&index, "current scope alpha").contains("current"));
        assert!(search(&index, "retained scope beta").contains("retained"));
        assert!(search(&index, "retained global").contains("retained"));
    }

    #[test]
    fn ops_enqueued_around_a_reindex_pass_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // Both entries are store-backed: a pass reconciles knowledge docs
        // from the kb store file, so index-only entries would be wiped by
        // design.
        let before = test_entry("cccc3333", "zorbl before pass");
        let during = test_entry("dddd4444", "wibble during pass");
        persist_kb_entries(
            &dir.path().join("kb.json"),
            &[before.clone(), during.clone()],
        );
        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(before)));

        // Run a pass while a second thread races an op into the queue.
        // Whether it drains mid-pass or batches after, flush_blocking must
        // not return until it is committed.
        let actor2 = actor.clone();
        let racer = std::thread::spawn(move || {
            actor2.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(during)));
        });
        let summary = actor.run_reindex_pass(false, true).unwrap();
        assert!(summary.starts_with("auto-reindex:"), "{summary}");
        racer.join().unwrap();
        actor.flush_blocking().unwrap();

        let hits = search(&index, "zorbl");
        assert!(hits.contains("zorbl"), "{hits}");
        let hits = search(&index, "wibble");
        assert!(hits.contains("wibble"), "{hits}");
    }

    #[test]
    fn full_pass_rebuilds_and_keeps_store_docs() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);

        // Store-backed entry: write it to the kb store file so the pass's
        // store-doc phase re-adds it after delete_all_documents.
        let kb_path = dir.path().join("kb.json");
        let entry = test_entry("eeee5555", "florp durable entry");
        persist_kb_entries(&kb_path, &[entry.clone()]);

        actor.enqueue(IndexWriteOp::UpsertKnowledge(Box::new(entry)));
        actor.flush_blocking().unwrap();
        assert!(search(&index, "florp").contains("florp"));

        let summary = actor.run_reindex_pass(true, true).unwrap();
        assert!(summary.starts_with("auto-reindex:"), "{summary}");
        assert!(
            search(&index, "florp").contains("florp"),
            "full rebuild must re-add store-backed docs"
        );
    }
}
