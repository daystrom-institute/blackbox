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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::{IndexRecordOption, Term};
use tantivy::{Index, IndexReader, IndexWriter};

use bbox_corpus_core::code_project_identity::{CodeProjectIdentity, IdentityOrigin};
use bbox_corpus_core::project_catalog::ProjectScope;
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_knowledge::knowledge::KnowledgeEntry;
use bbox_stores::roadmap::RoadmapItem;
use bbox_threads::threads::Thread;

use super::knowledge_docs::KnowledgeIndexDocument;
use super::reindex::{FullRebuildCause, conservative_log_merge_policy, execute_reindex_pass};
use super::{FieldHandles, ReindexConfig, StatsCache, TranscriptIndex};
use crate::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector, ValidatedCheckoutLease,
};
#[cfg(test)]
use crate::projects::ProjectRegistry;
use bbox_corpus_core::project_record::ProjectRecordsProvider;

#[derive(Debug)]
pub enum IndexWriterRetryableError {
    ReindexPassInProgress,
    EdgeIndexRebuildInProgress,
    VectorStoreWarming,
}

impl std::fmt::Display for IndexWriterRetryableError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReindexPassInProgress => "an index reindex pass is already running",
            Self::EdgeIndexRebuildInProgress => {
                "an edge-index rebuild is already reading the sidecar publication"
            }
            Self::VectorStoreWarming => "the vector store is still warming up",
        })
    }
}

impl std::error::Error for IndexWriterRetryableError {}

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
        /// WHY this pass is running. Never inferred: see
        /// [`FullRebuildCause`] for the asymmetry it selects.
        cause: FullRebuildCause,
        /// Operator-named projects whose H3 empty-scan refusal is waived for
        /// THIS pass (Phase 3 plan section 7 item 2). Operator authority,
        /// passed through and never defaulted (RX-V1): no code path may add
        /// a project here on the operator's behalf.
        accept_empty_projects: Vec<String>,
        /// Synchronous internal callers wait for the pass result. Interactive
        /// tool requests omit the ack and receive admission immediately.
        ack: Option<mpsc::SyncSender<Result<String>>>,
    },
    /// Stage one collected generation. Identity-first (Phase 3 plan section 6
    /// item 1): the op carries no checkout path and the handler opens no Git,
    /// so a collected generation can be staged for a project with zero
    /// attachments.
    StageCollectedGeneration {
        identity: Box<CodeProjectIdentity>,
        descriptor: Box<bbox_code_source::GenerationDescriptor>,
        generation_id: String,
        entries: Vec<bbox_code_source::ManifestEntry>,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
        ack: mpsc::SyncSender<Result<super::project_files::CollectedIndexResult>>,
        release: mpsc::Receiver<()>,
        hold_state: Arc<AtomicU8>,
    },
    /// Stage one local generation by walking the leased checkout.
    ///
    /// `scope` is the authorized producer scope the caller acted on. For a
    /// `Catalog` identity it must equal the identity's own published scope
    /// (the handler refuses a mismatch); for a `Bridge` identity it is the
    /// only `PublishedScope` in existence, because a bridge record carries
    /// none and D-034 forbids fabricating one.
    StageLocalGeneration {
        identity: Box<CodeProjectIdentity>,
        scope: Box<bbox_corpus_core::identity::PublishedScope>,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
        ack: mpsc::SyncSender<Result<super::project_files::CollectedIndexResult>>,
        release: mpsc::Receiver<()>,
        hold_state: Arc<AtomicU8>,
    },
    /// Post-activation Git current-file overlay (Phase 3 plan section 6
    /// item 3). Runs AFTER a collected generation is already active, never
    /// inside its transaction: the daemon acquires the `GitHistory` lease,
    /// hands it here for the walk, and treats every failure as best effort.
    StageGitCurrentOverlay {
        project: Box<ProjectRecord>,
        /// The identity's display name. P3-E: the commit documents this walk
        /// emits carry it in `project` instead of the checkout path, so it must
        /// travel with the op rather than be re-derived from the compat record
        /// (whose alias set is not the catalog display name).
        project_display: String,
        lease: Box<ValidatedCheckoutLease>,
        snapshot_id: String,
        current_chunk_targets: HashMap<String, bbox_corpus_core::entity_ref::EntityRef>,
        ack: mpsc::SyncSender<Result<()>>,
    },
    /// Consolidated repo-history ingestion for ONE repo-history record
    /// (Phase 3 plan section 10 item 2), catalog mode only.
    ///
    /// The walk itself happened on the daemon side, off this actor: it needs
    /// the catalog snapshot and the attachment ladder, neither of which this
    /// actor holds, and it must not run while a staged-generation hold is
    /// alive. What arrives here is the already-decided result - the commits
    /// to write under the primary namespace and the per-project edges to
    /// materialize - so this op does index work only.
    StageConsolidatedHistory {
        commit_namespace: String,
        /// Display name for the `project` field of every commit document in
        /// this repository. Repo-level facts, one single-valued schema field:
        /// the daemon picks the deterministic member and it travels with the
        /// op rather than being re-derived here.
        project_display: String,
        commits: Vec<bbox_corpus_core::git::GitCommit>,
        /// member project id -> that project's managed Git sidecar edges.
        edges_by_project: std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
        /// member project id -> the active snapshot whose `git-current.jsonl`
        /// member must receive that project's edges. A member absent here
        /// keeps its managed sidecar refreshed without a snapshot member,
        /// which is the right shape for a sibling whose own generation has
        /// not activated in this pass.
        snapshot_by_project: std::collections::BTreeMap<String, String>,
        ack: mpsc::SyncSender<Result<u64>>,
    },
    /// Publish the exact document/vector inventory carried by one verified
    /// immutable P3 generation, plus its per-project current-file edges.
    PublishHistoryGeneration {
        generation: Box<bbox_corpus_index::index::history_generations::HistoryGenerationRecordV1>,
        owner: Box<bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1>,
        edges_by_project: std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
        snapshot_by_project: std::collections::BTreeMap<String, String>,
        ack: mpsc::SyncSender<Result<HistoryPublicationResultV1>>,
    },
    RetireCodeSelector {
        selector: String,
        ack: mpsc::SyncSender<Result<u64>>,
        release: mpsc::Receiver<()>,
        hold_state: Arc<AtomicU8>,
    },
    /// Barrier: acked once every previously-enqueued op has been applied and
    /// committed. Drained-queue semantics, not durability.
    Flush(mpsc::SyncSender<()>),
}

/// Cloneable handle to the writer actor. Lives in `SharedState`.
#[derive(Clone)]
pub struct IndexWriterActor {
    tx: mpsc::Sender<IndexWriteOp>,
    publication_activity: Arc<AtomicU8>,
    reader: IndexReader,
    fields: FieldHandles,
    post_commit_hook: Arc<parking_lot::RwLock<Option<PostCommitHook>>>,
    checkout_access: Arc<CheckoutAccessBroker>,
    records_provider: Arc<dyn ProjectRecordsProvider>,
    assignments: Arc<parking_lot::RwLock<Option<Arc<dyn ProducerAssignmentSource>>>>,
    config: ReindexConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPublicationResultV1 {
    pub commit_document_count: u64,
    pub commit_view_commitment: String,
    pub overlay_file_commitments: std::collections::BTreeMap<String, String>,
}

type PostCommitHook = Arc<dyn Fn(tantivy::Searcher) + Send + Sync>;

pub struct StagedIndexGeneration {
    result: super::project_files::CollectedIndexResult,
    release: Option<mpsc::SyncSender<()>>,
    hold_state: Arc<AtomicU8>,
}

pub struct RetiredCodeSelector {
    pub document_count: u64,
    release: Option<mpsc::SyncSender<()>>,
    hold_state: Arc<AtomicU8>,
}

impl Drop for RetiredCodeSelector {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl RetiredCodeSelector {
    /// Convert the bounded retirement hold into a cleanup hold. Cleanup must
    /// fail closed if the actor released the writer lane before this call.
    pub fn begin_cleanup(&self) -> Result<()> {
        begin_generation_publication(&self.hold_state)
    }
}

impl std::ops::Deref for StagedIndexGeneration {
    type Target = super::project_files::CollectedIndexResult;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

impl StagedIndexGeneration {
    /// Convert the bounded staging hold into an activation hold. Once this
    /// succeeds, the actor cannot release its writer lane on timeout until
    /// this staged value is dropped.
    pub fn begin_publication(&self) -> Result<()> {
        begin_generation_publication(&self.hold_state)
    }
}

fn begin_generation_publication(hold_state: &AtomicU8) -> Result<()> {
    hold_state
        .compare_exchange(
            STAGE_HOLD_HELD,
            STAGE_HOLD_PUBLISHING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|state| match state {
            STAGE_HOLD_EXPIRED => anyhow!("staged generation hold expired before publication"),
            STAGE_HOLD_PUBLISHING => anyhow!("staged generation publication already began"),
            _ => anyhow!("staged generation hold is no longer available"),
        })
}

impl Drop for StagedIndexGeneration {
    fn drop(&mut self) {
        self.hold_state
            .store(STAGE_HOLD_RELEASED, Ordering::Release);
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
    records_provider: Arc<dyn ProjectRecordsProvider>,
    assignments: Arc<parking_lot::RwLock<Option<Arc<dyn ProducerAssignmentSource>>>>,
    publication_activity: Arc<AtomicU8>,
}

const PUBLICATION_IDLE: u8 = 0;
const PUBLICATION_REINDEX: u8 = 1;
const PUBLICATION_EDGE_REBUILD: u8 = 2;

struct ReindexActivityGuard(Arc<AtomicU8>);

impl Drop for ReindexActivityGuard {
    fn drop(&mut self) {
        self.0.store(PUBLICATION_IDLE, Ordering::Release);
    }
}

/// Admission guard for one complete edge-sidecar parse and graph publication.
/// A reindex request admitted while the parse was already running used to
/// mutate the same sidecars underneath it, forcing a multi-minute retry and
/// doubling peak memory. Holding this guard makes that race impossible; an
/// interactive reindex request fails fast and the periodic pass retries on
/// its next tick.
pub struct EdgeIndexRebuildActivityGuard(Arc<AtomicU8>);

impl Drop for EdgeIndexRebuildActivityGuard {
    fn drop(&mut self) {
        self.0.store(PUBLICATION_IDLE, Ordering::Release);
    }
}

fn commit_snapshot_publications(
    index: &Index,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    mut publication: super::project_files::PublicationResult,
) -> Result<(super::project_files::PublicationResult, String)> {
    let attempt = (|| -> Result<String> {
        let prior_payload = index
            .load_metas()
            .context("loading prior index payload before snapshot commit")?
            .payload;
        let current = publication.pending_commitments();
        let commitments = bbox_edge_sidecar::snapshot::carry_forward_commitments(
            edges_dir,
            prior_payload.as_deref(),
            &current,
        )?;
        let mut prepared = writer.prepare_commit()?;
        let payload = commitments.join(",");
        if !payload.is_empty() {
            prepared.set_payload(&payload);
        }
        prepared.commit()?;
        Ok(payload)
    })();

    let payload = match attempt {
        Ok(payload) => payload,
        Err(error) => {
            if let Err(cleanup) = publication.rollback_pending() {
                return Err(error).context(format!(
                    "snapshot commit failed and rollback also failed: {cleanup:#}"
                ));
            }
            return Err(error);
        }
    };
    publication.mark_commit_succeeded();
    Ok((publication, payload))
}

/// Derived effective source for one catalog project at planning time
/// (Phase 3 plan section 4.7). There is no persisted effective-source store:
/// this is computed per pass from the pinned catalog snapshot, the edge
/// manifest, the activation record, and the producer assignment table, and
/// the edge-sidecar workspace entry stays the single durable authority for
/// the live selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EffectiveSource {
    /// An active collected generation serves this project. Planned with no
    /// local walk and no lease requirement: a project with zero attachments
    /// reaches this arm and stays fully indexed.
    Collected { generation: String },
    /// A validated local checkout is the live source; walked as today.
    Local,
    /// A cutback to local is in flight. Pass-level no-op: the pass neither
    /// walks nor purges, so the in-flight transition owns the transition.
    CutbackPending,
    /// A producer assignment exists but no collected generation is active
    /// yet (first upload in flight). Warming preserves local freshness: with
    /// a usable local source this plans exactly as `Local`; with none (the
    /// remote-only warming case) it is a pass-level no-op with NO durable
    /// health record, because nothing is wrong.
    Warming,
    /// No usable source this pass. Carries the reason and gets a durable
    /// health record instead of a per-pass warning.
    Unavailable { reason: UnavailableReason },
}

/// Why a project has no usable source this pass. Each arm maps to a durable
/// health code so doctor can render the state (the sweep's "unavailable" and
/// "unavailable-no-attachment" rows), replacing the per-pass `tracing::warn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UnavailableReason {
    /// A compatibility record exists but the `LocalProjectWalk` lease was
    /// denied. Diagnostic is the broker denial string.
    LocalWalkDenied(String),
    /// The catalog project has no attached checkout and no collected
    /// generation: the state H1/H2 previously expressed as silent deletion.
    NoAttachment,
    /// The scan of an attached, readable root returned zero entries while
    /// the prior pass recorded files for this project (H3). Refusing is the
    /// only safe reading: an empty automount is indistinguishable from a
    /// deleted project by scan alone.
    EmptyRootRefused,
    /// Planning could not derive an identity for a catalog project id.
    IdentityUnavailable(String),
}

impl UnavailableReason {
    /// Durable health code for this state. `empty_root_refused` is the
    /// operator-acknowledgeable one (`bbox_reindex accept_empty_projects`).
    pub(super) fn health_code(&self) -> &'static str {
        match self {
            Self::EmptyRootRefused => "empty_root_refused",
            _ => "source_unavailable",
        }
    }

    pub(super) fn diagnostic(&self) -> String {
        match self {
            Self::LocalWalkDenied(error) => {
                format!("LocalProjectWalk unavailable: {error}")
            }
            Self::NoAttachment => {
                "project has no attached checkout and no active collected generation".to_string()
            }
            Self::EmptyRootRefused => {
                "local scan returned zero entries while the prior pass recorded files; \
                 purge refused"
                    .to_string()
            }
            Self::IdentityUnavailable(error) => {
                format!("no code identity could be derived: {error}")
            }
        }
    }
}

/// Every durable health code source planning owns. Used to clear stale
/// records for projects that left the state, so a resolved condition does
/// not linger in doctor forever.
pub(super) const PLANNING_HEALTH_CODES: [&str; 2] = ["source_unavailable", "empty_root_refused"];

/// One project's planned source for one pass.
///
/// `access` is present exactly when the project has a compatibility record
/// this pass; leases are a property of ATTACHMENT, not of effective source,
/// so an attached collected project keeps today's lease bundle (its Git
/// history walk and repo-owned knowledge lanes are bridge-observable and
/// stay at parity) while a detached or remote-only project acquires nothing
/// at all. That is what makes the remote-only exit-gate row zero-lease: the
/// broker is never called for a project with no record.
pub(super) struct ProjectSourcePlan {
    pub(super) project_id: String,
    pub(super) identity: Option<CodeProjectIdentity>,
    pub(super) effective: EffectiveSource,
    pub(super) access: Option<LeasedProjectAccess>,
}

impl ProjectSourcePlan {
    /// True when this pass walks the project's local checkout. This is the
    /// single predicate the purge exemptions key on: every project that is
    /// NOT locally scanned this pass keeps its last-good documents (F2).
    pub(super) fn is_local_scanned(&self) -> bool {
        match &self.effective {
            EffectiveSource::Local => true,
            // Warming plans as Local exactly when a valid local source
            // exists; the remote-only warming arm has no access at all.
            EffectiveSource::Warming => self
                .access
                .as_ref()
                .is_some_and(|access| access.local.is_some()),
            EffectiveSource::Collected { .. }
            | EffectiveSource::CutbackPending
            | EffectiveSource::Unavailable { .. } => false,
        }
    }

    /// Lower this plan into the crate-boundary access shape.
    ///
    /// `None` for a plan the pass must not touch at all: a `CutbackPending`
    /// project (the in-flight transition owns it) or a project with no
    /// derivable identity. Every other plan lowers, INCLUDING one with no
    /// compatibility record, which is how a remote-only collected project
    /// reaches the indexer at all. `local_root` is populated only when the
    /// plan is actually locally scanned this pass, so an exempt project can
    /// never be walked (and therefore never scanned as empty) by accident.
    pub(super) fn lowered(&self) -> Option<super::project_files::ProjectIndexAccess<'_>> {
        if matches!(self.effective, EffectiveSource::CutbackPending) {
            return None;
        }
        let identity = self.identity.as_ref()?;
        let scanned = self.is_local_scanned();
        Some(super::project_files::ProjectIndexAccess {
            identity,
            project: self.access.as_ref().map(|access| &access.project),
            local_root: self
                .access
                .as_ref()
                .filter(|_| scanned)
                .and_then(|access| access.local.as_ref())
                .map(ValidatedCheckoutLease::project_root),
            git_root: self
                .access
                .as_ref()
                .and_then(|access| access.git.as_ref())
                .map(ValidatedCheckoutLease::checkout_root),
        })
    }
}

/// Producer assignment view for source planning (Phase 3 plan section 4.7).
/// The grant table lives in the daemon's code-source runtime, above this
/// crate; the actor receives it as an injected read so a `Warming` project
/// is classified from real assignment state instead of guessed from store
/// residue. Absent (bridge, tests, offline) means no assignments exist.
pub trait ProducerAssignmentSource: Send + Sync {
    /// Project ids with a configured collector assignment.
    fn assigned_project_ids(&self) -> std::collections::BTreeSet<String>;
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
}

/// Every project the stale-path purge must exempt this pass: exactly the
/// projects that are not locally scanned (Phase 3 plan section 7 item 2).
/// A missing scanned path proves a file was deleted only when the pass
/// actually walked that project's checkout.
pub(super) fn purge_exempt_project_ids(
    plans: &[ProjectSourcePlan],
) -> std::collections::BTreeSet<String> {
    plans
        .iter()
        .filter(|plan| !plan.is_local_scanned())
        .map(|plan| plan.project_id.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectLeasePurpose {
    SpeculativeScan,
    Reindex,
}

/// Walk the pinned catalog snapshot and plan one source per corpus project
/// (Phase 3 plan section 7 item 1). Supersedes `acquire_project_leases`,
/// which iterated the attached-only compatibility rows and therefore never
/// visited a remote-only project (F1) and never gave the purge a name for
/// the states it was silently deleting (F2).
///
/// `accept_empty_projects` is operator authority passed straight through
/// (RX-V1): a named project skips the H3 empty-scan refusal on THIS pass and
/// has its `empty_root_refused` record cleared. Nothing in this function may
/// add a project to that set on its own.
pub(super) fn plan_project_sources(
    config: &ReindexConfig,
    records_provider: &Arc<dyn ProjectRecordsProvider>,
    broker: &Arc<CheckoutAccessBroker>,
    assignments: Option<&Arc<dyn ProducerAssignmentSource>>,
    purpose: ProjectLeasePurpose,
    prior_meta: &HashMap<String, super::FileMeta>,
    accept_empty_projects: &std::collections::BTreeSet<String>,
) -> Result<Vec<ProjectSourcePlan>> {
    let collected = super::project_files::active_collected_sources(config)?;
    let snapshot = records_provider.records_snapshot();
    let identities = records_provider.code_identities();
    let records = snapshot
        .records
        .iter()
        .map(|record| (record.project_id.clone(), record.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let assigned = assignments
        .map(|source| source.assigned_project_ids())
        .unwrap_or_default();
    // Health writes are best-effort per project: a store that cannot be
    // opened must degrade planning to today's warn-only behavior, never fail
    // the pass.
    let health_store = bbox_code_source_store::CodeSourceStore::open(
        &config.code_source_store_path,
        bbox_code_source_store::StoreLimits::default(),
    )
    .map_err(|error| {
        tracing::warn!(%error, "code-source store unavailable; planning health records skipped");
    })
    .ok();
    let prior_meta_counts = prior_meta_project_file_counts(prior_meta);

    let mut plans = Vec::with_capacity(snapshot.corpus_project_ids.len());
    for project_id in snapshot.corpus_project_ids.iter() {
        let identity = identities.get(project_id).cloned();
        let record = records.get(project_id).cloned();
        let access = match record {
            Some(record) => Some(acquire_leases_for_record(
                broker,
                record,
                &collected,
                purpose,
                records_provider.git_history_transport_governed(project_id),
                records_provider.code_source_locality_governed(project_id),
            )?),
            None => None,
        };
        let cutback_pending = health_store
            .as_ref()
            .and_then(|store| store.load_activation(project_id).ok().flatten())
            .is_some_and(|activation| activation.cutback_pending);
        let effective = classify_effective_source(
            project_id,
            identity.as_ref(),
            access.as_ref(),
            collected.get(project_id),
            cutback_pending,
            assigned.contains(project_id),
            config,
            prior_meta_counts.get(project_id).copied().unwrap_or(0),
            accept_empty_projects.contains(project_id),
        );
        plans.push(ProjectSourcePlan {
            project_id: project_id.clone(),
            identity,
            effective,
            access,
        });
    }
    if let Some(store) = health_store.as_ref() {
        reconcile_planning_health(store, &plans, accept_empty_projects);
    }
    Ok(plans)
}

/// Prior-pass file counts per project, from the freshness rows. This is the
/// only inventory that can distinguish "an empty root" from "a project with
/// no files" (H3), and the same inventory the detached preservation arm
/// verifies against.
fn prior_meta_project_file_counts(
    meta: &HashMap<String, super::FileMeta>,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for row in meta.values() {
        if let super::FileMetaSource::LocalProjectFile { project_id, .. } = &row.source {
            *counts.entry(project_id.clone()).or_insert(0_usize) += 1;
        }
    }
    counts
}

#[allow(clippy::too_many_arguments)]
fn classify_effective_source(
    project_id: &str,
    identity: Option<&CodeProjectIdentity>,
    access: Option<&LeasedProjectAccess>,
    collected: Option<&super::project_files::ActiveCollectedSource>,
    cutback_pending: bool,
    assigned: bool,
    config: &ReindexConfig,
    prior_meta_files: usize,
    empty_purge_accepted: bool,
) -> EffectiveSource {
    if identity.is_none() {
        return EffectiveSource::Unavailable {
            reason: UnavailableReason::IdentityUnavailable(format!(
                "catalog project {project_id} has no projected identity"
            )),
        };
    }
    // A cutback in flight owns the transition; the pass neither walks nor
    // purges. Checked before the collected arm because the collected
    // selector is still live while the cutback stages.
    if cutback_pending {
        return EffectiveSource::CutbackPending;
    }
    if let Some(collected) = collected {
        return EffectiveSource::Collected {
            generation: collected.generation_id.clone(),
        };
    }
    let Some(access) = access else {
        // No compatibility record: nothing to lease and nothing to walk.
        // Warming here is the remote-only first-upload window, which is
        // informational, not a fault.
        return if assigned {
            EffectiveSource::Warming
        } else {
            EffectiveSource::Unavailable {
                reason: UnavailableReason::NoAttachment,
            }
        };
    };
    let Some(local) = access.local.as_ref() else {
        return EffectiveSource::Unavailable {
            reason: UnavailableReason::LocalWalkDenied(
                access
                    .local_denial
                    .clone()
                    .unwrap_or_else(|| "no LocalProjectWalk lease was acquired".to_string()),
            ),
        };
    };
    // H3: an attached, readable, EMPTY root is indistinguishable from a
    // deleted project by scan alone. Refuse rather than purge whenever the
    // prior pass recorded files, unless the operator acknowledged this pass.
    if !empty_purge_accepted
        && prior_meta_files > 0
        && !super::project_files::project_root_has_indexable_entry(local.project_root(), config)
    {
        return EffectiveSource::Unavailable {
            reason: UnavailableReason::EmptyRootRefused,
        };
    }
    if assigned {
        EffectiveSource::Warming
    } else {
        EffectiveSource::Local
    }
}

/// Persist the pass's unavailable states and clear the records of projects
/// that left them. Clearing is what makes the operator escapes converge:
/// detach, unregister, retire, and an acknowledged purge all remove the
/// project from the state that produced the record.
fn reconcile_planning_health(
    store: &bbox_code_source_store::CodeSourceStore,
    plans: &[ProjectSourcePlan],
    accept_empty_projects: &std::collections::BTreeSet<String>,
) {
    for plan in plans {
        let active_code = match &plan.effective {
            EffectiveSource::Unavailable { reason } => Some(reason.health_code()),
            _ => None,
        };
        for code in PLANNING_HEALTH_CODES {
            if Some(code) == active_code {
                let reason = match &plan.effective {
                    EffectiveSource::Unavailable { reason } => reason,
                    _ => unreachable!("active_code is Some only on the Unavailable arm"),
                };
                if let Err(error) =
                    store.record_health_failure(&plan.project_id, code, &reason.diagnostic())
                {
                    tracing::warn!(
                        project_id = %plan.project_id,
                        %error,
                        "persisting a source-planning health record failed"
                    );
                }
            } else if let Err(error) = store.clear_health_failure(&plan.project_id, code) {
                tracing::warn!(
                    project_id = %plan.project_id,
                    %error,
                    "clearing a source-planning health record failed"
                );
            }
        }
    }
    // An acknowledged project that is no longer planned at all (detached,
    // unregistered, retired between passes) still gets its record cleared.
    for project_id in accept_empty_projects {
        if plans.iter().any(|plan| &plan.project_id == project_id) {
            continue;
        }
        if let Err(error) = store.clear_health_failure(project_id, "empty_root_refused") {
            tracing::warn!(
                %project_id,
                %error,
                "clearing an acknowledged empty-root record failed"
            );
        }
    }
}

fn acquire_leases_for_record(
    broker: &Arc<CheckoutAccessBroker>,
    project: ProjectRecord,
    collected: &std::collections::BTreeMap<String, super::project_files::ActiveCollectedSource>,
    purpose: ProjectLeasePurpose,
    git_history_transport_governed: bool,
    code_source_locality_governed: bool,
) -> Result<LeasedProjectAccess> {
    let (publisher_config, expected_scope, publisher_config_denial) = match broker
        .recorded_project_scope(&project.project_id)
    {
        Ok(crate::checkout_access::CheckoutRecordedProjectScope::Published(scope)) => {
            (None, Some(scope), None)
        }
        Ok(crate::checkout_access::CheckoutRecordedProjectScope::LegacyLocal) => (None, None, None),
        Ok(crate::checkout_access::CheckoutRecordedProjectScope::Unavailable) => {
            let publisher_config = broker.acquire(access_request(
                &project.project_id,
                None,
                CheckoutAccessKind::PublisherConfigTreeRead,
            ));
            let expected_scope = publisher_config
                .as_ref()
                .ok()
                .and_then(|lease| lease.published_scope().cloned());
            let denial = publisher_config.as_ref().err().map(ToString::to_string);
            (publisher_config.ok(), expected_scope, denial)
        }
        Err(error) => (None, None, Some(error.to_string())),
    };
    // A collected project needs no local walk lease for its own indexing,
    // but a full reindex pass still uses the local root for the tool-edge,
    // project-record, and knowledge lanes, so the acquisition set stays
    // exactly today's (bridge parity; leases are a property of attachment,
    // not of effective source).
    let needs_local = !code_source_locality_governed
        && (!collected.contains_key(&project.project_id)
            || purpose == ProjectLeasePurpose::Reindex);
    let (local, local_denial) = if !needs_local {
        (None, None)
    } else {
        match broker.acquire(access_request(
            &project.project_id,
            expected_scope.clone(),
            CheckoutAccessKind::LocalProjectWalk,
        )) {
            Ok(lease) => (Some(lease), None),
            Err(error) => (None, Some(error.to_string())),
        }
    };
    let (git, git_denial) = if project.is_git_repo && !git_history_transport_governed {
        match broker.acquire(access_request(
            &project.project_id,
            expected_scope.clone(),
            CheckoutAccessKind::GitHistory,
        )) {
            Ok(lease) => (Some(lease), None),
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, None)
    };
    let (knowledge_overlay, knowledge_overlay_denial) = if purpose == ProjectLeasePurpose::Reindex {
        match broker.acquire(access_request(
            &project.project_id,
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
    })
}

pub(super) fn revalidate_planned_leases(
    broker: &CheckoutAccessBroker,
    plans: &[ProjectSourcePlan],
) -> Result<()> {
    for access in plans.iter().filter_map(|plan| plan.access.as_ref()) {
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
    project_id: &str,
    expected_scope: Option<bbox_corpus_core::identity::PublishedScope>,
    kind: CheckoutAccessKind,
) -> CheckoutAccessRequest {
    CheckoutAccessRequest {
        project_id: project_id.to_string(),
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
const STAGE_HOLD_HELD: u8 = 0;
const STAGE_HOLD_PUBLISHING: u8 = 1;
const STAGE_HOLD_EXPIRED: u8 = 2;
const STAGE_HOLD_RELEASED: u8 = 3;

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
        let records_provider = Arc::new(crate::projects::BridgeProjectRecordsProvider::new(
            projects.clone(),
        ));
        Self::spawn_for_with_checkout_access(idx, records_provider, broker)
    }

    /// Spawn against the daemon's single shared registry and access broker.
    pub fn spawn_for_with_checkout_access(
        idx: &TranscriptIndex,
        records_provider: Arc<dyn ProjectRecordsProvider>,
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Self {
        register_index_store_hooks();
        Self::spawn(
            idx.index_handle(),
            idx.field_handles(),
            idx.reindex_config(),
            idx.reader_handle(),
            idx.stats_cache_handle(),
            records_provider,
            checkout_access,
        )
    }

    pub fn spawn(
        index: Index,
        fields: FieldHandles,
        config: ReindexConfig,
        reader: IndexReader,
        stats_cache: StatsCache,
        records_provider: Arc<dyn ProjectRecordsProvider>,
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<IndexWriteOp>();
        let post_commit_hook = Arc::new(parking_lot::RwLock::new(None));
        let assignments: Arc<parking_lot::RwLock<Option<Arc<dyn ProducerAssignmentSource>>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let publication_activity = Arc::new(AtomicU8::new(PUBLICATION_IDLE));
        let ctx = ActorCtx {
            index,
            fields,
            config: config.clone(),
            reader: reader.clone(),
            stats_cache,
            post_commit_hook: post_commit_hook.clone(),
            checkout_access: checkout_access.clone(),
            records_provider: records_provider.clone(),
            assignments: assignments.clone(),
            publication_activity: publication_activity.clone(),
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
            publication_activity,
            reader,
            fields,
            post_commit_hook,
            checkout_access,
            records_provider,
            assignments,
            config,
        }
    }

    /// Inject the producer assignment table (Phase 3 plan section 4.7). The
    /// daemon's code-source runtime is built after the writer actor spawns,
    /// so this is a post-spawn install exactly like the post-commit hook
    /// rather than a constructor argument. Unset means no assignments exist,
    /// which is correct for the bridge, tests, and offline index builds.
    pub fn set_producer_assignment_source(&self, source: Arc<dyn ProducerAssignmentSource>) {
        *self.assignments.write() = Some(source);
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
    ///
    /// ORDINARY cause: this is the periodic/triggered pass over a populated
    /// index, where the preservation gates are load-bearing. The post-reset
    /// rebuild uses [`Self::run_reindex_pass_for_schema_migration`] instead.
    pub fn run_reindex_pass(&self, full: bool, dirty: bool) -> Result<String> {
        self.run_reindex_pass_accepting_empty(full, dirty, Vec::new())
    }

    /// The synchronous full rebuild that follows a destructive schema
    /// replacement (P3-E). Distinguished from an ordinary full pass by an
    /// explicit cause rather than by observing that the index is empty: an
    /// empty index on an ORDINARY pass must still fail the preservation gates,
    /// because that is exactly the property they exist to enforce.
    pub fn run_reindex_pass_for_schema_migration(&self) -> Result<String> {
        self.dispatch_reindex_pass(true, true, FullRebuildCause::SchemaMigration, Vec::new())
    }

    /// Reindex pass with the operator's H3 acknowledgement list. Only
    /// `bbox_reindex` supplies a non-empty list; every other caller uses
    /// [`Self::run_reindex_pass`], which passes none (RX-V1: never defaulted).
    pub fn run_reindex_pass_accepting_empty(
        &self,
        full: bool,
        dirty: bool,
        accept_empty_projects: Vec<String>,
    ) -> Result<String> {
        self.dispatch_reindex_pass(
            full,
            dirty,
            FullRebuildCause::Ordinary,
            accept_empty_projects,
        )
    }

    /// Admit an interactive reindex request without holding the MCP call open
    /// for the entire corpus walk. Exactly one pass may be queued or running;
    /// completion is logged by the actor and periodic reconciliation remains
    /// the durable correctness backstop.
    pub fn request_reindex_pass_accepting_empty(
        &self,
        full: bool,
        dirty: bool,
        accept_empty_projects: Vec<String>,
    ) -> Result<String> {
        self.reserve_reindex()?;
        if self
            .tx
            .send(IndexWriteOp::ReindexPass {
                full,
                dirty,
                cause: FullRebuildCause::Ordinary,
                accept_empty_projects,
                ack: None,
            })
            .is_err()
        {
            self.publication_activity
                .store(PUBLICATION_IDLE, Ordering::Release);
            return Err(anyhow!("index writer actor unavailable"));
        }
        Ok("reindex accepted; the index writer is processing it in the background".to_string())
    }

    /// Whether a reindex pass has been admitted and has not yet reached its
    /// terminal outcome. Sidecar consumers use this to avoid parsing an
    /// authority set while the pass is still publishing its members.
    pub fn reindex_in_progress(&self) -> bool {
        self.publication_activity.load(Ordering::Acquire) == PUBLICATION_REINDEX
    }

    /// Try to reserve the sidecar publication for one complete edge-index
    /// rebuild. The caller must hold the returned guard through both parsing
    /// and publication so reindex admission cannot slip into that window.
    pub fn try_begin_edge_index_rebuild(&self) -> Option<EdgeIndexRebuildActivityGuard> {
        self.publication_activity
            .compare_exchange(
                PUBLICATION_IDLE,
                PUBLICATION_EDGE_REBUILD,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| EdgeIndexRebuildActivityGuard(self.publication_activity.clone()))
    }

    fn reserve_reindex(&self) -> Result<()> {
        self.publication_activity
            .compare_exchange(
                PUBLICATION_IDLE,
                PUBLICATION_REINDEX,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|activity| {
                let error = if activity == PUBLICATION_EDGE_REBUILD {
                    IndexWriterRetryableError::EdgeIndexRebuildInProgress
                } else {
                    IndexWriterRetryableError::ReindexPassInProgress
                };
                anyhow::Error::new(error)
            })
    }

    fn dispatch_reindex_pass(
        &self,
        full: bool,
        dirty: bool,
        cause: FullRebuildCause,
        accept_empty_projects: Vec<String>,
    ) -> Result<String> {
        self.reserve_reindex()?;
        let (ack, ack_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(IndexWriteOp::ReindexPass {
                full,
                dirty,
                cause,
                accept_empty_projects,
                ack: Some(ack),
            })
            .is_err()
        {
            self.publication_activity
                .store(PUBLICATION_IDLE, Ordering::Release);
            return Err(anyhow!("index writer actor unavailable"));
        }
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the reindex ack"))?
    }

    pub fn needs_reindex(&self) -> Result<bool> {
        super::reindex::needs_reindex(
            &self.config,
            &self.records_provider,
            &self.checkout_access,
            self.assignments.read().clone().as_ref(),
        )
    }

    pub fn verify_code_selector_document_count(&self, selector: &str, expected: u64) -> Result<()> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.code_source_selector, selector),
            IndexRecordOption::Basic,
        );
        let observed = searcher.search(&query, &Count)? as u64;
        if observed != expected {
            anyhow::bail!(
                "staged code-source document count changed before publication: expected {expected}, observed {observed}"
            );
        }
        Ok(())
    }

    pub fn stage_collected_generation(
        &self,
        identity: CodeProjectIdentity,
        descriptor: bbox_code_source::GenerationDescriptor,
        generation_id: String,
        entries: Vec<bbox_code_source::ManifestEntry>,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
    ) -> Result<StagedIndexGeneration> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let hold_state = Arc::new(AtomicU8::new(STAGE_HOLD_HELD));
        self.tx
            .send(IndexWriteOp::StageCollectedGeneration {
                identity: Box::new(identity),
                descriptor: Box::new(descriptor),
                generation_id,
                entries,
                store,
                ack,
                release: release_rx,
                hold_state: hold_state.clone(),
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let result = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the collected-stage ack"))??;
        Ok(StagedIndexGeneration {
            result,
            release: Some(release),
            hold_state,
        })
    }

    pub fn stage_local_generation(
        &self,
        identity: CodeProjectIdentity,
        scope: bbox_corpus_core::identity::PublishedScope,
        store: std::sync::Arc<bbox_code_source_store::CodeSourceStore>,
    ) -> Result<StagedIndexGeneration> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let hold_state = Arc::new(AtomicU8::new(STAGE_HOLD_HELD));
        self.tx
            .send(IndexWriteOp::StageLocalGeneration {
                identity: Box::new(identity),
                scope: Box::new(scope),
                store,
                ack,
                release: release_rx,
                hold_state: hold_state.clone(),
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let result = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the local-stage ack"))??;
        Ok(StagedIndexGeneration {
            result,
            release: Some(release),
            hold_state,
        })
    }

    /// Stage the Git current-file overlay for an ALREADY ACTIVE generation
    /// (Phase 3 plan section 6 item 3). The caller owns the `GitHistory`
    /// lease and the best-effort policy: this returns the walk's error
    /// instead of rolling anything back, because the generation it decorates
    /// is already published and must stay published.
    pub fn stage_git_current_overlay(
        &self,
        project: ProjectRecord,
        project_display: String,
        lease: ValidatedCheckoutLease,
        snapshot_id: String,
        current_chunk_targets: HashMap<String, bbox_corpus_core::entity_ref::EntityRef>,
    ) -> Result<()> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::StageGitCurrentOverlay {
                project: Box::new(project),
                project_display,
                lease: Box::new(lease),
                snapshot_id,
                current_chunk_targets,
                ack,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the git-overlay ack"))?
    }

    /// Write one consolidated repo-history walk into the index.
    ///
    /// Returns the number of commit documents written. Best effort like the
    /// overlay op: the code generation it decorates is already published, so
    /// a failure here degrades history health and never unpublishes anything.
    pub fn stage_consolidated_history(
        &self,
        commit_namespace: String,
        project_display: String,
        commits: Vec<bbox_corpus_core::git::GitCommit>,
        edges_by_project: std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
        snapshot_by_project: std::collections::BTreeMap<String, String>,
    ) -> Result<u64> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::StageConsolidatedHistory {
                commit_namespace,
                project_display,
                commits,
                edges_by_project,
                snapshot_by_project,
                ack,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the consolidated-history ack"))?
    }

    pub fn publish_history_generation(
        &self,
        generation: bbox_corpus_index::index::history_generations::HistoryGenerationRecordV1,
        owner: bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1,
        edges_by_project: std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
        snapshot_by_project: std::collections::BTreeMap<String, String>,
    ) -> Result<HistoryPublicationResultV1> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        self.tx
            .send(IndexWriteOp::PublishHistoryGeneration {
                generation: Box::new(generation),
                owner: Box::new(owner),
                edges_by_project,
                snapshot_by_project,
                ack,
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the history-publication ack"))?
    }

    pub fn retire_code_selector(&self, selector: String) -> Result<RetiredCodeSelector> {
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let hold_state = Arc::new(AtomicU8::new(STAGE_HOLD_HELD));
        self.tx
            .send(IndexWriteOp::RetireCodeSelector {
                selector,
                ack,
                release: release_rx,
                hold_state: hold_state.clone(),
            })
            .map_err(|_| anyhow!("index writer actor unavailable"))?;
        let document_count = ack_rx
            .recv()
            .map_err(|_| anyhow!("index writer actor dropped the selector-retirement ack"))??;
        Ok(RetiredCodeSelector {
            document_count,
            release: Some(release),
            hold_state,
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
            IndexWriteOp::ReindexPass {
                full,
                dirty,
                cause,
                accept_empty_projects,
                ack,
            } => {
                let activity = ReindexActivityGuard(ctx.publication_activity.clone());
                let result = run_pass(&ctx, &rx, full, dirty, cause, &accept_empty_projects);
                drop(activity);
                if let Some(ack) = ack {
                    let _ = ack.send(result);
                } else {
                    match result {
                        Ok(summary) => tracing::info!(%summary, "background reindex completed"),
                        Err(error) => tracing::error!(%error, "background reindex failed"),
                    }
                }
            }
            IndexWriteOp::StageCollectedGeneration {
                identity,
                descriptor,
                generation_id,
                entries,
                store,
                ack,
                release,
                hold_state,
            } => {
                let result = run_collected_stage(
                    &ctx,
                    &identity,
                    &descriptor,
                    &generation_id,
                    &entries,
                    &store,
                );
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_generation_stage_release(
                        release,
                        "collected generation",
                        &hold_state,
                        STAGED_GENERATION_HOLD_TIMEOUT,
                    );
                }
            }
            IndexWriteOp::StageLocalGeneration {
                identity,
                scope,
                store,
                ack,
                release,
                hold_state,
            } => {
                let result = run_local_stage(&ctx, &identity, &scope, &store);
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_generation_stage_release(
                        release,
                        "local generation",
                        &hold_state,
                        STAGED_GENERATION_HOLD_TIMEOUT,
                    );
                }
            }
            IndexWriteOp::StageGitCurrentOverlay {
                project,
                project_display,
                lease,
                snapshot_id,
                current_chunk_targets,
                ack,
            } => {
                let result = run_git_current_overlay(
                    &ctx,
                    &project,
                    &project_display,
                    &lease,
                    &snapshot_id,
                    &current_chunk_targets,
                );
                let _ = ack.send(result);
            }
            IndexWriteOp::StageConsolidatedHistory {
                commit_namespace,
                project_display,
                commits,
                edges_by_project,
                snapshot_by_project,
                ack,
            } => {
                let result = run_consolidated_history(
                    &ctx,
                    &commit_namespace,
                    &project_display,
                    &commits,
                    &edges_by_project,
                    &snapshot_by_project,
                );
                let _ = ack.send(result);
            }
            IndexWriteOp::PublishHistoryGeneration {
                generation,
                owner,
                edges_by_project,
                snapshot_by_project,
                ack,
            } => {
                let result = run_history_generation_publication(
                    &ctx,
                    &generation,
                    &owner,
                    &edges_by_project,
                    &snapshot_by_project,
                );
                let _ = ack.send(result);
            }
            IndexWriteOp::RetireCodeSelector {
                selector,
                ack,
                release,
                hold_state,
            } => {
                let result = run_selector_retirement(&ctx, &selector);
                let should_hold = result.is_ok();
                let _ = ack.send(result);
                if should_hold {
                    await_generation_stage_release(
                        release,
                        "selector retirement",
                        &hold_state,
                        STAGED_GENERATION_HOLD_TIMEOUT,
                    );
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
                        Ok(overlay @ IndexWriteOp::StageGitCurrentOverlay { .. }) => {
                            deferred = Some(overlay);
                            break;
                        }
                        // Deferred like every other control op: it opens its
                        // own writer and commits, so batching it beside small
                        // ops would nest two writers on one index.
                        Ok(history @ IndexWriteOp::StageConsolidatedHistory { .. }) => {
                            deferred = Some(history);
                            break;
                        }
                        Ok(history @ IndexWriteOp::PublishHistoryGeneration { .. }) => {
                            deferred = Some(history);
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

fn await_generation_stage_release(
    release: mpsc::Receiver<()>,
    operation: &str,
    hold_state: &AtomicU8,
    timeout: std::time::Duration,
) {
    match release.recv_timeout(timeout) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            hold_state.store(STAGE_HOLD_RELEASED, Ordering::Release);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if hold_state
                .compare_exchange(
                    STAGE_HOLD_HELD,
                    STAGE_HOLD_EXPIRED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                tracing::error!(
                    operation,
                    timeout_secs = timeout.as_secs(),
                    "index writer stage hold timed out; publication will fail closed"
                );
                return;
            }
            if hold_state.load(Ordering::Acquire) == STAGE_HOLD_PUBLISHING {
                let _ = release.recv();
                hold_state.store(STAGE_HOLD_RELEASED, Ordering::Release);
            }
        }
    }
}

/// Resolve the version-1 compatibility document fields the identity does not
/// carry, from the attached record when one exists.
///
/// A remote-only catalog project has no `ProjectRecord` at all and therefore
/// neither field; that is the correct answer, not a degradation. The lookup
/// runs against the same epoch-cached provider snapshot every other consumer
/// reads, so it cannot disagree with the daemon-side resolution.
fn compat_record(ctx: &ActorCtx, project_id: &str) -> Option<ProjectRecord> {
    ctx.records_provider
        .records_snapshot()
        .records
        .iter()
        .find(|record| record.project_id == project_id)
        .cloned()
}

/// Refuse collected staging for an identity that cannot own a producer grant
/// (Phase 3 plan section 6 item 1, D-034).
///
/// The predicate is exactly the one pinned in
/// `bbox_corpus_core::code_project_identity`: refuse if and only if the
/// identity came from the catalog AND its scope is `LegacyLocal`, because a
/// catalog `LegacyLocal` project has no published scope to collect under. A
/// `Bridge` identity always proceeds regardless of scope: bridge collected
/// staging runs on lease/grant-table authority through Phase 3, and its
/// placeholder `LegacyLocal` scope is an absence marker, not a signal.
fn refuse_collected_staging_for_legacy_local(identity: &CodeProjectIdentity) -> Result<()> {
    if identity.origin == IdentityOrigin::Catalog && identity.scope == ProjectScope::LegacyLocal {
        anyhow::bail!(
            "error.collected_source_scope_unavailable: catalog project {} is LegacyLocal \
             and cannot own a collected generation",
            identity.project_id.as_str()
        );
    }
    Ok(())
}

/// Stage a collected generation with NO checkout access whatsoever.
///
/// Governing section 11 / Phase 3 plan section 6 item 2: the activation
/// transaction commits code documents, code edges, the vector enqueue, and
/// the selector without opening Git. There is no `GitHistory` lease, no
/// current-file edge staging, and no post-stage revalidate/restage cycle
/// here any more, so a Git problem can no longer fail or roll back a valid
/// collected generation (F5). Current-file Git edges are a post-activation
/// best-effort overlay owned by the daemon (`StageGitCurrentOverlay`).
fn run_collected_stage(
    ctx: &ActorCtx,
    identity: &CodeProjectIdentity,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    store: &bbox_code_source_store::CodeSourceStore,
) -> Result<super::project_files::CollectedIndexResult> {
    // Fails BEFORE any writer work: no writer is created, no document is
    // staged, and no store state moves.
    refuse_collected_staging_for_legacy_local(identity)?;
    let compat = compat_record(ctx, identity.project_id.as_str());
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let mut publication = super::project_files::ProjectIndexPublicationBundle::default();
    let result = super::project_files::stage_collected_project_generation(
        identity,
        super::project_files::ProjectFileCompatFields {
            repo_id: compat.as_ref().and_then(|record| record.repo_id.as_deref()),
        },
        descriptor,
        generation_id,
        entries,
        ctx.fields,
        &mut writer,
        &edges_dir,
        &mut publication,
        |entry| {
            let mut file = store.verified_blob_file(&entry.content_sha256, entry.size)?;
            let mut bytes = Vec::with_capacity(entry.size as usize);
            std::io::Read::read_to_end(&mut file, &mut bytes)?;
            Ok(bytes)
        },
    )?;
    // No checkout lease contributed to this bundle, so there is no
    // publication guard to hold: the broker's guard exists to pin checkout
    // lifecycle across publish, and this transaction touched no checkout.
    let (publication_result, commit_payload) =
        commit_snapshot_publications(&ctx.index, &mut writer, &edges_dir, publication.publish()?)?;
    publication_result.finalize_publications()?;
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    post_commit(ctx);
    Ok(result)
}

fn run_local_stage(
    ctx: &ActorCtx,
    identity: &CodeProjectIdentity,
    scope: &bbox_corpus_core::identity::PublishedScope,
    _store: &bbox_code_source_store::CodeSourceStore,
) -> Result<super::project_files::CollectedIndexResult> {
    let project_id = identity.project_id.as_str();
    // A catalog identity carries its own published scope; the authorized
    // producer scope must be that same scope (P3-B resolves the grant table
    // by exact scope equality against the catalog snapshot, so a mismatch is
    // a bug, not a policy). A bridge identity has no catalog scope at all
    // and the caller-supplied one is authoritative (D-034).
    if let ProjectScope::Published(identity_scope) = &identity.scope
        && identity_scope != scope
    {
        anyhow::bail!(
            "error.local_source_scope_mismatch: catalog project {project_id} publishes a \
             different scope than the authorized producer scope"
        );
    }
    let local_lease = ctx.checkout_access.acquire(access_request(
        project_id,
        Some(scope.clone()),
        CheckoutAccessKind::LocalProjectWalk,
    ))?;
    // Local code staging needs the checkout HEAD for its descriptor, but it
    // does not ingest repository history. Reuse the LocalProjectWalk lease so
    // code cutback cannot manufacture a GitHistory observation after strict
    // transport cutover.
    let compat = compat_record(ctx, project_id);
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let mut publication = super::project_files::ProjectIndexPublicationBundle::default();
    let result = super::project_files::stage_local_project_generation(
        identity,
        super::project_files::ProjectFileCompatFields {
            repo_id: compat.as_ref().and_then(|record| record.repo_id.as_deref()),
        },
        scope,
        local_lease.project_root(),
        local_lease.checkout_root(),
        ctx.fields,
        &mut writer,
        &edges_dir,
        &mut publication,
    )?;
    // P3-F completes the P3-B deferral: local staging no longer walks Git
    // inside its own transaction. It stages the EMPTY current-file member and
    // lets the post-activation overlay step own the walk, exactly as the
    // collected lane does.
    //
    // The parity consequence is enumerated in plan section 6 item 3 as
    // amended: after a local cutback, `git-current.jsonl` is empty and the
    // manifest entry is overlay-managed with no selector, so the loader gates
    // the member off until the overlay lands. That is strictly better than
    // the alternative it replaces - a Git error mid-walk previously failed
    // the whole cutback, which is the F5 class on the local side.
    //
    // `stage_snapshot_git_current` with `include_managed_git: false` is what
    // writes the empty member: the member must EXIST (the snapshot's file set
    // is fixed at write time) while carrying no edges.
    let _ = compat.as_ref();
    publication.stage_snapshot_git_current(&edges_dir, project_id, &result.snapshot_id, false);
    let _publication_guard = ctx.checkout_access.publication_guard(&local_lease)?;
    let (publication_result, commit_payload) =
        commit_snapshot_publications(&ctx.index, &mut writer, &edges_dir, publication.publish()?)?;
    publication_result.finalize_publications()?;
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    post_commit(ctx);
    Ok(result)
}

/// Walk Git for an already-active generation and publish its current-file
/// overlay (Phase 3 plan section 6 item 3).
///
/// Failure here never touches the generation: the caller reports it as
/// degraded health and leaves the active selector, documents, and snapshot
/// exactly as the activation left them.
fn run_git_current_overlay(
    ctx: &ActorCtx,
    project: &ProjectRecord,
    project_display: &str,
    lease: &ValidatedCheckoutLease,
    snapshot_id: &str,
    current_chunk_targets: &HashMap<String, bbox_corpus_core::entity_ref::EntityRef>,
) -> Result<()> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let git_meta_dir =
        super::git_history::git_meta_dir_from_projects_path(&ctx.config.projects_path);
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let mut publication = super::project_files::ProjectIndexPublicationBundle::default();
    let mut meta = HashMap::new();
    let mut git_ctx = super::git_history::GitIndexContext {
        f: ctx.fields,
        writer: &mut writer,
        meta: &mut meta,
        edges_dir: &edges_dir,
        git_meta_dir: &git_meta_dir,
        force_full: true,
        publication: &mut publication,
        project_display,
    };
    super::git_history::index_git_history_for_project(
        project,
        lease.checkout_root(),
        current_chunk_targets,
        &mut git_ctx,
    )?;
    drop(git_ctx);
    publication.stage_snapshot_git_current(&edges_dir, &project.project_id, snapshot_id, true);
    let _publication_guard = ctx.checkout_access.publication_guard(lease)?;
    let (publication_result, commit_payload) =
        commit_snapshot_publications(&ctx.index, &mut writer, &edges_dir, publication.publish()?)?;
    publication_result.finalize_publications()?;
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    post_commit(ctx);
    Ok(())
}

/// Write one consolidated repo-history walk: commit documents under the
/// PRIMARY namespace once, per-project managed Git sidecars, and one vector
/// enqueue per commit.
///
/// ONE ENQUEUE PER COMMIT, not one per member project. The bridge lane
/// enqueued inside its per-project walk, so a monorepo re-embedded every
/// commit message once per sibling; consolidating the walk is what makes the
/// single enqueue possible.
// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
fn run_consolidated_history(
    ctx: &ActorCtx,
    commit_namespace: &str,
    project_display: &str,
    commits: &[bbox_corpus_core::git::GitCommit],
    edges_by_project: &std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
    snapshot_by_project: &std::collections::BTreeMap<String, String>,
) -> Result<u64> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    let mut written = 0_u64;
    for commit in commits {
        let entity_id = super::git_history::commit_entity_id(commit_namespace, &commit.sha);
        writer.delete_term(Term::from_field_text(ctx.fields.entity_id, &entity_id));
        writer.add_document(super::git_history::build_commit_doc(
            commit,
            commit_namespace,
            // The per-project source key this document's `file_path` carries
            // is the delete term of the LEGACY per-project purge arm. A
            // consolidated document belongs to no single project, so it is
            // keyed by the namespace instead; the purge arm that used the
            // project key never applied to it.
            commit_namespace,
            project_display,
            ctx.fields,
        ))?;
        super::embed_hook::emit_git_message(
            &entity_id,
            &super::git_history::commit_message_hash(&commit.message),
            &commit.message,
        );
        written += 1;
    }
    let mut publications: Vec<super::project_files::PublicationResult> = Vec::new();
    let staging_attempt = (|| -> Result<()> {
        for (project_id, edges) in edges_by_project {
            // Replace, not merge: a consolidated walk that produced no edge for a
            // project is asserting that project currently has none, and merging
            // would keep a retired snapshot's edges alive forever.
            bbox_edge_sidecar::edge_sidecar::replace_materialized_edges(
                &edges_dir, "git", project_id, edges,
            )?;
            if let Some(snapshot_id) = snapshot_by_project.get(project_id) {
                let mut publication =
                    super::project_files::ProjectIndexPublicationBundle::default();
                publication.stage_snapshot_git_current(&edges_dir, project_id, snapshot_id, true);
                publications.push(publication.publish()?);
            }
        }
        Ok(())
    })();
    if let Err(error) = staging_attempt {
        let mut cleanup_failures = Vec::new();
        for publication in &mut publications {
            if let Err(cleanup) = publication.rollback_pending() {
                cleanup_failures.push(format!("{cleanup:#}"));
            }
        }
        if !cleanup_failures.is_empty() {
            return Err(error).context(format!(
                "consolidated snapshot staging failed and rollback left unresolved state: {}",
                cleanup_failures.join("; ")
            ));
        }
        return Err(error);
    }
    let commit_attempt = (|| -> Result<String> {
        let prior_payload = ctx
            .index
            .load_metas()
            .context("loading prior index payload before snapshot commit")?
            .payload;
        let current: Vec<String> = publications
            .iter()
            .flat_map(|publication| publication.pending_commitments())
            .collect();
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
        Ok(payload)
    })();
    let commit_payload = match commit_attempt {
        Ok(payload) => payload,
        Err(error) => {
            let mut cleanup_failures = Vec::new();
            for publication in &mut publications {
                if let Err(cleanup) = publication.rollback_pending() {
                    cleanup_failures.push(format!("{cleanup:#}"));
                }
            }
            if !cleanup_failures.is_empty() {
                return Err(error).context(format!(
                    "snapshot commit failed and rollback left unresolved state: {}",
                    cleanup_failures.join("; ")
                ));
            }
            return Err(error);
        }
    };
    let mut all_handles = Vec::new();
    for publication in &mut publications {
        publication.mark_commit_succeeded();
        all_handles.extend(publication.take_pending_snapshot_finalizations());
    }
    // R20F4: fail closed if any finalization fails.
    for handle in &all_handles {
        if let Err(error) = bbox_edge_sidecar::snapshot::finalize_snapshot_publication(handle) {
            tracing::error!(
                project_id = %handle.project_id,
                snapshot_id = %handle.snapshot_id,
                txn_token = %handle.txn_token,
                error = %error,
                "failed to finalize snapshot publication after consolidated index commit"
            );
            return Err(error);
        }
    }
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    post_commit(ctx);
    Ok(written)
}

/// Publish the exact rows already verified in one immutable P3 generation.
// executes inside the IndexWriterActor pass (sanctioned single-writer).
#[allow(clippy::disallowed_methods)]
fn run_history_generation_publication(
    ctx: &ActorCtx,
    generation: &bbox_corpus_index::index::history_generations::HistoryGenerationRecordV1,
    owner: &bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1,
    edges_by_project: &std::collections::BTreeMap<String, Vec<bbox_chunker::Edge>>,
    snapshot_by_project: &std::collections::BTreeMap<String, String>,
) -> Result<HistoryPublicationResultV1> {
    generation
        .validate()
        .map_err(|error| anyhow!("verified history generation is invalid: {error}"))?;
    if edges_by_project.keys().ne(snapshot_by_project.keys()) {
        anyhow::bail!("history edge projects and snapshot-receipt projects must be exactly equal");
    }
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&ctx.config.projects_path);
    let mut writer = create_writer(&ctx.index, WRITER_HEAP_REINDEX)?;
    writer.set_merge_policy(Box::new(conservative_log_merge_policy()));
    // This generation is the COMPLETE active commit view for its namespace.
    // A force-push can legitimately remove commits, so delete the
    // `(repo_id, doc_type=commit)` lane before re-emitting the exact
    // generation inventory; deleting `repo_id` alone would also erase live
    // code chunks from that repository.
    writer.delete_query(Box::new(BooleanQuery::new(vec![
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(
                    ctx.fields.repo_id,
                    generation.manifest.body.namespace.as_str(),
                ),
                IndexRecordOption::Basic,
            )),
        ),
        (
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(ctx.fields.doc_type, "commit"),
                IndexRecordOption::Basic,
            )),
        ),
    ])))?;
    let written = bbox_corpus_index::index::schema_replacement::reemit_commit_documents(
        &writer,
        ctx.fields,
        &generation.commit_documents,
        owner,
    )?;
    for input in &generation.vector_inputs {
        super::embed_hook::emit_git_message(&input.entity_id, &input.content_hash, &input.message);
    }

    let mut publications: Vec<(String, super::project_files::PublicationResult)> = Vec::new();
    let staging_attempt = (|| -> Result<()> {
        for (project_id, edges) in edges_by_project {
            bbox_edge_sidecar::edge_sidecar::replace_materialized_edges(
                &edges_dir, "git", project_id, edges,
            )?;
            if let Some(snapshot_id) = snapshot_by_project.get(project_id) {
                let mut publication =
                    super::project_files::ProjectIndexPublicationBundle::default();
                publication.stage_snapshot_git_current(&edges_dir, project_id, snapshot_id, true);
                publications.push((project_id.clone(), publication.publish()?));
            }
        }
        Ok(())
    })();
    if let Err(error) = staging_attempt {
        let mut cleanup_failures = Vec::new();
        for (_, publication) in &mut publications {
            if let Err(cleanup) = publication.rollback_pending() {
                cleanup_failures.push(format!("{cleanup:#}"));
            }
        }
        if !cleanup_failures.is_empty() {
            return Err(error).context(format!(
                "history snapshot staging failed and rollback left unresolved state: {}",
                cleanup_failures.join("; ")
            ));
        }
        return Err(error);
    }
    for (_, publication) in &publications {
        if publication.pending_commitments().len() != 1 {
            anyhow::bail!("history overlay publication did not stage exactly one receipt");
        }
    }
    let commit_attempt = (|| -> Result<String> {
        let prior_payload = ctx
            .index
            .load_metas()
            .context("loading prior index payload before history commit")?
            .payload;
        let current = publications
            .iter()
            .flat_map(|(_, publication)| publication.pending_commitments())
            .collect::<Vec<_>>();
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
        Ok(payload)
    })();
    let commit_payload = match commit_attempt {
        Ok(payload) => payload,
        Err(error) => {
            let mut cleanup_failures = Vec::new();
            for (_, publication) in &mut publications {
                if let Err(cleanup) = publication.rollback_pending() {
                    cleanup_failures.push(format!("{cleanup:#}"));
                }
            }
            if !cleanup_failures.is_empty() {
                return Err(error).context(format!(
                    "history snapshot commit failed and rollback left unresolved state: {}",
                    cleanup_failures.join("; ")
                ));
            }
            return Err(error);
        }
    };
    let mut handles = Vec::new();
    for (_, publication) in &mut publications {
        publication.mark_commit_succeeded();
        handles.extend(publication.take_pending_snapshot_finalizations());
    }
    for handle in &handles {
        bbox_edge_sidecar::snapshot::finalize_snapshot_publication(handle)?;
    }
    let overlay_file_commitments = publications
        .iter()
        .map(|(project_id, _)| {
            let snapshot_id = snapshot_by_project
                .get(project_id)
                .ok_or_else(|| anyhow!("history overlay publication lost its snapshot id"))?;
            let commitment = bbox_edge_sidecar::snapshot::snapshot_publication_commitment(
                &edges_dir,
                project_id,
                snapshot_id,
            )?
            .ok_or_else(|| anyhow!("history overlay publication has no durable receipt"))?;
            Ok((project_id.clone(), commitment))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    bbox_edge_sidecar::snapshot::prune_receipt_closeouts_after_commit(
        &edges_dir,
        (!commit_payload.is_empty()).then_some(commit_payload.as_str()),
    )?;
    post_commit(ctx);
    Ok(HistoryPublicationResultV1 {
        commit_document_count: written,
        commit_view_commitment: generation
            .manifest
            .body
            .commit_document_commitment_sha256
            .clone(),
        overlay_file_commitments,
    })
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
        if count == 0 {
            // Nothing indexed under the selector (fresh or empty index).
            // TopDocs panics on a zero limit, so the sweep short-circuits
            // exactly like the other selector search sites.
            return Ok(0);
        }
        let vectors = bbox_vectors::try_global()
            .ok_or_else(|| anyhow::Error::new(IndexWriterRetryableError::VectorStoreWarming))?;
        let mut entity_ids = Vec::with_capacity(count);
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<tantivy::TantivyDocument>(address)?;
            if let Some(tantivy::schema::OwnedValue::Str(entity_id)) =
                document.get_first(ctx.fields.entity_id)
            {
                entity_ids.push(entity_id.clone());
            }
        }
        let vector_delete = vectors
            .delete_entities_all_routes(&entity_ids)
            .map_err(|failure| {
                tracing::warn!(
                    selector,
                    requested_entities = failure.requested_entities,
                    completed_entity_route_ops = failure.entity_route_ops_completed,
                    remaining_entity_route_ops = failure.entity_route_ops_remaining,
                    failing_route = %failure.route,
                    failing_chunk = failure.chunk_index,
                    "selector-retirement vector batch stopped after a partial durable prefix"
                );
                anyhow::Error::new(failure)
            })?;
        tracing::info!(
            selector,
            requested_entities = vector_delete.requested_entities,
            completed_routes = vector_delete.routes.len(),
            tombstones_appended = vector_delete
                .routes
                .iter()
                .map(|route| route.tombstones_appended)
                .sum::<usize>(),
            active_removed = vector_delete
                .routes
                .iter()
                .map(|route| route.active_removed)
                .sum::<usize>(),
            "selector-retirement vector batch completed"
        );
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
    cause: FullRebuildCause,
    accept_empty_projects: &[String],
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
                        if let Some(ack) = ack {
                            let _ = ack.send(Err(anyhow::Error::new(
                                IndexWriterRetryableError::ReindexPassInProgress,
                            )));
                        }
                    }
                    IndexWriteOp::StageCollectedGeneration { ack, .. } => {
                        let _ = ack.send(Err(anyhow::Error::new(
                            IndexWriterRetryableError::ReindexPassInProgress,
                        )));
                    }
                    IndexWriteOp::StageLocalGeneration { ack, .. } => {
                        let _ = ack.send(Err(anyhow::Error::new(
                            IndexWriterRetryableError::ReindexPassInProgress,
                        )));
                    }
                    IndexWriteOp::StageConsolidatedHistory { ack, .. } => {
                        let _ = ack.send(Err(anyhow!("index writer actor is shutting down")));
                    }
                    IndexWriteOp::StageGitCurrentOverlay { ack, .. } => {
                        let _ = ack.send(Err(anyhow::Error::new(
                            IndexWriterRetryableError::ReindexPassInProgress,
                        )));
                    }
                    IndexWriteOp::RetireCodeSelector { ack, .. } => {
                        let _ = ack.send(Err(anyhow::Error::new(
                            IndexWriterRetryableError::ReindexPassInProgress,
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
            cause,
            &mut writer,
            &mut drain,
            &ctx.records_provider,
            &ctx.checkout_access,
            ctx.assignments.read().clone().as_ref(),
            accept_empty_projects,
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
        | IndexWriteOp::StageGitCurrentOverlay { .. }
        | IndexWriteOp::StageConsolidatedHistory { .. }
        | IndexWriteOp::PublishHistoryGeneration { .. }
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
            project_id: None,
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
        TranscriptIndex::open_or_create_with_records(
            &dir.join("idx"),
            Vec::new(),
            None,
            dir.join("projects.json"),
            dir.join("kb.json"),
            dir.join("threads.json"),
            dir.join("roadmap.json"),
            std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap()
    }

    #[test]
    fn history_publication_replaces_the_exact_namespace_after_force_push() {
        use bbox_corpus_core::git::GitCommit;
        use bbox_corpus_core::project_catalog::{CommitNamespace, RepoHistoryId};
        use bbox_corpus_index::index::history_generations::{
            HistoryGenerationInputV1, HistoryGenerationOwnerV1, HistoryGenerationStore,
            generation_rows_for_commit, live_schema_evidence,
        };
        use bbox_corpus_index::index::schema_replacement::CommitDocumentOwnerV1;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let transcript = test_index(&root);
        let namespace = CommitNamespace::parse("repo-force-push").unwrap();
        let sentinel_entity = "code-document-sharing-history-repo-id";
        {
            let index = transcript.index_handle();
            let mut writer = index.writer(15_000_000).unwrap();
            let fields = transcript.field_handles();
            let mut document = tantivy::TantivyDocument::default();
            document.add_text(fields.entity_id, sentinel_entity);
            document.add_text(fields.doc_type, "code");
            document.add_text(fields.repo_id, namespace.as_str());
            writer.add_document(document).unwrap();
            writer.commit().unwrap();
        }
        let actor = IndexWriterActor::spawn_for(&transcript);
        let history = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let make_generation = |commits: Vec<GitCommit>| {
            let mut documents = Vec::new();
            let mut vectors = Vec::new();
            for commit in commits {
                let (document, vector) = generation_rows_for_commit(&commit, namespace.as_str());
                documents.push(document);
                vectors.push(vector);
            }
            let (schema_version, schema_fingerprint) = live_schema_evidence().unwrap();
            HistoryGenerationStore::open_for_index(&root.join("idx"))
                .unwrap()
                .create_or_open(HistoryGenerationInputV1 {
                    namespace: namespace.clone(),
                    owner: HistoryGenerationOwnerV1::Owned {
                        repo_history_id: history.clone(),
                    },
                    commit_documents: documents,
                    vector_inputs: vectors,
                    truncated_message_count: 0,
                    source_schema_version: schema_version,
                    source_schema_fingerprint_sha256: schema_fingerprint,
                    source_index_fingerprint_sha256: "test-force-push".into(),
                })
                .unwrap()
        };
        let removed = GitCommit {
            sha: "1".repeat(40),
            parent_shas: vec![],
            author_name: "A".into(),
            author_email: "a@example.invalid".into(),
            message: "removed".into(),
        };
        let retained = GitCommit {
            sha: "2".repeat(40),
            parent_shas: vec![],
            author_name: "B".into(),
            author_email: "b@example.invalid".into(),
            message: "retained".into(),
        };
        let first = make_generation(vec![removed, retained.clone()]);
        assert!(
            actor
                .publish_history_generation(
                    first.clone(),
                    CommitDocumentOwnerV1::unclaimed(namespace.as_str()),
                    std::collections::BTreeMap::from([("p_one".to_string(), Vec::new())]),
                    Default::default(),
                )
                .unwrap_err()
                .to_string()
                .contains("must be exactly equal"),
            "the writer must not publish project edges without their durable snapshot receipt"
        );
        actor
            .publish_history_generation(
                first,
                CommitDocumentOwnerV1::unclaimed(namespace.as_str()),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        let replacement = make_generation(vec![retained]);
        let result = actor
            .publish_history_generation(
                replacement.clone(),
                CommitDocumentOwnerV1::unclaimed(namespace.as_str()),
                Default::default(),
                Default::default(),
            )
            .unwrap();
        assert_eq!(result.commit_document_count, 1);
        assert_eq!(
            result.commit_view_commitment,
            replacement.manifest.body.commit_document_commitment_sha256
        );
        super::super::history_transport::verify_history_commit_view(
            &transcript.searcher(),
            transcript.field_handles(),
            &replacement,
        )
        .unwrap();
        let fields = transcript.field_handles();
        let sentinel_query = TermQuery::new(
            Term::from_field_text(fields.entity_id, sentinel_entity),
            IndexRecordOption::Basic,
        );
        assert_eq!(
            transcript
                .searcher()
                .search(&sentinel_query, &Count)
                .unwrap(),
            1,
            "exact Git replacement must not delete code documents sharing the repo id"
        );
    }

    #[test]
    fn snapshot_commit_aborts_and_rolls_back_when_prior_payload_is_unreadable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let transcript = test_index(&root);
        let index = transcript.index_handle();
        let mut writer = index.writer(15_000_000).unwrap();
        let edges_dir = root.join("edges");
        std::fs::create_dir_all(&edges_dir).unwrap();
        let snapshot_id = "snapshot-a";
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            "p1",
            snapshot_id,
            &[("project.jsonl", &[])],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            "p1",
            "repo",
            &"a".repeat(40),
            "generation-a",
            "collected:p1:.:generation-a",
            snapshot_id,
        )
        .unwrap();
        let mut bundle = super::super::project_files::ProjectIndexPublicationBundle::default();
        bundle.stage_snapshot_git_current(&edges_dir, "p1", snapshot_id, false);
        let publication = bundle.publish().unwrap();
        std::fs::write(root.join("idx/meta.json"), b"{broken").unwrap();

        let error =
            commit_snapshot_publications(&index, &mut writer, &edges_dir, publication).unwrap_err();
        assert!(format!("{error:#}").contains("loading prior index payload"));
        let txn_dir =
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join("workspace/p1/txn");
        if txn_dir.is_dir() {
            assert_eq!(std::fs::read_dir(txn_dir).unwrap().count(), 0);
        }
    }

    #[test]
    fn snapshot_commit_rejects_missing_current_journal_and_rolls_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let transcript = test_index(&root);
        let index = transcript.index_handle();
        let mut writer = index.writer(15_000_000).unwrap();
        let edges_dir = root.join("edges");
        std::fs::create_dir_all(&edges_dir).unwrap();
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            "p1",
            "snapshot-a",
            &[("project.jsonl", &[])],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            "p1",
            "repo",
            &"a".repeat(40),
            "generation-a",
            "collected:p1:.:generation-a",
            "snapshot-a",
        )
        .unwrap();
        let mut bundle = super::super::project_files::ProjectIndexPublicationBundle::default();
        bundle.stage_snapshot_git_current(&edges_dir, "p1", "snapshot-a", false);
        let publication = bundle.publish().unwrap();
        let token = publication.pending_snapshot_finalizations[0]
            .txn_token
            .clone();
        let txn_dir =
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join("workspace/p1/txn");
        std::fs::remove_file(txn_dir.join(format!("{token}.journal.json"))).unwrap();

        let error =
            commit_snapshot_publications(&index, &mut writer, &edges_dir, publication).unwrap_err();
        assert!(
            format!("{error:#}").contains("unexpected entry")
                || format!("{error:#}").contains("no exact validated journal")
        );
        assert_eq!(std::fs::read_dir(txn_dir).unwrap().count(), 0);
    }

    #[test]
    fn snapshot_commit_rejects_replaced_current_journal_and_rolls_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let transcript = test_index(&root);
        let index = transcript.index_handle();
        let mut writer = index.writer(15_000_000).unwrap();
        let edges_dir = root.join("edges");
        std::fs::create_dir_all(&edges_dir).unwrap();
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            "p1",
            "snapshot-a",
            &[("project.jsonl", &[])],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            "p1",
            "repo",
            &"a".repeat(40),
            "generation-a",
            "collected:p1:.:generation-a",
            "snapshot-a",
        )
        .unwrap();
        let mut bundle = super::super::project_files::ProjectIndexPublicationBundle::default();
        bundle.stage_snapshot_git_current(&edges_dir, "p1", "snapshot-a", false);
        let publication = bundle.publish().unwrap();
        let handle = &publication.pending_snapshot_finalizations[0];
        let txn_dir =
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join("workspace/p1/txn");
        let journal_path = txn_dir.join(format!("{}.journal.json", handle.txn_token));
        let mut journal: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
        journal["members"][0]["sha256"] = serde_json::Value::String("a".repeat(64));
        std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();

        let error =
            commit_snapshot_publications(&index, &mut writer, &edges_dir, publication).unwrap_err();
        assert!(format!("{error:#}").contains("no exact validated journal"));
        assert_eq!(std::fs::read_dir(txn_dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_commit_reports_failed_precommit_cleanup_as_unresolved() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let transcript = test_index(&root);
        let index = transcript.index_handle();
        let mut writer = index.writer(15_000_000).unwrap();
        let edges_dir = root.join("edges");
        std::fs::create_dir_all(&edges_dir).unwrap();
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            "p1",
            "snapshot-a",
            &[("project.jsonl", &[])],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            "p1",
            "repo",
            &"a".repeat(40),
            "generation-a",
            "collected:p1:.:generation-a",
            "snapshot-a",
        )
        .unwrap();
        let mut bundle = super::super::project_files::ProjectIndexPublicationBundle::default();
        bundle.stage_snapshot_git_current(&edges_dir, "p1", "snapshot-a", false);
        let publication = bundle.publish().unwrap();
        let token = publication.pending_snapshot_finalizations[0]
            .txn_token
            .clone();
        let txn_dir =
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join("workspace/p1/txn");
        std::fs::remove_dir_all(txn_dir.join(&token)).unwrap();
        std::fs::write(txn_dir.join(&token), b"not-a-directory").unwrap();

        let error =
            commit_snapshot_publications(&index, &mut writer, &edges_dir, publication).unwrap_err();
        assert!(format!("{error:#}").contains("rollback also failed"));
        assert!(txn_dir.join(&token).is_file());
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
        let records_provider: Arc<dyn ProjectRecordsProvider> = Arc::new(
            crate::projects::BridgeProjectRecordsProvider::new(projects.clone()),
        );
        let plans = plan_project_sources(
            &index.reindex_config(),
            &records_provider,
            &broker,
            None,
            ProjectLeasePurpose::Reindex,
            &HashMap::new(),
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        let leased = plans[0].access.as_ref().unwrap();
        assert!(leased.local.is_some());
        assert!(leased.git.is_some());

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

        drop(plans);
        let record = records_provider.records_snapshot().records[0].clone();
        let actor = IndexWriterActor::spawn_for_with_checkout_access(
            &index,
            records_provider,
            broker.clone(),
        );
        let store = Arc::new(
            bbox_code_source_store::CodeSourceStore::open(
                &index.reindex_config().code_source_store_path,
                bbox_code_source_store::StoreLimits::default(),
            )
            .unwrap(),
        );
        let staged = actor
            .stage_local_generation(
                bridge_identity(&record.project_id, record.repo_id.as_deref()),
                bbox_corpus_core::identity::PublishedScope::try_new("repo-family", ".").unwrap(),
                store,
            )
            .unwrap();
        drop(staged);
        let health = broker.health();
        let git = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::GitHistory)
            .unwrap();
        let local = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .unwrap();
        assert_eq!(git.granted, 1, "local code staging must not lease history");
        assert_eq!(git.denied, 0);
        assert_eq!(local.granted, 2, "local code staging reuses its walk lease");
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
        let records_provider: Arc<dyn ProjectRecordsProvider> =
            Arc::new(crate::projects::BridgeProjectRecordsProvider::new(projects));
        let actor = IndexWriterActor::spawn_for_with_checkout_access(
            &index,
            records_provider,
            broker.clone(),
        );

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

    /// Build a bridge identity for a staging test. Collected staging is
    /// identity-first, so tests no longer hand it a checkout path.
    fn bridge_identity(project_id: &str, repo_id: Option<&str>) -> CodeProjectIdentity {
        CodeProjectIdentity::from_bridge_record(&attached_record(project_id, repo_id)).unwrap()
    }

    struct CollectedFixture {
        descriptor: bbox_code_source::GenerationDescriptor,
        generation_id: String,
        entries: Vec<bbox_code_source::ManifestEntry>,
        store: Arc<bbox_code_source_store::CodeSourceStore>,
    }

    /// One finalized collected generation in a real store, ready to stage.
    fn collected_fixture(root: &std::path::Path) -> CollectedFixture {
        use sha2::{Digest, Sha256};

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
        CollectedFixture {
            descriptor,
            generation_id: generation.generation_id,
            entries,
            store,
        }
    }

    /// Records provider returning a fixed set of attached rows. The staging
    /// path resolves the version-1 compatibility document fields (`repo_id`,
    /// and the local display root) through this provider, so tests that pin
    /// those fields state the attached record explicitly instead of standing
    /// up a registry.
    struct FixedRecordsProvider(Vec<ProjectRecord>);

    impl ProjectRecordsProvider for FixedRecordsProvider {
        fn records_snapshot(&self) -> bbox_corpus_core::project_record::ProjectRecordsSnapshot {
            bbox_corpus_core::project_record::ProjectRecordsSnapshot::from_bridge_records(
                self.0.clone(),
                1,
            )
        }
    }

    struct GovernedFixedRecordsProvider(Vec<ProjectRecord>);

    impl ProjectRecordsProvider for GovernedFixedRecordsProvider {
        fn records_snapshot(&self) -> bbox_corpus_core::project_record::ProjectRecordsSnapshot {
            bbox_corpus_core::project_record::ProjectRecordsSnapshot::from_bridge_records(
                self.0.clone(),
                1,
            )
        }

        fn code_source_locality_governed(&self, _project_id: &str) -> bool {
            true
        }
    }

    fn deny_all_actor(
        index: &TranscriptIndex,
        records: Vec<ProjectRecord>,
    ) -> (IndexWriterActor, Arc<CheckoutAccessBroker>) {
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        let records_provider: Arc<dyn ProjectRecordsProvider> =
            Arc::new(FixedRecordsProvider(records));
        let actor = IndexWriterActor::spawn_for_with_checkout_access(
            index,
            records_provider,
            broker.clone(),
        );
        (actor, broker)
    }

    fn attached_record(project_id: &str, repo_id: Option<&str>) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.into(),
            repo_id: repo_id.map(str::to_string),
            canonical_path: "/unavailable/remote/project".into(),
            registered_at: "2026-07-22T00:00:00Z".into(),
            is_git_repo: repo_id.is_some(),
            languages: Default::default(),
            aliases: Default::default(),
        }
    }

    #[test]
    fn governed_collected_reindex_does_not_request_local_project_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let fixture = collected_fixture(&root);
        let project_id = "collected-project";
        install_outgoing_collected_state(
            &root,
            &fixture.store,
            project_id,
            &fixture.generation_id,
            &fixture.descriptor,
        );
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access::DenyCheckoutAccess),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        let records_provider: Arc<dyn ProjectRecordsProvider> = Arc::new(
            GovernedFixedRecordsProvider(vec![attached_record(project_id, Some("repo-family"))]),
        );
        let plans = plan_project_sources(
            &index.reindex_config(),
            &records_provider,
            &broker,
            None,
            ProjectLeasePurpose::Reindex,
            &HashMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(matches!(
            plans[0].effective,
            EffectiveSource::Collected { .. }
        ));
        let local = broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .unwrap();
        assert_eq!(local.granted, 0);
        assert_eq!(local.denied, 0);
    }

    /// Phase 3 plan section 6 item 2 (governing section 11, closing F5): the
    /// collected activation transaction opens no Git and acquires NO
    /// checkout lease of any kind. Under a deny-all broker the stage still
    /// succeeds, every lease counter stays at zero, no degradation health is
    /// recorded by staging itself, and no `git-current.jsonl` member is
    /// staged (the post-activation overlay owns that file now).
    #[test]
    fn collected_stage_acquires_zero_leases_and_stages_no_git_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let (actor, broker) = deny_all_actor(
            &index,
            vec![attached_record("collected-project", Some("repo-family"))],
        );
        let fixture = collected_fixture(&root);
        let identity = bridge_identity("collected-project", Some("repo-family"));

        let staged = actor
            .stage_collected_generation(
                identity,
                fixture.descriptor,
                fixture.generation_id,
                fixture.entries,
                fixture.store.clone(),
            )
            .unwrap();
        let snapshot_id = staged.snapshot_id.clone();
        drop(staged);

        let health = broker.health();
        let attempted: u64 = health
            .operations
            .iter()
            .map(|operation| operation.granted + operation.denied)
            .sum();
        assert_eq!(
            attempted, 0,
            "collected staging must acquire no checkout lease at all"
        );
        assert!(
            fixture.store.health_records().unwrap().is_empty(),
            "collected staging no longer records a Git degradation of its own"
        );
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &root.join("projects.json"),
        );
        let snapshot_dir = bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            "collected-project",
            &snapshot_id,
        );
        assert!(
            snapshot_dir.join("project.jsonl").is_file(),
            "the code-edge snapshot member is still published in the transaction"
        );
        assert!(
            !snapshot_dir.join("git-current.jsonl").exists(),
            "the Git overlay member is staged post-activation, not in the transaction"
        );
    }

    /// Fabricate the OUTGOING durable state for an active collected
    /// generation: a manifest entry and activation record naming a synthetic
    /// old materialization suffix and snapshot id, exactly as a pre-bump daemon
    /// left them. `validate_collected_materialization_selector` is shape-only,
    /// so any historic 16-hex suffix is a legitimate past selector.
    ///
    /// `document_count` is 0 by construction: the pass's preservation check
    /// verifies the live document count under the OUTGOING selector against the
    /// activation record, and a fixture cannot mint documents under a
    /// materialization version this binary no longer computes. The migration
    /// logic under test is unaffected - it re-stages from store blobs and
    /// records whatever count the re-stage produces.
    fn install_outgoing_collected_state(
        root: &std::path::Path,
        store: &bbox_code_source_store::CodeSourceStore,
        project_id: &str,
        generation_id: &str,
        descriptor: &bbox_code_source::GenerationDescriptor,
    ) -> (String, String) {
        let outgoing_selector = format!(
            "{}:m{}",
            bbox_code_source::source_selector(project_id, generation_id),
            "0123456789abcdef"
        );
        let outgoing_snapshot = format!("collected-{}", "9".repeat(32));
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &root.join("projects.json"),
        );
        let snapshot_dir =
            bbox_edge_sidecar::snapshot::snapshot_dir(&edges_dir, project_id, &outgoing_snapshot);
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        std::fs::write(snapshot_dir.join("project.jsonl"), b"").unwrap();
        let empty_inventory = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b""));
        store
            .record_materialization(&descriptor.scope, generation_id, 0, empty_inventory.clone())
            .unwrap();
        store
            .save_activation(&bbox_code_source_store::ActivationRecord {
                version: 1,
                project_id: project_id.to_string(),
                generation_id: generation_id.to_string(),
                selector: outgoing_selector.clone(),
                snapshot_id: outgoing_snapshot.clone(),
                document_count: 0,
                entity_inventory_sha256: empty_inventory,
                current_chunk_targets: Default::default(),
                activated_unix_secs: 1,
                cutback_pending: false,
                diagnostic: None,
            })
            .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
            &edges_dir,
            project_id,
            descriptor.scope.repo_id(),
            &descriptor.head_commit,
            generation_id,
            &outgoing_selector,
            &outgoing_snapshot,
            || Ok(()),
        )
        .unwrap();
        (outgoing_selector, outgoing_snapshot)
    }

    /// P3-E materialization migration (plan section 9): a full rebuild after
    /// the paired version bump CONVERGES an active collected generation onto the
    /// current materialization instead of refusing it.
    ///
    /// Before this arm the pass bailed with
    /// `active collected selector requires materialization migration`, which
    /// failed the synchronous schema-migration rebuild and therefore boot -
    /// for exactly the remote-only shape the Phase 3 exit gate asserts.
    #[test]
    fn a_full_rebuild_migrates_an_outgoing_collected_materialization_with_zero_leases() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let fields = index.field_handles();
        let fixture = collected_fixture(&root);
        let project_id = "collected-project";
        let (outgoing_selector, outgoing_snapshot) = install_outgoing_collected_state(
            &root,
            &fixture.store,
            project_id,
            &fixture.generation_id,
            &fixture.descriptor,
        );
        let (actor, broker) = deny_all_actor(
            &index,
            vec![attached_record(project_id, Some("repo-family"))],
        );

        actor
            .run_reindex_pass(true, true)
            .expect("the full rebuild must converge, not refuse");

        let expected_selector =
            bbox_corpus_index::index::project_files::collected_materialization_selector(
                project_id,
                &fixture.generation_id,
            );
        let expected_snapshot =
            bbox_edge_sidecar::snapshot::collected_snapshot_id(project_id, &fixture.generation_id);
        assert_ne!(expected_selector, outgoing_selector);
        assert_ne!(expected_snapshot, outgoing_snapshot);

        // Documents are served under the NEW selector.
        index.reader_reload_for_test();
        let live = |selector: &str| {
            index
                .searcher()
                .search(
                    &TermQuery::new(
                        Term::from_field_text(fields.code_source_selector, selector),
                        IndexRecordOption::Basic,
                    ),
                    &Count,
                )
                .unwrap()
        };
        assert!(
            live(&expected_selector) > 0,
            "the re-staged generation must be served under the current selector"
        );
        assert_eq!(
            live(&outgoing_selector),
            0,
            "no document may remain under the outgoing selector"
        );

        // The activation record moved with it, and carries the NEW inventory.
        let activation = fixture
            .store
            .load_activation(project_id)
            .unwrap()
            .expect("an activation record");
        assert_eq!(activation.selector, expected_selector);
        assert_eq!(activation.snapshot_id, expected_snapshot);
        assert_eq!(activation.generation_id, fixture.generation_id);
        assert!(activation.document_count > 0);
        let generation = fixture
            .store
            .find_generation(&fixture.generation_id)
            .unwrap();
        assert_eq!(
            generation.entity_inventory_sha256.as_deref(),
            Some(activation.entity_inventory_sha256.as_str()),
            "the re-staged inventory must be recorded, or the next full rebuild refuses the project"
        );
        assert_eq!(
            generation.materialized_doc_count,
            Some(activation.document_count)
        );

        // The manifest entry flipped.
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &root.join("projects.json"),
        );
        let entry = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)
            .unwrap()
            .workspaces
            .remove(project_id)
            .expect("a workspace entry");
        assert_eq!(
            entry.code_source_selector.as_deref(),
            Some(expected_selector.as_str())
        );

        // The outgoing selector's retirement is queued.
        assert!(
            fixture
                .store
                .retirement_pending(&outgoing_selector)
                .unwrap(),
            "the outgoing selector must be enqueued for retirement"
        );
        assert!(
            !fixture
                .store
                .retirement_pending(&expected_selector)
                .unwrap(),
            "the incoming selector must never be retired"
        );

        // ZERO GRANTS: the migration read verified store blobs only and never
        // obtained checkout access, which is what lets the remote-only
        // exit-gate shape (a project with no usable attachment at all) migrate.
        // Denials are non-zero and expected: this fixture keeps an attached
        // compat record so the project is planned, and source planning probes
        // its local/Git/publisher/overlay leases before the collected arm runs.
        // The migration itself asks for nothing.
        let health = broker.health();
        let granted: u64 = health
            .operations
            .iter()
            .map(|operation| operation.granted)
            .sum();
        assert_eq!(
            granted, 0,
            "the materialization migration must acquire no checkout lease: {:?}",
            health.operations
        );
    }

    /// The refusals that are NOT a version bump survive the migration arm: an
    /// activation record disagreeing with the manifest still fails the pass.
    #[test]
    fn a_full_rebuild_still_refuses_an_inconsistent_collected_activation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let fixture = collected_fixture(&root);
        let project_id = "collected-project";
        let (_outgoing_selector, outgoing_snapshot) = install_outgoing_collected_state(
            &root,
            &fixture.store,
            project_id,
            &fixture.generation_id,
            &fixture.descriptor,
        );
        // Repoint the MANIFEST at a different generation while the activation
        // record keeps naming the original: internally inconsistent, not merely
        // outgoing. (The store's own activation validator rejects a tampered
        // selector, so the manifest is the tamperable side.)
        let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
            &root.join("projects.json"),
        );
        bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
            &edges_dir,
            project_id,
            fixture.descriptor.scope.repo_id(),
            &fixture.descriptor.head_commit,
            "gen-a-different-generation",
            &format!(
                "{}:m{}",
                bbox_code_source::source_selector(project_id, "gen-a-different-generation"),
                "0123456789abcdef"
            ),
            &outgoing_snapshot,
            || Ok(()),
        )
        .unwrap();

        let (actor, _broker) = deny_all_actor(
            &index,
            vec![attached_record(project_id, Some("repo-family"))],
        );
        let error = actor
            .run_reindex_pass(true, true)
            .err()
            .expect("an inconsistent activation record must still fail closed");
        assert!(
            format!("{error:#}").contains("disagrees with its activation record"),
            "{error:#}"
        );
    }

    /// Phase 3 plan section 6 item 1 (D-034): the typed refusal fires for a
    /// catalog `LegacyLocal` identity and for that alone, BEFORE any writer
    /// work - nothing is staged and the store is untouched.
    #[test]
    fn collected_stage_refuses_a_catalog_legacy_local_identity() {
        use bbox_corpus_core::project_catalog::{CorpusProject, ProjectId, ProjectScope};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let (actor, broker) = deny_all_actor(&index, Vec::new());
        let fixture = collected_fixture(&root);
        let project = CorpusProject {
            project_id: ProjectId::parse("p_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            scope: ProjectScope::LegacyLocal,
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: "legacy-local".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        };
        let identity = CodeProjectIdentity::from_catalog(&project, None);

        let error = actor
            .stage_collected_generation(
                identity,
                fixture.descriptor,
                fixture.generation_id,
                fixture.entries,
                fixture.store.clone(),
            )
            .map(|_| ())
            .expect_err("a catalog LegacyLocal project cannot own a collected generation");
        assert!(
            error
                .to_string()
                .contains("error.collected_source_scope_unavailable"),
            "unexpected refusal diagnostic: {error}"
        );
        let attempted: u64 = broker
            .health()
            .operations
            .iter()
            .map(|operation| operation.granted + operation.denied)
            .sum();
        assert_eq!(attempted, 0, "the refusal precedes every lease decision");
    }

    /// The refusal keys on origin AND scope: a bridge identity carries a
    /// placeholder `LegacyLocal` scope for lack of anywhere else to put "no
    /// catalog scope resolved", and refusing it would break live bridge
    /// collected staging.
    #[test]
    fn collected_stage_proceeds_for_a_bridge_legacy_local_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let (actor, _broker) = deny_all_actor(
            &index,
            vec![attached_record("bridge-project", Some("repo-family"))],
        );
        let fixture = collected_fixture(&root);
        let identity = bridge_identity("bridge-project", Some("repo-family"));
        assert_eq!(
            identity.scope,
            bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal
        );

        let staged = actor
            .stage_collected_generation(
                identity,
                fixture.descriptor,
                fixture.generation_id,
                fixture.entries,
                fixture.store.clone(),
            )
            .expect("every bridge identity proceeds through collected staging");
        assert_eq!(staged.document_count, 1);
    }

    /// The staged documents carry the identity's project id, not a checkout
    /// path (Phase 3 plan section 6 item 5, closing F6). The `project` and
    /// `file_path` fields take the doc builder's absent-display-root
    /// fallback: the project id and the normalized relative path.
    #[test]
    fn collected_documents_are_path_free_after_the_display_root_cut() {
        use tantivy::collector::TopDocs;
        use tantivy::query::TermQuery;
        use tantivy::schema::{IndexRecordOption, Term};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let (actor, _broker) = deny_all_actor(
            &index,
            vec![attached_record("collected-project", Some("repo-family"))],
        );
        let fixture = collected_fixture(&root);
        let identity = bridge_identity("collected-project", Some("repo-family"));
        let fields = index.field_handles();

        let staged = actor
            .stage_collected_generation(
                identity,
                fixture.descriptor,
                fixture.generation_id.clone(),
                fixture.entries,
                fixture.store.clone(),
            )
            .unwrap();
        let selector = staged.selector.clone();
        drop(staged);
        actor.flush_blocking().unwrap();

        let reader = index.index_handle().reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(fields.code_source_selector, &selector),
            IndexRecordOption::Basic,
        );
        let hits = searcher.search(&query, &TopDocs::with_limit(4)).unwrap();
        assert_eq!(hits.len(), 1);
        let document = searcher.doc::<tantivy::TantivyDocument>(hits[0].1).unwrap();
        let text = |field: tantivy::schema::Field| match document.get_first(field) {
            Some(tantivy::schema::OwnedValue::Str(value)) => value.clone(),
            other => panic!("expected a text field, got {other:?}"),
        };
        // The two enumerated changes (plan section 4.3 item 2, step one).
        assert_eq!(text(fields.project), "collected-project");
        assert_eq!(text(fields.file_path), "src/lib.rs");
        // Everything else on the document is unchanged.
        assert_eq!(text(fields.project_id), "collected-project");
        assert_eq!(text(fields.repo_id), "repo-family");
        assert_eq!(text(fields.doc_type), "project_file");
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

    /// Selector retirement on an index with zero documents under the
    /// selector must short-circuit to a zero count: tantivy's `TopDocs`
    /// panics on a zero limit, and the panic previously took the whole
    /// retirement ack with it (surfaced by the phase-2 catalog bootsmoke
    /// on a fresh throwaway index).
    #[test]
    fn selector_retirement_on_an_empty_index_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let actor = IndexWriterActor::spawn_for(&index);
        let retired = actor
            .retire_code_selector("local:00000000".into())
            .expect("empty-index retirement completes");
        assert_eq!(retired.document_count, 0);
    }

    #[test]
    fn interactive_reindex_admission_is_nonblocking_and_single_flight() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = test_index(&root);
        let actor = IndexWriterActor::spawn_for(&index);

        actor.reserve_reindex().unwrap();
        assert!(actor.reindex_in_progress());
        let duplicate = actor.reserve_reindex().unwrap_err();
        assert!(
            duplicate
                .downcast_ref::<IndexWriterRetryableError>()
                .is_some_and(|error| matches!(
                    error,
                    IndexWriterRetryableError::ReindexPassInProgress
                ))
        );
        actor
            .publication_activity
            .store(PUBLICATION_IDLE, Ordering::Release);
        assert!(!actor.reindex_in_progress());

        let edge_rebuild = actor
            .try_begin_edge_index_rebuild()
            .expect("idle publication admits an edge rebuild");
        let blocked = actor
            .request_reindex_pass_accepting_empty(false, true, Vec::new())
            .unwrap_err();
        assert!(
            blocked
                .downcast_ref::<IndexWriterRetryableError>()
                .is_some_and(|error| matches!(
                    error,
                    IndexWriterRetryableError::EdgeIndexRebuildInProgress
                ))
        );
        drop(edge_rebuild);

        actor.reserve_reindex().unwrap();
        assert!(actor.try_begin_edge_index_rebuild().is_none());
        actor
            .publication_activity
            .store(PUBLICATION_IDLE, Ordering::Release);

        let response = actor
            .request_reindex_pass_accepting_empty(false, true, Vec::new())
            .unwrap();
        assert!(response.contains("accepted"));
        actor.flush_blocking().unwrap();
        for _ in 0..100 {
            if !actor.reindex_in_progress() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!actor.reindex_in_progress());
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

    #[test]
    fn expired_generation_hold_refuses_publication() {
        let (_release, release_rx) = mpsc::sync_channel(1);
        let hold_state = AtomicU8::new(STAGE_HOLD_HELD);

        await_generation_stage_release(
            release_rx,
            "test generation",
            &hold_state,
            std::time::Duration::from_millis(1),
        );

        assert_eq!(hold_state.load(Ordering::Acquire), STAGE_HOLD_EXPIRED);
        assert!(begin_generation_publication(&hold_state).is_err());
    }

    #[test]
    fn publication_in_progress_survives_the_bounded_staging_timeout() {
        let (release, release_rx) = mpsc::sync_channel(1);
        let hold_state = Arc::new(AtomicU8::new(STAGE_HOLD_HELD));
        begin_generation_publication(&hold_state).unwrap();
        let waiting_state = hold_state.clone();
        let waiter = std::thread::spawn(move || {
            await_generation_stage_release(
                release_rx,
                "test generation",
                &waiting_state,
                std::time::Duration::from_millis(1),
            );
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!waiter.is_finished());

        hold_state.store(STAGE_HOLD_RELEASED, Ordering::Release);
        release.send(()).unwrap();
        waiter.join().unwrap();
        assert_eq!(hold_state.load(Ordering::Acquire), STAGE_HOLD_RELEASED);
    }
}

/// Phase 3 P3-C source-planning matrix (plan section 7 item 1 gate).
///
/// Each row is one `EffectiveSource` classification plus its
/// lease-acquisition and purge-exemption consequences, driven through
/// `plan_project_sources` against a catalog-shaped records provider: a
/// corpus id set that EXCEEDS the attached rows, which is exactly the shape
/// the bridge cannot produce and the pre-Phase-3 planner could not see (F1).
#[cfg(test)]
mod source_planning_tests {
    use super::*;
    use crate::index::TranscriptIndex;
    use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
    use bbox_corpus_core::project_record::ProjectRecordsSnapshot;
    use std::collections::{BTreeMap, BTreeSet};

    const REMOTE: &str = "p_000000000000000000000000000000b1";
    const GENERATION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct PlanningProvider {
        snapshot: ProjectRecordsSnapshot,
        identities: BTreeMap<String, CodeProjectIdentity>,
        git_transport_governed: BTreeSet<String>,
    }

    impl ProjectRecordsProvider for PlanningProvider {
        fn records_snapshot(&self) -> ProjectRecordsSnapshot {
            self.snapshot.clone()
        }

        fn code_identities(&self) -> BTreeMap<String, CodeProjectIdentity> {
            self.identities.clone()
        }

        fn git_history_transport_governed(&self, project_id: &str) -> bool {
            self.git_transport_governed.contains(project_id)
        }
    }

    struct TestAssignments(BTreeSet<String>);

    impl ProducerAssignmentSource for TestAssignments {
        fn assigned_project_ids(&self) -> BTreeSet<String> {
            self.0.clone()
        }
    }

    fn identity(id: &str) -> CodeProjectIdentity {
        CodeProjectIdentity {
            project_id: ProjectId::parse(id.to_string()).unwrap(),
            scope: ProjectScope::LegacyLocal,
            display_name: format!("display {id}"),
            repo_history: None,
            origin: IdentityOrigin::Catalog,
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        config: ReindexConfig,
        projects: Arc<parking_lot::RwLock<ProjectRegistry>>,
        broker: Arc<CheckoutAccessBroker>,
    }

    impl Fixture {
        /// Register a real checkout so the version-1 access authority can
        /// grant leases against it, returning the record with the id the
        /// registry actually minted.
        fn attach(&self, name: &str, files: &[(&str, &str)]) -> ProjectRecord {
            let root = self.root.join(name);
            std::fs::create_dir_all(root.join(".bbox")).unwrap();
            for (path, contents) in files {
                let path = root.join(path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(path, contents).unwrap();
            }
            self.projects.write().register_path(&root).unwrap()
        }

        fn store(&self) -> bbox_code_source_store::CodeSourceStore {
            bbox_code_source_store::CodeSourceStore::open(
                &self.config.code_source_store_path,
                bbox_code_source_store::StoreLimits::default(),
            )
            .unwrap()
        }

        fn has_health(&self, project_id: &str, code: &str) -> bool {
            self.store()
                .health_records()
                .unwrap()
                .iter()
                .any(|row| row.project_id == project_id && row.code == code)
        }
    }

    fn fixture() -> Fixture {
        fixture_with_broker(true)
    }

    fn deny_all_fixture() -> Fixture {
        fixture_with_broker(false)
    }

    fn fixture_with_broker(grant: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let config = index.reindex_config();
        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(&config.projects_path).unwrap(),
        ));
        let checkouts = Arc::new(parking_lot::RwLock::new(
            crate::checkout_registry::CheckoutRegistry::open(&root.join("checkouts.json")).unwrap(),
        ));
        let broker = if grant {
            Arc::new(CheckoutAccessBroker::new(
                Arc::new(crate::checkout_access_v1::V1CheckoutAccessAuthority::new(
                    projects.clone(),
                    checkouts,
                )),
                crate::checkout_access::CheckoutAccessObservations::in_memory(),
            ))
        } else {
            Arc::new(CheckoutAccessBroker::new(
                Arc::new(crate::checkout_access::DenyCheckoutAccess),
                crate::checkout_access::CheckoutAccessObservations::in_memory(),
            ))
        };
        Fixture {
            _dir: dir,
            root,
            config,
            projects,
            broker,
        }
    }

    fn provider(
        records: Vec<ProjectRecord>,
        corpus_ids: &[&str],
    ) -> Arc<dyn ProjectRecordsProvider> {
        provider_with_git_transport_governed(records, corpus_ids, &[])
    }

    fn provider_with_git_transport_governed(
        records: Vec<ProjectRecord>,
        corpus_ids: &[&str],
        governed: &[&str],
    ) -> Arc<dyn ProjectRecordsProvider> {
        Arc::new(PlanningProvider {
            snapshot: ProjectRecordsSnapshot {
                records: Arc::new(records),
                corpus_project_ids: Arc::new(
                    corpus_ids.iter().map(|id| (*id).to_string()).collect(),
                ),
                omitted_catalog_count: 0,
                authority_epoch: 7,
            },
            identities: corpus_ids
                .iter()
                .map(|id| ((*id).to_string(), identity(id)))
                .collect(),
            git_transport_governed: governed
                .iter()
                .map(|project_id| (*project_id).to_string())
                .collect(),
        })
    }

    fn mark_collected(config: &ReindexConfig, project_id: &str) {
        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
        std::fs::create_dir_all(&edges_dir).unwrap();
        let mut manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir).unwrap();
        manifest.upsert_workspace(
            project_id,
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("{project_id}.json"),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(
                    bbox_corpus_index::index::project_files::collected_materialization_selector(
                        project_id, GENERATION,
                    ),
                ),
                code_source_generation: Some(GENERATION.to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        manifest.write_atomic(&edges_dir).unwrap();
    }

    fn mark_cutback_pending(config: &ReindexConfig, project_id: &str) {
        let store = bbox_code_source_store::CodeSourceStore::open(
            &config.code_source_store_path,
            bbox_code_source_store::StoreLimits::default(),
        )
        .unwrap();
        store
            .save_activation(&bbox_code_source_store::ActivationRecord {
                version: 1,
                project_id: project_id.to_string(),
                generation_id: GENERATION.to_string(),
                selector:
                    bbox_corpus_index::index::project_files::collected_materialization_selector(
                        project_id, GENERATION,
                    ),
                snapshot_id: bbox_edge_sidecar::snapshot::collected_snapshot_id(
                    project_id, GENERATION,
                ),
                document_count: 0,
                entity_inventory_sha256: "b".repeat(64),
                current_chunk_targets: Default::default(),
                activated_unix_secs: 0,
                cutback_pending: true,
                diagnostic: Some("cutback staged".into()),
            })
            .unwrap();
    }

    fn local_meta(project_id: &str) -> HashMap<String, super::super::FileMeta> {
        [(
            "/previous/pass/src/lib.rs".to_string(),
            super::super::FileMeta {
                mtime: 1,
                size: 1,
                mat_version: Some("v1".into()),
                source: super::super::FileMetaSource::LocalProjectFile {
                    project_id: project_id.to_string(),
                    selector: bbox_code_source::local_selector(project_id),
                    relative_path: "src/lib.rs".into(),
                    entry_key: format!("entry-{project_id}"),
                },
            },
        )]
        .into_iter()
        .collect()
    }

    fn plan(
        fixture: &Fixture,
        provider: &Arc<dyn ProjectRecordsProvider>,
        assignments: Option<&Arc<dyn ProducerAssignmentSource>>,
        meta: &HashMap<String, super::super::FileMeta>,
        accept: &BTreeSet<String>,
    ) -> Vec<ProjectSourcePlan> {
        plan_project_sources(
            &fixture.config,
            provider,
            &fixture.broker,
            assignments,
            ProjectLeasePurpose::Reindex,
            meta,
            accept,
        )
        .unwrap()
    }

    fn find<'a>(plans: &'a [ProjectSourcePlan], id: &str) -> &'a ProjectSourcePlan {
        plans
            .iter()
            .find(|plan| plan.project_id == id)
            .expect("planned project")
    }

    #[test]
    fn remote_only_project_plans_collected_with_zero_leases() {
        let fixture = deny_all_fixture();
        mark_collected(&fixture.config, REMOTE);
        let provider = provider(Vec::new(), &[REMOTE]);
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        let remote = find(&plans, REMOTE);
        assert_eq!(
            remote.effective,
            EffectiveSource::Collected {
                generation: GENERATION.to_string()
            }
        );
        assert!(remote.access.is_none(), "collected plan holds no lease");
        assert!(!remote.is_local_scanned());
        assert!(
            remote.lowered().is_some(),
            "a remote-only collected project still reaches the indexer"
        );
        // The exit-gate property: the broker was never called at all for a
        // project with no compatibility record.
        for operation in &fixture.broker.health().operations {
            assert_eq!(operation.granted, 0, "{:?} grants", operation.kind);
            assert_eq!(operation.denied, 0, "{:?} denials", operation.kind);
        }
    }

    #[test]
    fn detached_project_plans_unavailable_and_is_purge_exempt() {
        let fixture = fixture();
        let provider = provider(Vec::new(), &[REMOTE]);
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        assert_eq!(
            find(&plans, REMOTE).effective,
            EffectiveSource::Unavailable {
                reason: UnavailableReason::NoAttachment
            }
        );
        assert!(purge_exempt_project_ids(&plans).contains(REMOTE));
        assert!(
            fixture.has_health(REMOTE, "source_unavailable"),
            "the detached state gets a durable record, not a per-pass warn"
        );
    }

    #[test]
    fn attached_project_plans_local_and_is_not_exempt() {
        let fixture = fixture();
        let record = fixture.attach("attached", &[("lib.rs", "fn main() {}\n")]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        let attached = find(&plans, &id);
        assert_eq!(attached.effective, EffectiveSource::Local);
        assert!(
            attached.access.is_some(),
            "local plans carry the lease bundle"
        );
        assert!(attached.is_local_scanned());
        assert!(!purge_exempt_project_ids(&plans).contains(&id));
        assert!(!fixture.has_health(&id, "source_unavailable"));
    }

    #[test]
    fn transport_governed_reindex_never_attempts_a_git_history_lease() {
        let fixture = fixture();
        let mut record = fixture.attach("covered", &[("lib.rs", "fn main() {}\n")]);
        record.is_git_repo = true;
        record.repo_id = Some("repo-family".to_string());
        let id = record.project_id.clone();
        let provider = provider_with_git_transport_governed(vec![record], &[&id], &[&id]);

        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        let access = find(&plans, &id).access.as_ref().unwrap();
        assert!(access.git.is_none());
        let git_health = fixture
            .broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::GitHistory)
            .unwrap();
        assert_eq!(git_health.granted, 0);
        assert_eq!(git_health.denied, 0);
    }

    #[test]
    fn empty_root_refuses_purge_when_the_prior_pass_recorded_files() {
        let fixture = fixture();
        let record = fixture.attach("attached-empty", &[]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let plans = plan(
            &fixture,
            &provider,
            None,
            &local_meta(&id),
            &BTreeSet::new(),
        );
        let refused = find(&plans, &id);
        assert_eq!(
            refused.effective,
            EffectiveSource::Unavailable {
                reason: UnavailableReason::EmptyRootRefused
            }
        );
        assert!(!refused.is_local_scanned());
        assert!(
            refused.lowered().unwrap().local_root.is_none(),
            "a refused project must never be walked"
        );
        assert!(purge_exempt_project_ids(&plans).contains(&id));
        assert!(fixture.has_health(&id, "empty_root_refused"));
    }

    #[test]
    fn empty_root_with_no_prior_files_is_an_ordinary_local_plan() {
        // The refusal is about losing a known non-empty inventory, not about
        // emptiness: a genuinely new empty project must not be refused.
        let fixture = fixture();
        let record = fixture.attach("fresh-empty", &[]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        assert_eq!(find(&plans, &id).effective, EffectiveSource::Local);
        assert!(!fixture.has_health(&id, "empty_root_refused"));
    }

    #[test]
    fn accept_empty_projects_waives_the_refusal_and_clears_the_record() {
        let fixture = fixture();
        let record = fixture.attach("attached-empty", &[]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let meta = local_meta(&id);

        let plans = plan(&fixture, &provider, None, &meta, &BTreeSet::new());
        assert!(!find(&plans, &id).is_local_scanned());
        assert!(fixture.has_health(&id, "empty_root_refused"));

        let accepted = BTreeSet::from([id.clone()]);
        let plans = plan(&fixture, &provider, None, &meta, &accepted);
        let acknowledged = find(&plans, &id);
        assert_eq!(acknowledged.effective, EffectiveSource::Local);
        assert!(acknowledged.is_local_scanned());
        assert!(!purge_exempt_project_ids(&plans).contains(&id));
        assert!(
            !fixture.has_health(&id, "empty_root_refused"),
            "the acknowledgement clears the record on that pass"
        );
    }

    #[test]
    fn detaching_an_empty_root_project_also_clears_the_record() {
        let fixture = fixture();
        let record = fixture.attach("attached-empty", &[]);
        let id = record.project_id.clone();
        let meta = local_meta(&id);
        let attached_provider = provider(vec![record], &[&id]);
        plan(&fixture, &attached_provider, None, &meta, &BTreeSet::new());
        assert!(fixture.has_health(&id, "empty_root_refused"));

        // Detach: the project leaves Local planning entirely, which is the
        // second operator escape the plan names.
        let detached_provider = provider(Vec::new(), &[&id]);
        let plans = plan(&fixture, &detached_provider, None, &meta, &BTreeSet::new());
        assert_eq!(
            find(&plans, &id).effective,
            EffectiveSource::Unavailable {
                reason: UnavailableReason::NoAttachment
            }
        );
        assert!(!fixture.has_health(&id, "empty_root_refused"));
        assert!(fixture.has_health(&id, "source_unavailable"));
    }

    #[test]
    fn attached_warming_keeps_local_walking_and_leases() {
        let fixture = fixture();
        let record = fixture.attach("warming", &[("lib.rs", "fn main() {}\n")]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let assignments: Arc<dyn ProducerAssignmentSource> =
            Arc::new(TestAssignments(BTreeSet::from([id.clone()])));
        let plans = plan(
            &fixture,
            &provider,
            Some(&assignments),
            &HashMap::new(),
            &BTreeSet::new(),
        );
        let warming = find(&plans, &id);
        assert_eq!(warming.effective, EffectiveSource::Warming);
        assert!(
            warming.is_local_scanned(),
            "warming with a valid local source plans exactly as Local"
        );
        assert!(warming.access.as_ref().unwrap().local.is_some());
        assert!(warming.lowered().unwrap().local_root.is_some());
        assert!(!purge_exempt_project_ids(&plans).contains(&id));
        assert!(!fixture.has_health(&id, "source_unavailable"));
    }

    #[test]
    fn remote_only_warming_is_a_no_op_with_no_health_record() {
        let fixture = deny_all_fixture();
        let provider = provider(Vec::new(), &[REMOTE]);
        let assignments: Arc<dyn ProducerAssignmentSource> =
            Arc::new(TestAssignments(BTreeSet::from([REMOTE.to_string()])));
        let plans = plan(
            &fixture,
            &provider,
            Some(&assignments),
            &HashMap::new(),
            &BTreeSet::new(),
        );
        let warming = find(&plans, REMOTE);
        assert_eq!(warming.effective, EffectiveSource::Warming);
        assert!(!warming.is_local_scanned());
        assert!(warming.access.is_none());
        assert!(purge_exempt_project_ids(&plans).contains(REMOTE));
        assert!(
            fixture.store().health_records().unwrap().is_empty(),
            "a first upload in flight is informational, not a fault"
        );
    }

    #[test]
    fn bridge_shaped_snapshot_reproduces_the_lease_set_including_warming() {
        // Bridge parity row: corpus ids == record ids, so every project is
        // attached, and the warming window must not change the lease set or
        // local freshness.
        let fixture = fixture();
        let record = fixture.attach("bridge", &[("lib.rs", "fn main() {}\n")]);
        let id = record.project_id.clone();
        let provider = provider(vec![record], &[&id]);
        let baseline = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        let assignments: Arc<dyn ProducerAssignmentSource> =
            Arc::new(TestAssignments(BTreeSet::from([id.clone()])));
        let warming = plan(
            &fixture,
            &provider,
            Some(&assignments),
            &HashMap::new(),
            &BTreeSet::new(),
        );
        let shape = |plans: &[ProjectSourcePlan]| {
            plans
                .iter()
                .map(|plan| {
                    let access = plan.access.as_ref().expect("bridge plans are attached");
                    (
                        plan.project_id.clone(),
                        access.publisher_config.is_some(),
                        access.knowledge_overlay.is_some(),
                        access.local.is_some(),
                        access.git.is_some(),
                        plan.is_local_scanned(),
                        plan.lowered().map(|lowered| lowered.local_root.is_some()),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&baseline), shape(&warming));
        assert!(purge_exempt_project_ids(&warming).is_empty());
    }

    #[test]
    fn cutback_pending_is_a_pass_level_no_op() {
        let fixture = fixture();
        let record = fixture.attach("cutback", &[("lib.rs", "fn main() {}\n")]);
        let id = record.project_id.clone();
        mark_collected(&fixture.config, &id);
        mark_cutback_pending(&fixture.config, &id);
        let provider = provider(vec![record], &[&id]);
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        let cutback = find(&plans, &id);
        assert_eq!(cutback.effective, EffectiveSource::CutbackPending);
        assert!(!cutback.is_local_scanned());
        assert!(purge_exempt_project_ids(&plans).contains(&id));
        assert!(
            cutback.lowered().is_none(),
            "the in-flight transition owns the project this pass"
        );
    }

    /// End-to-end F2/H1 closure on the REINDEX pass: a catalog project with
    /// no attachment keeps its documents across both an incremental tick and
    /// a full rebuild. Before Phase 3 the incremental purge deleted them on
    /// the next ordinary tick and `delete_all_documents()` dropped them with
    /// no preservation arm at all, in both cases with no health record.
    #[test]
    fn a_detached_project_survives_an_incremental_tick_and_a_full_rebuild() {
        let fixture = fixture();
        let entry_key = format!("entry-{REMOTE}");
        let index = TranscriptIndex::open_or_create_with_records(
            &fixture.root.join("idx"),
            Vec::new(),
            None,
            fixture.config.projects_path.clone(),
            fixture.config.knowledge_path.clone(),
            fixture.config.threads_path.clone(),
            fixture.config.roadmap_path.clone(),
            std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        {
            let handle = index.index_handle();
            let mut writer: IndexWriter = handle.writer(WRITER_HEAP_SMALL_OPS).unwrap();
            let mut document = tantivy::TantivyDocument::new();
            document.add_text(fields.doc_type, "project_file");
            document.add_text(fields.project_id, REMOTE);
            document.add_text(fields.entity_id, "project_file:detached:fixture");
            document.add_text(
                fields.code_source_selector,
                &bbox_code_source::local_selector(REMOTE),
            );
            document.add_text(fields.code_source_entry_key, &entry_key);
            document.add_text(fields.content, "detached preservation fixture");
            document.add_text(fields.file_path, "/gone/src/lib.rs");
            writer.add_document(document).unwrap();
            writer.commit().unwrap();
        }
        index.reader_reload_for_test();

        // P3-E: project freshness rows are keyed by the composite
        // `pf\0<project_id>\0<source_kind>\0<relative_path>`, never by the
        // checkout absolute path (plan section 4.6).
        let meta_key = bbox_code_source::project_file_meta_key(
            REMOTE,
            bbox_code_source::SOURCE_KIND_LOCAL,
            "src/lib.rs",
        );
        let meta: HashMap<String, super::super::FileMeta> = [(
            meta_key.clone(),
            super::super::FileMeta {
                mtime: 1,
                size: 1,
                mat_version: Some("v1".into()),
                source: super::super::FileMetaSource::LocalProjectFile {
                    project_id: REMOTE.to_string(),
                    selector: bbox_code_source::local_selector(REMOTE),
                    relative_path: "src/lib.rs".into(),
                    entry_key: entry_key.clone(),
                },
            },
        )]
        .into_iter()
        .collect();
        bbox_corpus_index::index::passes::save_meta(&index.reindex_config().meta_path, &meta)
            .unwrap();

        let provider = provider(Vec::new(), &[REMOTE]);
        let actor = IndexWriterActor::spawn_for_with_checkout_access(
            &index,
            provider,
            fixture.broker.clone(),
        );

        let live = |index: &TranscriptIndex| {
            index.reader_reload_for_test();
            index
                .searcher()
                .search(
                    &TermQuery::new(
                        Term::from_field_text(fields.code_source_entry_key, &entry_key),
                        IndexRecordOption::Basic,
                    ),
                    &Count,
                )
                .unwrap()
        };

        actor.run_reindex_pass(false, true).unwrap();
        assert_eq!(live(&index), 1, "incremental tick must not purge (H2)");

        actor.run_reindex_pass(true, true).unwrap();
        assert_eq!(live(&index), 1, "full rebuild must preserve (H1)");
        assert!(
            bbox_corpus_index::index::passes::load_meta(&index.reindex_config().meta_path)
                .unwrap()
                .contains_key(&meta_key),
            "the freshness inventory that verified the preservation must survive it"
        );
    }

    /// P3-E convergence row (plan section 9 gate): for a LegacyLocal bridge
    /// fixture, the document set after an incremental tick equals the set a
    /// full rebuild produces, and both are path-free.
    ///
    /// This is the row that catches a composite-rekey mistake. The purge loop
    /// diffs meta keys against the pass's current key set; if the scan emitted
    /// composite keys while the meta map still held absolute ones (or the other
    /// way round), every project row would look stale and the incremental tick
    /// would delete the whole project. Equality across the two pass shapes is
    /// the only assertion that fails loudly for that.
    #[test]
    fn incremental_equals_full_for_a_legacy_local_fixture() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project_root = root.join("project");
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        std::fs::create_dir_all(project_root.join(".bbox")).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.test"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&project_root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::write(
            project_root.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-family\"\naliases = [\"acme-service\"]\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join("src/lib.rs"),
            "pub struct Helper;\n\npub fn helper() {}\n",
        )
        .unwrap();
        std::fs::write(project_root.join("README.md"), "# fixture\n\nprose\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "fixture"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&project_root)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }

        let projects = Arc::new(parking_lot::RwLock::new(
            ProjectRegistry::open(root.join("projects.json")).unwrap(),
        ));
        projects.write().register_path(&project_root).unwrap();
        let checkouts = Arc::new(parking_lot::RwLock::new(
            crate::checkout_registry::CheckoutRegistry::open(&root.join("checkout-registry.json"))
                .unwrap(),
        ));
        let broker = Arc::new(CheckoutAccessBroker::new(
            Arc::new(crate::checkout_access_v1::V1CheckoutAccessAuthority::new(
                projects.clone(),
                checkouts,
            )),
            crate::checkout_access::CheckoutAccessObservations::in_memory(),
        ));
        let index = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(bbox_corpus_index::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let records_provider: Arc<dyn ProjectRecordsProvider> = Arc::new(
            crate::projects::BridgeProjectRecordsProvider::new(projects.clone()),
        );
        let actor = IndexWriterActor::spawn_for_with_checkout_access(
            &index,
            records_provider,
            broker.clone(),
        );

        // (entity_id, project, relative_path) for every project-file document,
        // sorted. The triple is the whole point: identity plus the two fields
        // the cut redefined.
        let snapshot = |index: &TranscriptIndex| -> Vec<(String, String, String)> {
            index.reader_reload_for_test();
            let searcher = index.searcher();
            let query = TermQuery::new(
                Term::from_field_text(fields.doc_type, "project_file"),
                IndexRecordOption::Basic,
            );
            let count = searcher.search(&query, &Count).unwrap();
            if count == 0 {
                return Vec::new();
            }
            let mut rows = searcher
                .search(&query, &tantivy::collector::TopDocs::with_limit(count))
                .unwrap()
                .into_iter()
                .map(|(_, address)| {
                    let doc: tantivy::TantivyDocument = searcher.doc(address).unwrap();
                    (
                        bbox_corpus_index::index::first_text(&doc, fields.entity_id),
                        bbox_corpus_index::index::first_text(&doc, fields.project),
                        bbox_corpus_index::index::first_text(&doc, fields.relative_path),
                    )
                })
                .collect::<Vec<_>>();
            rows.sort();
            rows
        };

        actor.run_reindex_pass(true, true).unwrap();
        let full = snapshot(&index);
        assert!(!full.is_empty(), "the full rebuild indexed the fixture");

        // Two incremental ticks: the first exercises the skip path against the
        // freshness rows the full pass wrote, the second proves idempotence.
        actor.run_reindex_pass(false, true).unwrap();
        actor.run_reindex_pass(false, true).unwrap();
        assert_eq!(snapshot(&index), full, "incremental must equal full");

        // And a second full rebuild reproduces the same set.
        actor.run_reindex_pass(true, true).unwrap();
        assert_eq!(snapshot(&index), full, "a repeat full rebuild is stable");

        for (entity_id, project, relative_path) in &full {
            assert!(
                !project.starts_with('/') && !project.contains(root.to_str().unwrap()),
                "`project` must be the display value, not a host path: {project}"
            );
            assert!(
                !relative_path.starts_with('/'),
                "`relative_path` must be relative: {relative_path}"
            );
            assert!(
                !entity_id.contains('/') || entity_id.starts_with("project_file"),
                "unexpected entity id shape: {entity_id}"
            );
        }
        assert!(
            full.iter().any(|(_, _, path)| path == "src/lib.rs"),
            "the fixture's source file is present by relative path: {full:?}"
        );

        // Every project freshness row is composite-keyed, and none is an
        // absolute path.
        let meta =
            bbox_corpus_index::index::passes::load_meta(&index.reindex_config().meta_path).unwrap();
        let project_rows = meta
            .iter()
            .filter(|(_, row)| {
                matches!(
                    row.source,
                    super::super::FileMetaSource::LocalProjectFile { .. }
                )
            })
            .collect::<Vec<_>>();
        assert!(!project_rows.is_empty(), "freshness rows were written");
        for (key, _row) in project_rows {
            assert!(
                bbox_code_source::parse_project_file_meta_key(key).is_some(),
                "a project freshness row must carry the composite key: {key}"
            );
            assert!(
                !key.starts_with('/'),
                "a project freshness row must not be keyed by an absolute path: {key}"
            );
        }
    }

    #[test]
    fn identity_absent_for_a_corpus_id_plans_unavailable_rather_than_vanishing() {
        let fixture = fixture();
        let provider: Arc<dyn ProjectRecordsProvider> = Arc::new(PlanningProvider {
            snapshot: ProjectRecordsSnapshot {
                records: Arc::new(Vec::new()),
                corpus_project_ids: Arc::new(BTreeSet::from([REMOTE.to_string()])),
                omitted_catalog_count: 0,
                authority_epoch: 1,
            },
            identities: BTreeMap::new(),
            git_transport_governed: BTreeSet::new(),
        });
        let plans = plan(&fixture, &provider, None, &HashMap::new(), &BTreeSet::new());
        assert!(matches!(
            find(&plans, REMOTE).effective,
            EffectiveSource::Unavailable {
                reason: UnavailableReason::IdentityUnavailable(_)
            }
        ));
        assert!(purge_exempt_project_ids(&plans).contains(REMOTE));
    }
}
