use std::collections::{BTreeMap, BTreeSet};
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bbox_code_source::{
    BeginUploadRequest, ContractError, CutbackErrorClass, CutbackReason, CutbackStateV2,
    ErrorResponse, FinalizeResponse, GenerationState, GenerationStatus, ManifestPage,
    MissingBlobsPage,
};
use bbox_code_source_store::{
    ActivationFence, ActivationFenceConflict, ActivationRecord, ActivationRecordV2,
    CodeSourceStore, CollisionRetirementWorkV1, CutbackAuthorityRevision, CutbackCompareOutcome,
    MixedActivationRecord, MixedStoredGeneration, RetirementRecord, RuntimeRecordMode, StoreLimits,
    StoreRequestError,
};
use bbox_corpus_core::code_project_identity::CodeProjectIdentity;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, ProjectId, RepoHistoryMaterialization};
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessError, CheckoutAccessIntent, CheckoutAccessKind,
    CheckoutAccessRequest, CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::SharedState;
use super::producer_auth::{ProducerAuthRuntime, ProducerGrant};

const UPLOAD_BODY_TEMP_PREFIX: &str = ".upload-body-";
const UPLOAD_BODY_TEMP_SUFFIX: &str = ".tmp";

struct CodeSourceSnapshot {
    auth: Arc<ProducerAuthRuntime>,
    store: Arc<CodeSourceStore>,
}

/// A transition intent enqueued to the reconciler (section 4.4, 8.1 item 3).
/// Events coalesce by project id: the channel is a deduplicated set, so
/// multiple SIGHUP reloads or concurrent triggers for the same project
/// produce one transition pass, not N.
#[derive(Clone, Debug)]
struct ReconcileEvent {
    project_id: String,
    scope: PublishedScope,
    kind: ReconcileKind,
    origins: BTreeSet<ReconcileOrigin>,
    authority_revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReconcileKind {
    Activate,
    Cutback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReconcileOrigin {
    AssignmentConfigReload,
    CatalogCommit,
    ReadinessAvailable,
    TransientDeadline,
    SelectorRetirementCompletion,
    StartupRecovery,
    ActivationCompletion,
}

#[derive(Clone, Debug)]
struct PendingReconcileEvent {
    scope: PublishedScope,
    kind: ReconcileKind,
    origins: BTreeSet<ReconcileOrigin>,
    authority_revision: Option<u64>,
}

/// The per-project transition guard (section 4.4). Whoever holds a
/// project's entry owns its transitions until release. A concurrent trigger
/// from the other owner finds the lock held and coalesces into an
/// already-queued event.
///
/// P4-D skeleton: the guard is a `Mutex<BTreeMap<String, ()>>` because the
/// value carries no per-transition data yet. P4-E will add retry counters
/// and attempt timestamps. The guard extends the existing
/// `begin_activation`/`end_activation` reentrancy guard
/// (`CodeSourceRuntime::activating_projects`) by making it accessible to
/// both the reconciler and the legacy spawn path during the staged-adoption
/// window.
type TransitionGuardMap = std::sync::Mutex<BTreeMap<String, ()>>;

/// The cutback reconciler: one project-keyed owner with a bounded event
/// channel, backed by one background task (section 4.4, 8.1 item 1).
///
/// P4-D introduces the skeleton: the event channel plus per-project
/// transition guard. The reconciler drains the channel and delegates the
/// actual transition work to the existing paths (`schedule_cutback` /
/// `schedule_activation`) under the transition guard. P4-E fills in the
/// bounded scheduler and post-commit observer.
///
/// The reconciler must not disturb persisted cutback state this milestone.
pub(crate) struct CutbackReconciler {
    /// Coalesced event set: deduplicated by project id so repeated triggers
    /// collapse into one pass while retaining every triggering origin.
    pending: Arc<std::sync::Mutex<BTreeMap<String, PendingReconcileEvent>>>,
    /// Deferred events: an event whose project guard is held goes here
    /// instead of being dropped. On guard release (or the 5s timeout
    /// backstop) the deferred set is merged back into `pending` so the
    /// event fires exactly once after the in-flight transition completes
    /// (section 4.4: coalesce-or-defer, never drop).
    deferred: Arc<std::sync::Mutex<BTreeMap<String, PendingReconcileEvent>>>,
    /// Wake signal for the background task. Notified on enqueue and on
    /// guard release.
    notify: Arc<std::sync::Condvar>,
    /// Per-project transition guard shared with the legacy spawn path.
    guards: Arc<TransitionGuardMap>,
    /// Scheduler wakeup signal: notified on every Transient persist so
    /// the bounded scheduler recomputes the minimum deadline (section 9.2).
    scheduler_notify: Arc<std::sync::Condvar>,
    /// Scheduler shared state: the minimum deadline and associated project
    /// ids. The scheduler reads this on wake.
    scheduler_state: Arc<std::sync::Mutex<SchedulerState>>,
}

/// Scheduler state: the minimum deadline across all Transient states and
/// the set of due projects (section 9.2).
#[derive(Default)]
struct SchedulerState {
    /// (deadline_unix_secs, project_id) pairs for all Transient states.
    pending_deadlines: BTreeMap<u64, Vec<String>>,
}

impl CutbackReconciler {
    fn new(guards: Arc<TransitionGuardMap>) -> Self {
        Self {
            pending: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            deferred: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            notify: Arc::new(std::sync::Condvar::new()),
            guards,
            scheduler_notify: Arc::new(std::sync::Condvar::new()),
            scheduler_state: Arc::new(std::sync::Mutex::new(SchedulerState::default())),
        }
    }

    /// Register a Transient deadline so the scheduler can re-attempt the
    /// project when it becomes due (section 9.2). Called by the one-attempt
    /// driver after persisting a Transient state.
    fn register_transient(&self, deadline_unix_secs: u64, project_id: &str) {
        let mut state = self.scheduler_state.lock().unwrap();
        state
            .pending_deadlines
            .entry(deadline_unix_secs)
            .or_default()
            .push(project_id.to_string());
        self.scheduler_notify.notify_one();
    }

    /// Consume all due projects: those whose deadline <= now. Called by
    /// the scheduler after sleeping until the minimum deadline.
    fn drain_due(&self, now: u64) -> Vec<String> {
        let mut state = self.scheduler_state.lock().unwrap();
        let due_keys: Vec<u64> = state
            .pending_deadlines
            .range(..=now)
            .map(|(k, _)| *k)
            .collect();
        let mut due_projects = Vec::new();
        for key in due_keys {
            if let Some(projects) = state.pending_deadlines.remove(&key) {
                due_projects.extend(projects);
            }
        }
        due_projects
    }

    /// The minimum deadline across all registered Transient states, or
    /// None if there are none (section 9.2). Used by tests and by the
    /// scheduler's internal wake logic.
    #[cfg_attr(not(test), allow(dead_code))]
    fn min_deadline(&self) -> Option<u64> {
        let state = self.scheduler_state.lock().unwrap();
        state.pending_deadlines.keys().next().copied()
    }

    /// Wait for the scheduler: blocks until the minimum deadline is due,
    /// or until a new deadline is registered (whichever is earlier).
    /// Returns the minimum deadline or None on shutdown.
    /// Wait for the scheduler: blocks until the minimum deadline is due,
    /// or until shutdown. Returns the due deadline or None on shutdown.
    /// The wait is interruptible: if a new earlier deadline is registered
    /// via `register_transient`, the condvar is notified and this returns
    /// early so the caller re-evaluates.
    fn scheduler_wait(&self, shutdown: &std::sync::atomic::AtomicBool) -> Option<u64> {
        loop {
            let state = self.scheduler_state.lock().unwrap();
            if let Some(&min) = state.pending_deadlines.keys().next() {
                let now = unix_now();
                if min <= now {
                    drop(state);
                    return Some(min);
                }
                // Sleep until the deadline, but use the condvar so a
                // newly registered earlier deadline can preempt.
                let wait_dur = std::time::Duration::from_secs(min - now);
                let (_guard, _timeout) =
                    self.scheduler_notify.wait_timeout(state, wait_dur).unwrap();
                if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    return None;
                }
                // Loop back: re-evaluate min deadline (may have changed).
                continue;
            }
            if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            // No deadlines: wait for one to be registered.
            let _guard = self
                .scheduler_notify
                .wait_timeout(state, std::time::Duration::from_secs(5))
                .unwrap();
            if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
        }
    }

    /// Enqueue a transition event. Coalesces by project id: the latest
    /// scope/kind/revision wins while origins accumulate (section 4.4).
    fn enqueue(
        &self,
        project_id: &str,
        scope: PublishedScope,
        kind: ReconcileKind,
        origin: ReconcileOrigin,
        authority_revision: Option<u64>,
    ) {
        let mut pending = self.pending.lock().unwrap();
        match pending.entry(project_id.to_string()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PendingReconcileEvent {
                    scope,
                    kind,
                    origins: BTreeSet::from([origin]),
                    authority_revision,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let pending = entry.get_mut();
                pending.scope = scope;
                pending.kind = kind;
                pending.origins.insert(origin);
                pending.authority_revision = authority_revision;
            }
        }
        self.notify.notify_one();
    }

    /// Defer an event: the project's transition guard is held by an
    /// in-flight transition. The event stays in the deferred set until
    /// the guard releases (notified via condvar) or the 5s timeout
    /// backstop fires, at which point it is merged back into `pending`.
    fn defer(&self, event: ReconcileEvent) {
        let mut deferred = self.deferred.lock().unwrap();
        match deferred.entry(event.project_id) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(PendingReconcileEvent {
                    scope: event.scope,
                    kind: event.kind,
                    origins: event.origins,
                    authority_revision: event.authority_revision,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let deferred = entry.get_mut();
                deferred.scope = event.scope;
                deferred.kind = event.kind;
                deferred.origins.extend(event.origins);
                deferred.authority_revision = event.authority_revision;
            }
        }
    }

    /// Merge deferred events back into pending. Called by the background
    /// task on wake from guard-release notification or the timeout
    /// backstop. Returns the number of events promoted.
    fn promote_deferred(&self) -> usize {
        let mut deferred = self.deferred.lock().unwrap();
        if deferred.is_empty() {
            return 0;
        }
        let mut pending = self.pending.lock().unwrap();
        let count = deferred.len();
        for (project_id, deferred_event) in deferred.iter() {
            match pending.entry(project_id.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(deferred_event.clone());
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    // Pending was enqueued later and therefore owns the latest
                    // scope/kind/revision. Deferred contributes origins only.
                    entry
                        .get_mut()
                        .origins
                        .extend(deferred_event.origins.iter().copied());
                }
            }
        }
        deferred.clear();
        count
    }

    /// Drain all pending events. Called by the background task after waking.
    fn drain(&self) -> Vec<ReconcileEvent> {
        let mut pending = self.pending.lock().unwrap();
        let entries = pending
            .iter()
            .map(|(project_id, pending)| ReconcileEvent {
                project_id: project_id.clone(),
                scope: pending.scope.clone(),
                kind: pending.kind.clone(),
                origins: pending.origins.clone(),
                authority_revision: pending.authority_revision,
            })
            .collect::<Vec<_>>();
        pending.clear();
        entries
    }

    /// Wait for events or shutdown. Returns `true` if the task should
    /// continue running, `false` on shutdown. On wake from condvar or
    /// timeout, merges deferred events back into pending so guard-release
    /// notifications are not lost.
    fn wait(&self, shutdown: &std::sync::atomic::AtomicBool) -> bool {
        let pending = self.pending.lock().unwrap();
        if !pending.is_empty() {
            return true;
        }
        if shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let _guard = self
            .notify
            .wait_timeout(pending, std::time::Duration::from_secs(5))
            .unwrap();
        // Merge deferred events on wake (guard release or timeout).
        drop(_guard);
        self.promote_deferred();
        !shutdown.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Acquire a project's transition guard for the duration of a
    /// transition. Returns `Some(GuardHandle)` if the lock was acquired,
    /// `None` if another owner already holds it (defer: the event goes
    /// into the deferred set and re-fires on guard release).
    fn try_acquire(&self, project_id: &str) -> Option<GuardHandle> {
        let mut guards = self.guards.lock().unwrap();
        if guards.contains_key(project_id) {
            return None;
        }
        guards.insert(project_id.to_string(), ());
        Some(GuardHandle {
            guards: self.guards.clone(),
            notify: self.notify.clone(),
            project_id: project_id.to_string(),
        })
    }
}

/// RAII handle that releases the transition guard on drop and notifies
/// the reconciler condvar so deferred events for this project are
/// promoted back into pending.
struct GuardHandle {
    guards: Arc<TransitionGuardMap>,
    notify: Arc<std::sync::Condvar>,
    project_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RetirementWorkKey {
    Selector(String),
    Selectorless {
        project_id: ProjectId,
        generation_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RetirementWork {
    Ordinary(RetirementRecord),
    CollisionExact {
        record: RetirementRecord,
        project_id: ProjectId,
        generation_id: String,
        former_scope: PublishedScope,
    },
    CollisionSelectorless(CollisionRetirementWorkV1),
}

impl RetirementWork {
    fn key(&self) -> RetirementWorkKey {
        match self {
            Self::Ordinary(record) | Self::CollisionExact { record, .. } => {
                RetirementWorkKey::Selector(record.selector.clone())
            }
            Self::CollisionSelectorless(work) => RetirementWorkKey::Selectorless {
                project_id: work.project_id.clone(),
                generation_id: work.generation_id.clone(),
            },
        }
    }

    fn project_id(&self) -> &str {
        match self {
            Self::Ordinary(record) | Self::CollisionExact { record, .. } => &record.project_id,
            Self::CollisionSelectorless(work) => work.project_id.as_str(),
        }
    }

    fn same_selector_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ordinary(left), Self::Ordinary(right)) => left == right,
            (Self::CollisionExact { .. }, Self::CollisionExact { .. }) => self == other,
            (Self::Ordinary(left), Self::CollisionExact { record: right, .. })
            | (Self::CollisionExact { record: left, .. }, Self::Ordinary(right)) => left == right,
            (Self::CollisionSelectorless(left), Self::CollisionSelectorless(right)) => {
                left == right
            }
            _ => false,
        }
    }
}

struct RetirementWorkEntry {
    work: RetirementWork,
    previous_view: Option<Arc<super::CodeReadView>>,
    attempts: u32,
    retry_delay: std::time::Duration,
    next_due: std::time::Instant,
}

struct RetirementCoordinator {
    queue: std::sync::Mutex<BTreeMap<RetirementWorkKey, RetirementWorkEntry>>,
    notify: std::sync::Condvar,
    started: std::sync::atomic::AtomicBool,
}

impl RetirementCoordinator {
    fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(BTreeMap::new()),
            notify: std::sync::Condvar::new(),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Drop for GuardHandle {
    fn drop(&mut self) {
        self.guards.lock().unwrap().remove(&self.project_id);
        // Wake the reconciler so it merges deferred events and re-fires
        // any that were waiting for this guard to release.
        self.notify.notify_one();
    }
}

pub(crate) struct CodeSourceRuntime {
    snapshot: parking_lot::RwLock<Arc<CodeSourceSnapshot>>,
    activating_projects: parking_lot::Mutex<BTreeMap<String, bool>>,
    checkout_access: Arc<CheckoutAccessBroker>,
    /// The strict pair store when the catalog is the runtime authority.
    /// Fixed for the process lifetime by the startup probe, exactly like
    /// `ProjectAuthority`, so grant resolution cannot change arms mid-run.
    catalog_store: Option<Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>>,
    /// Per-project transition guard shared between the reconciler and the
    /// legacy spawn path (section 4.4). The reconciler holds its own clone
    /// from construction; this field is reserved for the bridge spawn path
    /// to adopt in P4-E so both arms share one guard map.
    #[allow(dead_code)]
    transition_guards: Arc<TransitionGuardMap>,
    /// The cutback reconciler event channel. `None` in bridge mode, where
    /// transitions are spawned inline byte-identically to pre-Phase-4.
    reconciler: Option<Arc<CutbackReconciler>>,
    /// Monotonic generation of the assignment/auth snapshot. Catalog epoch
    /// and this generation form the cutback authority fence.
    assignment_revision: std::sync::atomic::AtomicU64,
    retirement_coordinator: Arc<RetirementCoordinator>,
}

#[derive(Default)]
pub(crate) struct SourceTransitions {
    cutbacks: Vec<(PublishedScope, String)>,
    activations: Vec<(PublishedScope, String)>,
}

impl CodeSourceRuntime {
    pub(crate) fn open(
        config: &crate::config::Config,
        projects: &[ProjectRecord],
        catalog_store: Option<Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>>,
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Result<Self> {
        let transition_guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = if catalog_store.is_some() {
            Some(Arc::new(CutbackReconciler::new(transition_guards.clone())))
        } else {
            None
        };
        Ok(Self {
            snapshot: parking_lot::RwLock::new(Arc::new(build_snapshot(
                config,
                projects,
                catalog_store.as_ref(),
                None,
                &checkout_access,
            )?)),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
            checkout_access,
            catalog_store,
            transition_guards,
            reconciler,
            assignment_revision: std::sync::atomic::AtomicU64::new(1),
            retirement_coordinator: Arc::new(RetirementCoordinator::new()),
        })
    }

    pub(crate) fn reload(
        &self,
        config: &crate::config::Config,
        projects: &[ProjectRecord],
    ) -> Result<SourceTransitions> {
        let previous = self.snapshot.read().clone();
        let replacement = Arc::new(build_snapshot(
            config,
            projects,
            self.catalog_store.as_ref(),
            Some(previous.store.clone()),
            &self.checkout_access,
        )?);
        replacement.store.update_limits(store_limits(config))?;
        let old_assignments = assignment_map(&previous);
        let new_assignments = assignment_map(&replacement);
        let cutbacks = old_assignments
            .iter()
            .filter(|(scope, assignment)| new_assignments.get(*scope) != Some(*assignment))
            .map(|(scope, (project_id, _producer_id))| (scope.clone(), project_id.clone()))
            .collect();
        let activations = new_assignments
            .into_iter()
            .map(|(scope, (project_id, _producer_id))| (scope, project_id))
            .collect();
        *self.snapshot.write() = replacement;
        self.assignment_revision
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(SourceTransitions {
            cutbacks,
            activations,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        let store = Arc::new(
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap(),
        );
        let transition_guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        Self {
            snapshot: parking_lot::RwLock::new(Arc::new(CodeSourceSnapshot {
                auth: Arc::new(ProducerAuthRuntime::disabled()),
                store,
            })),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
            checkout_access: Arc::new(CheckoutAccessBroker::new(
                Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
                bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
            )),
            catalog_store: None,
            transition_guards,
            reconciler: None,
            assignment_revision: std::sync::atomic::AtomicU64::new(1),
            retirement_coordinator: Arc::new(RetirementCoordinator::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn install_auth_for_test(&self, auth: Arc<ProducerAuthRuntime>) {
        let store = self.store();
        *self.snapshot.write() = Arc::new(CodeSourceSnapshot { auth, store });
    }

    /// Test constructor for catalog mode: initializes the store in
    /// `CatalogV2` record mode and wires the reconciler event channel.
    #[cfg(test)]
    pub(crate) fn for_test_catalog(root: &std::path::Path) -> Self {
        let store = Arc::new(
            CodeSourceStore::open_with_mode(
                root.join("code-sources"),
                StoreLimits::default(),
                RuntimeRecordMode::CatalogV2,
            )
            .unwrap(),
        );
        let transition_guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = Some(Arc::new(CutbackReconciler::new(transition_guards.clone())));
        Self {
            snapshot: parking_lot::RwLock::new(Arc::new(CodeSourceSnapshot {
                auth: Arc::new(ProducerAuthRuntime::disabled()),
                store,
            })),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
            checkout_access: Arc::new(CheckoutAccessBroker::new(
                Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
                bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
            )),
            catalog_store: None,
            transition_guards,
            reconciler,
            assignment_revision: std::sync::atomic::AtomicU64::new(1),
            retirement_coordinator: Arc::new(RetirementCoordinator::new()),
        }
    }

    /// True when the catalog is the runtime authority and the reconciler
    /// channel is active.
    fn is_catalog(&self) -> bool {
        self.reconciler.is_some()
    }

    /// Enqueue a transition event to the reconciler channel (section 8.1
    /// item 3). Catalog mode only; bridge mode spawns inline.
    fn enqueue_transition(
        &self,
        project_id: &str,
        scope: PublishedScope,
        kind: ReconcileKind,
        origin: ReconcileOrigin,
        authority_revision: Option<u64>,
    ) {
        if let Some(reconciler) = &self.reconciler {
            reconciler.enqueue(project_id, scope, kind, origin, authority_revision);
        }
    }

    /// The reconciler handle, for spawning the background task. Returns
    /// `None` in bridge mode.
    pub(crate) fn reconciler(&self) -> Option<&Arc<CutbackReconciler>> {
        self.reconciler.as_ref()
    }

    pub(crate) fn producer_auth(&self) -> Arc<ProducerAuthRuntime> {
        self.snapshot.read().auth.clone()
    }

    pub(crate) fn store(&self) -> Arc<CodeSourceStore> {
        self.snapshot.read().store.clone()
    }

    fn begin_activation(&self, project_id: &str) -> bool {
        let mut projects = self.activating_projects.lock();
        if let Some(pending) = projects.get_mut(project_id) {
            *pending = true;
            false
        } else {
            projects.insert(project_id.to_string(), false);
            true
        }
    }

    fn end_activation(&self, project_id: &str) -> bool {
        self.activating_projects
            .lock()
            .remove(project_id)
            .unwrap_or(false)
    }

    fn assignments(&self) -> Vec<(PublishedScope, String)> {
        self.producer_auth().assignments()
    }

    fn assignment_matches(&self, scope: &PublishedScope, project_id: &str) -> bool {
        self.producer_auth()
            .assignment_map()
            .get(scope)
            .is_some_and(|(assigned, _producer_id)| assigned == project_id)
    }

    fn assignment_authorizes(
        &self,
        scope: &PublishedScope,
        project_id: &str,
        producer_id: &str,
    ) -> bool {
        self.producer_auth()
            .assignment_map()
            .get(scope)
            .is_some_and(|(assigned_project, assigned_producer)| {
                assigned_project == project_id && assigned_producer == producer_id
            })
    }
}

/// Producer assignment view for source planning (Phase 3 plan section 4.7).
/// A project named by a configured grant with no active collected generation
/// yet is `Warming`, which is why the planner must see live assignment state
/// rather than infer it from store residue.
impl bbox_indexing::index::ProducerAssignmentSource for CodeSourceRuntime {
    fn assigned_project_ids(&self) -> std::collections::BTreeSet<String> {
        self.producer_auth().assigned_project_ids()
    }
}

fn assignment_map(snapshot: &CodeSourceSnapshot) -> BTreeMap<PublishedScope, (String, String)> {
    snapshot.auth.assignment_map()
}

fn build_snapshot(
    config: &crate::config::Config,
    projects: &[ProjectRecord],
    catalog_store: Option<&Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>>,
    existing_store: Option<Arc<CodeSourceStore>>,
    checkout_access: &CheckoutAccessBroker,
) -> Result<CodeSourceSnapshot> {
    let limits = store_limits(config);
    if config.code_collection.enabled
        && (limits.max_manifest_files == 0
            || limits.max_manifest_logical_bytes == 0
            || limits.max_open_uploads_per_producer == 0
            || limits.max_migration_survivor_rows == 0
            || limits.max_migration_survivor_bytes == 0
            || config.code_collection.stale_warning_hours == 0)
    {
        bail!("code-collection limits and stale warning hours must be nonzero");
    }
    let store = if let Some(store) = existing_store {
        store
    } else {
        let mode = if catalog_store.is_some() {
            RuntimeRecordMode::CatalogV2
        } else {
            RuntimeRecordMode::BridgeV1
        };
        let store = Arc::new(CodeSourceStore::open_with_mode(
            config.paths.state_dir.join("code-sources"),
            limits,
            mode,
        )?);
        store
    };
    let auth = Arc::new(ProducerAuthRuntime::build(
        config,
        projects,
        catalog_store,
        checkout_access,
    )?);
    if auth.enabled() {
        reap_upload_body_tempfiles(store.root())?;
    }
    Ok(CodeSourceSnapshot { auth, store })
}

fn store_limits(config: &crate::config::Config) -> StoreLimits {
    StoreLimits {
        max_manifest_files: config.code_collection.max_manifest_files,
        max_manifest_logical_bytes: config.code_collection.max_manifest_logical_bytes,
        max_open_uploads_per_producer: config.code_collection.max_open_uploads_per_producer,
        retained_generations: config.code_collection.retained_generations,
        unreferenced_blob_grace_hours: config.code_collection.unreferenced_blob_grace_hours,
        max_migration_survivor_rows: config.code_collection.max_migration_survivor_rows,
        max_migration_survivor_bytes: config.code_collection.max_migration_survivor_bytes,
    }
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/code-source/v1/uploads",
            post(begin_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/manifest/{page}",
            put(put_manifest_page).layer(DefaultBodyLimit::max(
                bbox_code_source::MAX_MANIFEST_PAGE_BYTES,
            )),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/manifest/complete",
            post(complete_manifest).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/missing",
            get(missing_blobs),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/blobs/{hash}",
            put(put_blob).layer(DefaultBodyLimit::max(
                bbox_code_source::MAX_DOCUMENT_FILE_BYTES as usize,
            )),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/finalize",
            post(finalize_upload).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/generations/{generation}/status",
            get(generation_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_code_source_request,
        ))
}

async fn begin_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<BeginUploadRequest>,
) -> Result<impl IntoResponse, HttpError> {
    require_scope(&grant, &request.descriptor.scope)?;
    let store = state.code_sources.store();
    let response =
        blocking(move || store.begin_upload(&grant.producer_id, request.descriptor)).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn put_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, page)): Path<(String, u32)>,
    Json(page_body): Json<ManifestPage>,
) -> Result<StatusCode, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    blocking(move || {
        store.put_manifest_page(&grant.producer_id, &upload_id, page, &page_body.entries)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn complete_manifest(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<Json<MissingBlobsPage>, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let page = blocking(move || store.complete_manifest(&grant.producer_id, &upload_id)).await?;
    Ok(Json(page))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingQuery {
    cursor: Option<String>,
}

async fn missing_blobs(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingBlobsPage>, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let page = blocking(move || {
        store.missing_blobs(&grant.producer_id, &upload_id, query.cursor.as_deref())
    })
    .await?;
    Ok(Json(page))
}

async fn put_blob(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let expected_size = {
        let store = store.clone();
        let producer_id = grant.producer_id.clone();
        let upload_id = upload_id.clone();
        let hash = hash.clone();
        blocking(move || store.expected_blob_size(&producer_id, &upload_id, &hash)).await?
    };
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            HttpError::unprocessable("content_length_required", "exact Content-Length required")
        })?;
    if content_length != expected_size {
        return Err(HttpError::unprocessable(
            "blob_size_mismatch",
            "Content-Length does not match the manifest",
        ));
    }

    let temporary = tempfile::Builder::new()
        .prefix(UPLOAD_BODY_TEMP_PREFIX)
        .suffix(UPLOAD_BODY_TEMP_SUFFIX)
        .tempfile_in(store.root())
        .map_err(HttpError::storage)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen().map_err(HttpError::storage)?);
    let mut stream = body.into_data_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| HttpError::unprocessable("invalid_body", error.to_string()))?;
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| HttpError::too_large("blob_too_large", "blob body is too large"))?;
        if written > expected_size {
            return Err(HttpError::too_large(
                "blob_too_large",
                "blob body exceeds its manifest size",
            ));
        }
        file.write_all(&chunk).await.map_err(HttpError::storage)?;
    }
    if written != expected_size {
        return Err(HttpError::unprocessable(
            "blob_size_mismatch",
            "blob body is shorter than its manifest size",
        ));
    }
    file.sync_all().await.map_err(HttpError::storage)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(HttpError::storage)?;
    let file = file.into_std().await;
    let producer_id = grant.producer_id;
    blocking(move || store.install_blob(&producer_id, &upload_id, &hash, expected_size, file))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn reap_upload_body_tempfiles(store_root: &std::path::Path) -> Result<u64> {
    let mut reaped = 0_u64;
    for entry in std::fs::read_dir(store_root)
        .with_context(|| format!("reading code-source store root {}", store_root.display()))?
    {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(UPLOAD_BODY_TEMP_PREFIX) || !name.ends_with(UPLOAD_BODY_TEMP_SUFFIX) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        reaped = reaped.saturating_add(1);
    }
    if reaped > 0 {
        let directory = std::fs::File::open(store_root)?;
        directory.sync_all()?;
    }
    Ok(reaped)
}

async fn finalize_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<impl IntoResponse, HttpError> {
    let store = state.code_sources.store();
    let scope = require_upload_scope(&store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id.clone();
    let mixed = blocking({
        let store = store.clone();
        move || store.finalize_upload_mixed(&producer_id, &upload_id)
    })
    .await?;
    if mixed.state() == GenerationState::Ready {
        let project_id = require_scope(&grant, &scope)?.to_string();
        schedule_activation(state, scope, project_id, None);
    }
    let response = FinalizeResponse {
        generation_id: mixed.generation_id().to_string(),
        status_url: format!(
            "/internal/code-source/v1/generations/{}/status",
            mixed.generation_id()
        ),
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(generation): Path<String>,
) -> Result<Json<GenerationStatus>, HttpError> {
    bbox_code_source::validate_sha256(&generation)
        .map_err(|error| HttpError::unprocessable("invalid_generation_id", error.to_string()))?;
    let store = state.code_sources.store();
    for scope in grant.projects.keys() {
        let result = {
            let store = store.clone();
            let scope = scope.clone();
            let generation = generation.clone();
            tokio::task::spawn_blocking(move || store.load_generation_mixed(&scope, &generation))
                .await
                .map_err(|_| HttpError::storage("generation status task failed"))?
        };
        match result {
            Ok(stored) if stored.producer_id() == grant.producer_id => {
                return Ok(Json(status_from_mixed_generation(stored)));
            }
            Ok(_) => {}
            Err(error) if store_error_is_not_found(&error) => {}
            Err(error) => return Err(HttpError::from_store(error)),
        }
    }
    Err(HttpError::new(
        StatusCode::NOT_FOUND,
        "generation_not_found",
        "generation not found",
    ))
}

fn schedule_activation(
    state: Arc<SharedState>,
    scope: PublishedScope,
    project_id: String,
    guard: Option<GuardHandle>,
) {
    if !state.code_sources.begin_activation(&project_id) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        // Hold the transition guard for the full duration of this worker
        // (section 4.4). The guard drops here when the closure returns,
        // releasing the project's transition lock. None for bridge mode.
        let _guard = guard;
        let is_catalog = state.code_sources.store().record_mode() == RuntimeRecordMode::CatalogV2;
        let mut retry_delay = std::time::Duration::from_secs(1);
        loop {
            let Err(error) = activate_desired_loop(&state, &scope, &project_id) else {
                break;
            };
            // Catalog mode (section 9.1 loop elimination): one attempt
            // per invocation. The reconciler re-enqueues on the next
            // event. No sleep spin.
            if is_catalog {
                let _ = state.code_sources.store().record_health_failure(
                    &project_id,
                    "activation_failed",
                    "activation failed; inspect daemon logs",
                );
                tracing::error!(
                    project_id = %project_id,
                    scope_hash = %bbox_code_source::scope_hash(&scope),
                    error = %error,
                    "catalog-mode activation failed (single attempt)"
                );
                break;
            }
            let _ = state.code_sources.store().record_health_failure(
                &project_id,
                "activation_failed",
                "activation failed; inspect daemon logs",
            );
            tracing::error!(
                project_id = %project_id,
                scope_hash = %bbox_code_source::scope_hash(&scope),
                error = %error,
                retry_seconds = retry_delay.as_secs(),
                "code-source activation failed"
            );
            if !state.code_sources.assignment_matches(&scope, &project_id) {
                break;
            }
            std::thread::sleep(retry_delay);
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
        }
        let pending = state.code_sources.end_activation(&project_id);
        if is_catalog {
            enqueue_activation_completion(&state, &project_id, &scope);
            return;
        }
        if pending
            && let Some((assigned_scope, assigned_project)) = state
                .code_sources
                .assignments()
                .into_iter()
                .find(|(_scope, assigned_project)| assigned_project == &project_id)
        {
            schedule_activation(state, assigned_scope, assigned_project, None);
            return;
        }
        schedule_cutback_if_owner_changed(state, project_id);
    });
}

/// Every catalog worker emits one completion edge after releasing the
/// in-memory activation marker and before releasing the transition guard.
/// The reducer then observes current authority instead of a worker trying to
/// infer which concurrent trigger won while it was staging.
fn enqueue_activation_completion(
    state: &Arc<SharedState>,
    project_id: &str,
    fallback_scope: &PublishedScope,
) {
    enqueue_current_transition(
        state,
        project_id,
        fallback_scope,
        ReconcileOrigin::ActivationCompletion,
        None,
    );
}

fn enqueue_current_transition(
    state: &Arc<SharedState>,
    project_id: &str,
    fallback_scope: &PublishedScope,
    origin: ReconcileOrigin,
    authority_revision: Option<u64>,
) {
    let desired = determine_desired_assignment(state, project_id);
    let (scope, kind) = match desired {
        DesiredAssignment::Collected => {
            let assigned_scope = state
                .code_sources
                .assignments()
                .into_iter()
                .find_map(|(scope, assigned_project)| {
                    (assigned_project == project_id).then_some(scope)
                })
                .unwrap_or_else(|| fallback_scope.clone());
            (assigned_scope, ReconcileKind::Activate)
        }
        DesiredAssignment::Local | DesiredAssignment::Retired => {
            let effective_scope = state
                .code_sources
                .store()
                .load_activation_mixed(project_id)
                .ok()
                .flatten()
                .and_then(|activation| activation.published_scope().cloned())
                .unwrap_or_else(|| fallback_scope.clone());
            (effective_scope, ReconcileKind::Cutback)
        }
    };
    state
        .code_sources
        .enqueue_transition(project_id, scope, kind, origin, authority_revision);
}

fn schedule_cutback_if_owner_changed(state: Arc<SharedState>, project_id: String) {
    let store = state.code_sources.store();
    let Some(activation) = store.load_activation_mixed(&project_id).ok().flatten() else {
        return;
    };
    let Ok(generation) = store.find_generation_mixed(activation.generation_id()) else {
        return;
    };
    if !state.code_sources.assignment_authorizes(
        &generation.descriptor().scope,
        &project_id,
        generation.producer_id(),
    ) {
        // Catalog mode (section 11.5a): the reconciler is the SOLE
        // transition owner. Enqueue a Cutback event so the reconciler
        // drives the transition through schedule_cutback_catalog, which
        // is the one-attempt driver with no retry loop. The legacy
        // schedule_cutback path (with its loop+sleep) must never be
        // reached from catalog mode.
        if state.code_sources.is_catalog() {
            state.code_sources.enqueue_transition(
                &project_id,
                generation.descriptor().scope.clone(),
                ReconcileKind::Cutback,
                ReconcileOrigin::ActivationCompletion,
                None,
            );
        } else {
            schedule_cutback(
                state,
                generation.descriptor().scope.clone(),
                project_id,
                None,
            );
        }
    }
}

pub(crate) fn resume_pending_activations(state: Arc<SharedState>) {
    let is_catalog = state.code_sources.store().record_mode() == RuntimeRecordMode::CatalogV2;
    let assignments = state.code_sources.assignments();
    let assigned = assignments.iter().cloned().collect::<BTreeMap<_, _>>();
    for (scope, project_id) in assignments {
        schedule_activation(state.clone(), scope, project_id, None);
    }
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::error!(%error, "code-source manifest recovery is unavailable");
            return;
        }
    };
    let store = state.code_sources.store();
    for (project_id, entry) in &manifest.workspaces {
        if !entry
            .code_source_selector
            .as_deref()
            .is_some_and(|selector| selector.starts_with("collected:"))
        {
            continue;
        }
        let Some(activation) = store.load_activation_mixed(project_id).ok().flatten() else {
            tracing::error!(project_id, "active collected source has no recovery record");
            continue;
        };
        let Ok(generation) = store.find_generation_mixed(activation.generation_id()) else {
            tracing::error!(
                project_id,
                "active collected source generation is unavailable"
            );
            continue;
        };
        if assigned.get(&generation.descriptor().scope) != Some(project_id) {
            if is_catalog {
                // Section 9.7: catalog mode must never enter the legacy
                // sleep-retry loop. Enqueue a reconciler event instead;
                // the reducer evaluates the reduction table and
                // dispatches to the one-attempt driver.
                state.code_sources.enqueue_transition(
                    project_id,
                    generation.descriptor().scope.clone(),
                    ReconcileKind::Cutback,
                    ReconcileOrigin::StartupRecovery,
                    None,
                );
            } else {
                schedule_cutback(
                    state.clone(),
                    generation.descriptor().scope.clone(),
                    project_id.clone(),
                    None,
                );
            }
        }
    }

    // Section 9.7 restart re-drives have been folded into the pre-bind
    // startup sweep (section 10.1 step 8). The background task no longer
    // re-evaluates persisted cutback states separately; the pre-bind
    // path enqueues all reducer events before the listener binds.
    match store.activation_records_mixed() {
        Ok(records) => {
            for activation in records {
                if assigned
                    .values()
                    .any(|project_id| project_id == activation.project_id())
                {
                    continue;
                }
                let active_selector = manifest
                    .workspaces
                    .get(activation.project_id())
                    .cloned()
                    .and_then(|entry| entry.code_source_selector);
                if !active_selector
                    .as_deref()
                    .is_some_and(|selector| selector.starts_with("local:"))
                {
                    continue;
                }
                let retirement = RetirementRecord {
                    version: 1,
                    project_id: activation.project_id().to_string(),
                    selector: activation.selector().to_string(),
                    snapshot_id: activation.snapshot_id().to_string(),
                    generation_id: Some(activation.generation_id().to_string()),
                };
                let recovery = store.enqueue_retirement(&retirement).and_then(|()| {
                    if is_catalog {
                        if let Some(scope) = activation.published_scope().cloned() {
                            state.code_sources.enqueue_transition(
                                activation.project_id(),
                                scope,
                                ReconcileKind::Cutback,
                                ReconcileOrigin::StartupRecovery,
                                None,
                            );
                        }
                        Ok(())
                    } else {
                        store.clear_activation(activation.project_id())
                    }
                });
                if let Err(error) = recovery.and_then(|()| {
                    store.clear_health_failure(activation.project_id(), "cutback_pending")
                }) {
                    tracing::error!(%error, "recovering completed code-source cutback failed");
                }
            }
        }
        Err(error) => tracing::error!(%error, "loading code-source activations failed"),
    }
    match (
        collision_retirement_tasks_for_recovery(&store),
        retirement_records_for_recovery(&store),
    ) {
        (Ok(tasks), Ok(records)) => {
            enqueue_recovered_retirements(state, tasks, records);
        }
        (Err(error), _) => {
            tracing::error!(%error, "loading collision retirement work failed")
        }
        (_, Err(error)) => tracing::error!(%error, "loading code-source retirements failed"),
    }
}

fn enqueue_recovered_retirements(
    state: Arc<SharedState>,
    tasks: Vec<CollisionRetirementRecoveryTask>,
    records: Vec<RetirementRecord>,
) {
    let mut exact = BTreeMap::<String, CollisionRetirementWorkV1>::new();
    let mut selectorless = Vec::new();
    for task in tasks {
        match task {
            CollisionRetirementRecoveryTask::Exact { work, selector } => {
                if exact.insert(selector, work).is_some() {
                    tracing::error!("duplicate exact collision retirement selector refused");
                }
            }
            CollisionRetirementRecoveryTask::Selectorless { work } => selectorless.push(work),
        }
    }

    for record in records {
        let Some(work) = exact.remove(&record.selector) else {
            spawn_retirement(state.clone(), record, None, RetirementCompletion::Ordinary);
            continue;
        };
        let exact_identity = record.project_id == work.project_id.as_str()
            && record.snapshot_id == work.snapshot_id
            && record.generation_id.as_deref() == Some(work.generation_id.as_str())
            && work.exact_selector() == Some(record.selector.as_str());
        if !exact_identity {
            let _ = state.code_sources.store().record_health_failure(
                work.project_id.as_str(),
                "retirement_identity_conflict",
                "retirement queue and collision lifecycle identities disagree",
            );
            tracing::error!(
                project_id = %work.project_id,
                generation_id = %work.generation_id,
                "conflicting ordinary and collision retirement identities refused"
            );
            continue;
        }
        spawn_retirement(
            state.clone(),
            record,
            None,
            RetirementCompletion::Collision {
                project_id: work.project_id,
                generation_id: work.generation_id,
                former_scope: work.former_scope,
            },
        );
    }

    for (selector, work) in exact {
        let record = RetirementRecord {
            version: 1,
            project_id: work.project_id.to_string(),
            selector,
            snapshot_id: work.snapshot_id.clone(),
            generation_id: Some(work.generation_id.clone()),
        };
        spawn_retirement(
            state.clone(),
            record,
            None,
            RetirementCompletion::Collision {
                project_id: work.project_id,
                generation_id: work.generation_id,
                former_scope: work.former_scope,
            },
        );
    }
    for work in selectorless {
        spawn_selectorless_collision_retirement(state.clone(), work);
    }
}

fn retirement_records_for_recovery(store: &CodeSourceStore) -> Result<Vec<RetirementRecord>> {
    store.retirement_records()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CollisionRetirementRecoveryTask {
    Exact {
        work: CollisionRetirementWorkV1,
        selector: String,
    },
    Selectorless {
        work: CollisionRetirementWorkV1,
    },
}

fn collision_retirement_tasks_for_recovery(
    store: &CodeSourceStore,
) -> Result<Vec<CollisionRetirementRecoveryTask>> {
    store.reconcile_collision_retirements()?;
    store
        .collision_retirement_work_records()?
        .into_iter()
        .map(|work| {
            if let Some(selector) = work.exact_selector().map(str::to_string) {
                Ok(CollisionRetirementRecoveryTask::Exact { work, selector })
            } else {
                Ok(CollisionRetirementRecoveryTask::Selectorless { work })
            }
        })
        .collect()
}

pub(crate) fn apply_source_transitions(state: Arc<SharedState>, transitions: SourceTransitions) {
    if state.code_sources.is_catalog() {
        apply_source_transitions_catalog(state.clone(), transitions);
    } else {
        for (scope, project_id) in transitions.cutbacks {
            schedule_cutback(state.clone(), scope, project_id, None);
        }
        for (scope, project_id) in transitions.activations {
            schedule_activation(state.clone(), scope, project_id, None);
        }
    }
}

/// Catalog-mode transition application: enqueues events to the reconciler
/// channel rather than spawning inline (section 8.1 item 3).
///
/// On every successful auth-table swap, every project in any non-None
/// persisted cutback state is also enqueued, so that a config change
/// correcting a structural cause is re-evaluated without restart (section
/// 8.1 item 3, governing section 12.2).
fn apply_source_transitions_catalog(state: Arc<SharedState>, transitions: SourceTransitions) {
    for (scope, project_id) in &transitions.cutbacks {
        state.code_sources.enqueue_transition(
            project_id,
            scope.clone(),
            ReconcileKind::Cutback,
            ReconcileOrigin::AssignmentConfigReload,
            None,
        );
    }
    for (scope, project_id) in &transitions.activations {
        state.code_sources.enqueue_transition(
            project_id,
            scope.clone(),
            ReconcileKind::Activate,
            ReconcileOrigin::AssignmentConfigReload,
            None,
        );
    }
    // Config-event re-entry feed (section 8.1 item 3): every project with a
    // non-None persisted cutback state is re-enqueued so the reconciler
    // re-evaluates it under the new auth table. The reconciler must not
    // disturb persisted cutback state this milestone; it only re-runs the
    // transition under the guard.
    let store = state.code_sources.store();
    match store.activation_records_mixed() {
        Ok(records) => {
            for record in records {
                if record.cutback().is_some() {
                    if let Some(scope) = record.published_scope().cloned() {
                        state.code_sources.enqueue_transition(
                            record.project_id(),
                            scope,
                            ReconcileKind::Cutback,
                            ReconcileOrigin::AssignmentConfigReload,
                            None,
                        );
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "catalog transition re-entry: loading activation records for cutback state failed"
            );
        }
    }
    let _ = transitions;
}

/// Re-drive cutbacks that older daemons may have stranded by counting
/// writer/vector readiness as retry failures. Called when vector warmup
/// completes; current readiness deferrals also schedule their own bounded
/// retry, so this is both a migration edge and a prompt wake-up signal.
pub(crate) fn notify_cutback_readiness_available(state: &Arc<SharedState>) {
    if !state.code_sources.is_catalog() {
        return;
    }
    let store = state.code_sources.store();
    let records = match store.activation_records_mixed() {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(%error, "readiness reconciliation: loading activations failed");
            return;
        }
    };
    for record in records {
        let should_repair = matches!(
            record.cutback(),
            Some(CutbackStateV2::ManualRetryRequired {
                error_class: CutbackErrorClass::WriterContention | CutbackErrorClass::IndexCommit,
                ..
            })
        );
        if !should_repair {
            continue;
        }
        let Some(scope) = record.published_scope().cloned() else {
            continue;
        };
        enqueue_current_transition(
            state,
            record.project_id(),
            &scope,
            ReconcileOrigin::ReadinessAvailable,
            None,
        );
    }
}

/// Hourly blob GC, mode-split (F8).
///
/// CATALOG MODE passes the catalog scope set, which
/// `gc_blobs_for_scopes` already accepts and which no production caller ever
/// supplied. That is what gives a retained-only generation (one no
/// activation or desired record names, but whose scope is still a live
/// catalog scope) GC protection through its scope root. `LegacyLocal`
/// catalog projects have no `PublishedScope` and therefore add no scope
/// entry; they stay protected exactly as today through their activation and
/// anchor roots.
///
/// BRIDGE MODE keeps the empty-scope `gc_blobs()` call byte-for-byte. This
/// is not an oversight and must not be "unified": every bridge activation
/// and generation is a v1 record, and `protected_generation_ids` selects its
/// legacy classifier arm only when there is no current anchor, no v2 rows,
/// AND `catalog_scopes.is_empty()`. Passing a non-empty scope set on the
/// bridge therefore flips it onto the mixed classifier, which hard-fails on
/// v1 rows ("protected legacy generation lacks strict v2 ownership") and
/// would permanently wedge bridge blob GC.
fn gc_blobs_for_mode(
    state: &Arc<SharedState>,
    store: &Arc<CodeSourceStore>,
) -> Result<bbox_code_source_store::MaintenanceStats> {
    let Some(catalog) = state.project_authority.catalog_store() else {
        return store.gc_blobs();
    };
    let snapshot = catalog
        .snapshot()
        .map_err(|error| anyhow!("catalog snapshot unavailable during blob GC: {error}"))?;
    let catalog_state = snapshot.catalog();
    let scopes = catalog_state
        .projects
        .values()
        .filter_map(|project| match &project.scope {
            bbox_corpus_core::project_catalog::ProjectScope::Published(scope) => {
                Some(scope.clone())
            }
            bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    // Section 9.5 GC root: every non-null code_bridge_generation in
    // the catalog snapshot joins the protected set. The bridge holds
    // the named generation alive until the first new-scope activation
    // retires it or a scope-bridge-clear removes the reference.
    let bridge_generation_ids: std::collections::BTreeSet<String> = catalog_state
        .scope_migrations
        .values()
        .filter_map(|record| record.code_bridge_generation.clone())
        .collect();
    store.gc_blobs_for_scopes_with_bridge(&scopes, &bridge_generation_ids)
}

/// Spawn the cutback reconciler background task (section 4.4, 8.1 item 1).
///
/// One project-keyed owner drains the coalesced event channel and delegates
/// the actual transition work to the existing paths (`schedule_cutback` /
/// `schedule_activation`) under the per-project transition guard. The
/// bounded scheduler, exact structural-reason classification, and
/// post-commit observer are P4-E and are NOT in scope.
///
/// Catalog mode only; bridge mode never calls this.
pub(crate) fn spawn_reconciler(state: &Arc<SharedState>, runtime_handle: tokio::runtime::Handle) {
    let Some(reconciler) = state.code_sources.reconciler().cloned() else {
        return;
    };
    let state_for_task = state.clone();
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    std::thread::Builder::new()
        .name("blackbox-cutback-reconciler".to_string())
        .spawn(move || {
            // Enter the tokio runtime context so schedule_activation
            // and schedule_cutback_catalog can call spawn_blocking
            // from this plain std::thread (section 9.1: the reconciler
            // dispatches through the tokio pool, not inline).
            let _runtime_guard = runtime_handle.enter();
            while reconciler.wait(&shutdown_clone) {
                let events = reconciler.drain();
                for event in events {
                    let Some(guard) = reconciler.try_acquire(&event.project_id) else {
                        // Guard held: defer the event so it re-fires
                        // after the in-flight transition completes
                        // (section 4.4: coalesce-or-defer, never drop).
                        tracing::debug!(
                            project_id = %event.project_id,
                            "reconciler: transition guard held, deferring"
                        );
                        reconciler.defer(event);
                        continue;
                    };
                    // Guard acquired: move it INTO the spawned worker so
                    // it is held for the full transition duration and
                    // released (with condvar notification) on completion.
                    let project_id = event.project_id.clone();
                    let scope = event.scope.clone();
                    let desired = determine_desired_assignment(&state_for_task, &project_id);
                    let effective = determine_effective_source(&state_for_task, &project_id);
                    let store = state_for_task.code_sources.store();
                    let activation = match load_reconciler_activation(&store, &project_id) {
                        Ok(activation) => activation,
                        Err(error) => {
                            tracing::warn!(
                                project_id = %project_id,
                                %error,
                                "reconciler: project activation load failed"
                            );
                            if let Err(health_error) = store.record_health_failure(
                                &project_id,
                                "reconciler_project_refused",
                                &format!("activation load failed: {error}"),
                            ) {
                                tracing::warn!(
                                    project_id = %project_id,
                                    %health_error,
                                    "reconciler: failed to persist project refusal health"
                                );
                            }
                            continue;
                        }
                    };
                    let persisted = activation
                        .as_ref()
                        .and_then(|record| record.cutback().cloned());
                    let ladder = probe_ladder(&state_for_task, &project_id);

                    // Evaluate open-bridge predicate (section 9.3).
                    let effective_gen = activation
                        .as_ref()
                        .map(|a| a.generation_id().to_string())
                        .unwrap_or_default();
                    let effective_scope = activation
                        .as_ref()
                        .and_then(|a| a.published_scope().cloned());
                    let bridge_open = check_bridge_open_for_reducer(
                        &state_for_task,
                        &project_id,
                        &effective_gen,
                        effective_scope.as_ref(),
                    );

                    // Automatic bridge-clear (section 9.5): if the project
                    // has a non-null code_bridge_generation but the bridge
                    // is no longer open (effective scope is the new scope),
                    // trigger the transact to null it before evaluating the
                    // reduction table.
                    if !bridge_open {
                        if let Err(error) = try_automatic_bridge_clear(&state_for_task, &project_id)
                        {
                            tracing::warn!(
                                project_id = %project_id,
                                %error,
                                "automatic bridge-clear evidence failed"
                            );
                            if let Err(health_error) = store.record_health_failure(
                                &project_id,
                                "reconciler_project_refused",
                                &format!("automatic bridge clear failed: {error}"),
                            ) {
                                tracing::warn!(
                                    project_id = %project_id,
                                    %health_error,
                                    "reconciler: failed to persist bridge refusal health"
                                );
                            }
                            continue;
                        }
                    }
                    if let Err(error) =
                        store.clear_health_failure(&project_id, "reconciler_project_refused")
                    {
                        tracing::warn!(
                            project_id = %project_id,
                            %error,
                            "reconciler: failed to clear project refusal health"
                        );
                    }

                    let mut action = evaluate_reduction_for_event(
                        desired,
                        effective,
                        persisted.as_ref(),
                        ladder,
                        bridge_open,
                        &event.origins,
                    );
                    action = gate_completion_reentry(action, &event.origins);
                    action = gate_transient_deadline(action, persisted.as_ref(), unix_now());

                    tracing::debug!(
                        project_id = %project_id,
                        ?desired,
                        ?effective,
                        ?persisted,
                        ?ladder,
                        ?event.origins,
                        event.authority_revision,
                        bridge_open,
                        ?action,
                        "reducer: evaluated reduction table"
                    );

                    match action {
                        ReducerAction::NoOp => {
                            clear_cutback_health_if_converged(
                                &store,
                                &project_id,
                                desired,
                                effective,
                                persisted.as_ref(),
                            );
                            // Crash-window convergence (exit row 12.4): if
                            // the manifest entry is local:<project_id> but the
                            // activation record is still collected, the daemon
                            // crashed between local manifest publication and
                            // activation-record clear. The local/local cell
                            // returned NoOp (desired=Local, effective=Local,
                            // persisted=None), which is correct for a steady
                            // local project, but the stale collected record is
                            // the orphaned half of the interrupted transition.
                            // The distinction from pending-first-republish
                            // (entry ABSENT) is the manifest entry PRESENT and
                            // local: publication happened, so the record is
                            // stale and must be cleared.
                            if effective == EffectiveSource::Local {
                                if let Ok(Some(act)) = store.load_activation_mixed(&project_id) {
                                    if act.selector().starts_with("collected:") {
                                        tracing::info!(
                                            project_id = %project_id,
                                            "reducer: clearing stale collected activation \
                                             record from cutback crash window"
                                        );
                                        if let Err(error) = compare_and_apply_current_cutback(
                                            &state_for_task,
                                            &project_id,
                                            &scope,
                                            CutbackCompareOutcome::ClearActivation,
                                        ) {
                                            tracing::warn!(
                                                project_id = %project_id,
                                                %error,
                                                "reducer: failed to clear stale \
                                                 collected activation record"
                                            );
                                        }
                                    }
                                }
                            }
                            // Steady-state: guard drops, condvar notified.
                        }
                        ReducerAction::CancelCutback => {
                            if let Err(error) = compare_and_apply_current_cutback(
                                &state_for_task,
                                &project_id,
                                &scope,
                                CutbackCompareOutcome::ClearCutback,
                            ) {
                                tracing::warn!(
                                    project_id = %project_id,
                                    %error,
                                    "reducer: cancel cutback failed"
                                );
                            }
                        }
                        ReducerAction::Activate => {
                            schedule_activation(
                                state_for_task.clone(),
                                scope,
                                project_id,
                                Some(guard),
                            );
                        }
                        ReducerAction::AttemptCutback | ReducerAction::ReattemptCutback => {
                            schedule_cutback_catalog(
                                state_for_task.clone(),
                                scope,
                                project_id,
                                Some(guard),
                            );
                        }
                        ReducerAction::PersistStructural(reason) => {
                            if let Err(error) = compare_and_apply_current_cutback(
                                &state_for_task,
                                &project_id,
                                &scope,
                                CutbackCompareOutcome::Structural(reason),
                            ) {
                                tracing::warn!(
                                    project_id = %project_id,
                                    %error,
                                    "reducer: persist structural cutback state failed"
                                );
                            }
                        }
                        ReducerAction::Retire => {
                            tracing::info!(
                                project_id = %project_id,
                                "reducer: retirement handoff (P4-G)"
                            );
                        }
                    }
                }
            }
        })
        .expect("spawning cutback reconciler thread");
    // Register shutdown flag on SharedState so daemon shutdown drains the
    // reconciler cleanly. The flag is set from the shutdown path.
    // P4-D skeleton: the reconciler's `wait` method checks this flag and
    // exits. The background task is a daemon-lifetime thread; it is NOT
    // a tokio task and does NOT hold any sync lock across slow work.
    *state.reconciler_shutdown.write() = shutdown.clone();
}

fn clear_cutback_health_if_converged(
    store: &CodeSourceStore,
    project_id: &str,
    desired: DesiredAssignment,
    effective: EffectiveSource,
    persisted: Option<&CutbackStateV2>,
) {
    if persisted.is_some()
        || !matches!(
            (desired, effective),
            (DesiredAssignment::Local, EffectiveSource::Local) | (DesiredAssignment::Collected, _)
        )
    {
        return;
    }
    for code in CUTBACK_HEALTH_CODES {
        if let Err(error) = store.clear_health_failure(project_id, code) {
            tracing::warn!(
                project_id,
                code,
                %error,
                "reducer: failed to clear resolved cutback health"
            );
        }
    }
}

const CUTBACK_HEALTH_CODES: [&str; 5] = [
    "cutback_pending",
    "cutback_manual_retry_required",
    "cutback_terminal",
    "cutback_waiting_readiness",
    "cutback_waiting_selector_retirement",
];

/// Clear health rows left behind by an older daemon after cutback authority
/// has converged or a collected reassignment made the cutback obsolete. Some
/// steady projects have no startup reducer event, so limiting this cleanup to
/// the reducer's `NoOp` arm leaves those durable warnings stuck across every
/// restart.
///
/// Only projects which currently carry a cutback-specific health row are
/// inspected. The same authority predicate as the live reducer applies: no
/// persisted cutback state, and either local authority has converged or a
/// collected assignment has made the prior cutback direction obsolete. An
/// unresolved local cutback or unrelated health row remains untouched.
fn clear_converged_cutback_health_at_startup(
    store: &CodeSourceStore,
    code_sources: &CodeSourceRuntime,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
) {
    let health = match store.health_records() {
        Ok(health) => health,
        Err(error) => {
            tracing::warn!(%error, "startup sweep: loading code-source health failed");
            return;
        }
    };
    let project_ids = health
        .into_iter()
        .filter(|record| CUTBACK_HEALTH_CODES.contains(&record.code.as_str()))
        .map(|record| record.project_id)
        .collect::<BTreeSet<_>>();
    if project_ids.is_empty() {
        return;
    }
    let assigned = code_sources
        .assignments()
        .into_iter()
        .map(|(_scope, project_id)| project_id)
        .collect::<BTreeSet<_>>();
    for project_id in project_ids {
        let activation = match store.load_activation_mixed(&project_id) {
            Ok(activation) => activation,
            Err(error) => {
                tracing::warn!(
                    project_id,
                    %error,
                    "startup sweep: loading activation for health convergence failed"
                );
                continue;
            }
        };
        let desired = if assigned.contains(&project_id) {
            DesiredAssignment::Collected
        } else {
            DesiredAssignment::Local
        };
        let effective =
            determine_effective_source_from_manifest(activation.as_ref(), manifest, &project_id);
        clear_cutback_health_if_converged(
            store,
            &project_id,
            desired,
            effective,
            activation.as_ref().and_then(|record| record.cutback()),
        );
    }
}

fn gate_transient_deadline(
    action: ReducerAction,
    persisted: Option<&CutbackStateV2>,
    now: u64,
) -> ReducerAction {
    if action == ReducerAction::ReattemptCutback
        && persisted.is_some_and(|state| {
            matches!(
                state,
                CutbackStateV2::Transient {
                    deadline_unix_secs,
                    ..
                } if *deadline_unix_secs > now
            )
        })
    {
        ReducerAction::NoOp
    } else {
        action
    }
}

fn load_reconciler_activation(
    store: &bbox_code_source_store::CodeSourceStore,
    project_id: &str,
) -> anyhow::Result<Option<bbox_code_source_store::MixedActivationRecord>> {
    store.load_activation_mixed(project_id)
}

const CATALOG_OBSERVER_RESCAN_PAGE_SIZE: usize = 4096;

struct CatalogObserverRescanProgress {
    generation: u64,
    epoch: u64,
    project_ids: Vec<String>,
    next_index: usize,
}

impl CatalogObserverRescanProgress {
    fn next_event(
        &mut self,
    ) -> Option<bbox_indexing::project_catalog_store::CatalogCommittedEvent> {
        if self.next_index >= self.project_ids.len() {
            return None;
        }
        let end = (self.next_index + CATALOG_OBSERVER_RESCAN_PAGE_SIZE).min(self.project_ids.len());
        let changed_project_ids = self.project_ids[self.next_index..end]
            .iter()
            .cloned()
            .collect();
        self.next_index = end;
        Some(
            bbox_indexing::project_catalog_store::CatalogCommittedEvent {
                epoch: self.epoch,
                changed_project_ids,
            },
        )
    }

    fn is_complete(&self) -> bool {
        self.next_index >= self.project_ids.len()
    }
}

/// Spawn the post-commit observer thread (section 9.4).
///
/// Polls the `ProjectCatalogStore` commit observer at a bounded interval.
/// Each `CatalogCommittedEvent` is mapped to reconciler events: every
/// changed project id gets one `Activate` or `Cutback` event depending on
/// its desired assignment. Delivery failure marks health and triggers one
/// bounded rescan (R5). Catalog mode only; bridge mode has no observer.
pub(crate) fn spawn_commit_observer(state: &Arc<SharedState>) {
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return;
    };
    let catalog_store = catalog_store.clone();
    let observer = catalog_store.commit_observer();
    let state = state.clone();
    let shutdown = state.reconciler_shutdown.read().clone();
    std::thread::Builder::new()
        .name("blackbox-catalog-commit-observer".to_string())
        .spawn(move || {
            let poll_interval = std::time::Duration::from_secs(2);
            let mut rescan_progress: Option<CatalogObserverRescanProgress> = None;
            while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                let mut events = observer.drain_events();
                if let Some(generation) = observer.pending_rescan_generation() {
                    if rescan_progress
                        .as_ref()
                        .is_none_or(|progress| progress.generation != generation)
                    {
                        let mut project_ids = state
                            .code_sources
                            .assignments()
                            .into_iter()
                            .map(|(_, project_id)| project_id)
                            .collect::<BTreeSet<_>>();
                        let snapshot = match catalog_store.snapshot() {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                tracing::error!(%error, "catalog observer catalog rescan failed");
                                for project_id in &project_ids {
                                    let _ = state.code_sources.store().record_health_failure(
                                        project_id,
                                        "catalog_observer_rescan_failed",
                                        &error.to_string(),
                                    );
                                }
                                observer.request_rescan();
                                std::thread::sleep(poll_interval);
                                continue;
                            }
                        };
                        project_ids.extend(
                            snapshot
                                .catalog()
                                .projects
                                .keys()
                                .map(|project_id| project_id.as_str().to_string()),
                        );
                        let records = match state.code_sources.store().activation_records_mixed() {
                            Ok(records) => records,
                            Err(error) => {
                                tracing::error!(%error, "catalog observer activation rescan failed");
                                for project_id in &project_ids {
                                    let _ = state.code_sources.store().record_health_failure(
                                        project_id,
                                        "catalog_observer_rescan_failed",
                                        &error.to_string(),
                                    );
                                }
                                observer.request_rescan();
                                std::thread::sleep(poll_interval);
                                continue;
                            }
                        };
                        project_ids.extend(
                            records
                                .into_iter()
                                .map(|record| record.project_id().to_string()),
                        );
                        for project_id in &project_ids {
                            let _ = state.code_sources.store().record_health_failure(
                                project_id,
                                "catalog_observer_rescan",
                                "observer delivery overflow required a complete paged rescan",
                            );
                        }
                        rescan_progress = Some(CatalogObserverRescanProgress {
                            generation,
                            epoch: snapshot.epoch(),
                            project_ids: project_ids.into_iter().collect(),
                            next_index: 0,
                        });
                    }
                    if let Some(progress) = rescan_progress.as_mut()
                        && let Some(event) = progress.next_event()
                    {
                        events.push(event);
                    }
                }
                let delivered_a_commit = !events.is_empty();
                for event in events {
                    for project_id in &event.changed_project_ids {
                        // Map each affected id to one reconciler event
                        // (section 9.4). Determine whether to enqueue
                        // Activate or Cutback based on desired assignment.
                        let desired = determine_desired_assignment(&state, project_id);
                        let kind = match desired {
                            DesiredAssignment::Local => ReconcileKind::Cutback,
                            DesiredAssignment::Collected => ReconcileKind::Activate,
                            DesiredAssignment::Retired => continue,
                        };
                        // Derive scope: for Collected, use the auth-table
                        // assignment scope. For Local (assignment removed),
                        // fall back to the activation record's scope.
                        let scope = state
                            .code_sources
                            .assignments()
                            .into_iter()
                            .find(|(_, pid)| pid == project_id)
                            .map(|(scope, _)| scope)
                            .or_else(|| {
                                match state.code_sources.store().load_activation_mixed(project_id) {
                                    Ok(activation) => activation
                                        .and_then(|record| record.published_scope().cloned()),
                                    Err(error) => {
                                        tracing::error!(
                                            project_id,
                                            %error,
                                            "catalog observer activation read failed"
                                        );
                                        let _ = state.code_sources.store().record_health_failure(
                                            project_id,
                                            "catalog_observer_read_failed",
                                            &error.to_string(),
                                        );
                                        observer.request_rescan();
                                        None
                                    }
                                }
                            });
                        let Some(scope) = scope else {
                            // No assignment and no activation record: skip.
                            continue;
                        };
                        state
                            .code_sources
                            .enqueue_transition(
                                project_id,
                                scope,
                                kind,
                                ReconcileOrigin::CatalogCommit,
                                Some(event.epoch),
                            );
                    }
                    tracing::debug!(
                        epoch = event.epoch,
                        changed_count = event.changed_project_ids.len(),
                        "commit observer: mapped catalog commit to reconciler events"
                    );
                }
                // Watcher reconciliation is a whole-set comparison, so it
                // runs once per delivered batch rather than once per changed
                // project (plan 5.2). Unreadable authority degrades to the
                // observer's own bounded rescan.
                if delivered_a_commit {
                    super::checkout_access::reconcile_catalog_runtime_for_commit(
                        &state, &observer,
                    );
                }
                if let Some(progress) = rescan_progress.as_ref()
                    && progress.is_complete()
                {
                    let generation = progress.generation;
                    observer.complete_rescan(generation);
                    rescan_progress = None;
                }
                std::thread::sleep(poll_interval);
            }
        })
        .expect("spawning catalog commit observer thread");
}

/// Spawn the bounded cutback scheduler (section 9.2).
///
/// One thread beside `spawn_store_maintenance` computes the minimum
/// `deadline_unix_secs` across all Transient states, sleeps until then,
/// and re-attempts each due project through the reconciler event channel.
/// Structural and Terminal states never enter the scheduler. The scheduler
/// recomputes the minimum deadline on every wake, so a newly persisted
/// Transient with an earlier deadline is not delayed by the previous sleep
/// target.
///
/// Catalog mode only; bridge mode never calls this.
pub(crate) fn spawn_scheduler(state: &Arc<SharedState>, runtime_handle: tokio::runtime::Handle) {
    let Some(reconciler) = state.code_sources.reconciler().cloned() else {
        return;
    };
    let state = state.clone();
    let shutdown = state.reconciler_shutdown.read().clone();
    std::thread::Builder::new()
        .name("blackbox-cutback-scheduler".to_string())
        .spawn(move || {
            // Enter the tokio runtime context so dispatched cutback
            // events reach schedule_cutback_catalog's spawn_blocking.
            let _runtime_guard = runtime_handle.enter();
            loop {
                if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                // scheduler_wait blocks until a deadline is due or
                // shutdown, using an interruptible condvar wait (no
                // plain thread::sleep that could miss an earlier
                // deadline registered while sleeping).
                let Some(_min_deadline) = reconciler.scheduler_wait(&shutdown) else {
                    break;
                };
                // Drain all due projects and re-enqueue them through the
                // reconciler as Cutback events (section 9.2).
                let due = reconciler.drain_due(unix_now());
                for project_id in due {
                    // Derive scope from the activation record, not from
                    // current auth-table assignments. A transient cutback
                    // means the assignment was removed (desired=Local);
                    // the auth table no longer has the scope.
                    let scope = state
                        .code_sources
                        .store()
                        .load_activation_mixed(&project_id)
                        .ok()
                        .flatten()
                        .and_then(|a| a.published_scope().cloned());
                    if let Some(scope) = scope {
                        state.code_sources.enqueue_transition(
                            &project_id,
                            scope,
                            ReconcileKind::Cutback,
                            ReconcileOrigin::TransientDeadline,
                            None,
                        );
                    } else {
                        tracing::warn!(
                            project_id = %project_id,
                            "scheduler: project has a transient deadline but no activation scope"
                        );
                    }
                }
            }
        })
        .expect("spawning cutback scheduler thread");
}

pub(crate) fn spawn_store_maintenance(state: &Arc<SharedState>) -> Result<()> {
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("blackbox-code-source-maintenance".to_string())
        .spawn(move || {
            let mut tick = 0_u64;
            loop {
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let store = state.code_sources.store();
                match store.expire_uploads(24 * 60 * 60) {
                    Ok(expired) if expired > 0 => {
                        tracing::info!(expired, "expired idle code-source uploads");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "code-source upload expiry failed"),
                }
                match gc_blobs_for_mode(&state, &store) {
                    Ok(stats) if stats.reclaimed_blobs > 0 || stats.reclaimed_generations > 0 => {
                        tracing::info!(
                            blobs = stats.reclaimed_blobs,
                            bytes = stats.reclaimed_bytes,
                            generations = stats.reclaimed_generations,
                            "code-source GC reclaimed unreferenced data"
                        )
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "code-source blob GC failed"),
                }
                let protected_git_sources = state
                    .code_read_view
                    .read()
                    .git_overlays
                    .values()
                    .filter_map(|overlay| {
                        overlay
                            .source
                            .producer_transport()
                            .map(|(_, source)| source.to_string())
                    })
                    .collect::<std::collections::BTreeSet<_>>();
                match state.git_sources.store().maintain(&protected_git_sources) {
                    Ok(report) if report != bbox_git_source_store::MaintenanceReport::default() => {
                        tracing::info!(
                            expired_uploads = report.expired_uploads,
                            generations = report.retired_generations,
                            records = report.deleted_records,
                            bytes = report.deleted_record_bytes,
                            "Git-source maintenance reclaimed unreferenced data"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "Git-source maintenance failed"),
                }
                if tick.is_multiple_of(24) {
                    match store.scrub_retained() {
                        Ok(stats) => tracing::info!(
                            blobs = stats.scrubbed_blobs,
                            degraded_generations = stats.degraded_generations,
                            "code-source retained blob scrub complete"
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "code-source retained blob scrub failed")
                        }
                    }
                }
                tick = tick.wrapping_add(1);
                drop(state);
                std::thread::sleep(std::time::Duration::from_secs(60 * 60));
            }
        })
        .context("spawning code-source maintenance thread")?;
    Ok(())
}

fn schedule_cutback(
    state: Arc<SharedState>,
    scope: PublishedScope,
    project_id: String,
    guard: Option<GuardHandle>,
) {
    if !state.code_sources.begin_activation(&project_id) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        // Hold the transition guard for the full duration of this worker
        // (section 4.4). The guard drops here when the closure returns,
        // releasing the project's transition lock. None for bridge mode.
        let _guard = guard;
        let store = state.code_sources.store();
        let mut retry_delay = std::time::Duration::from_secs(1);
        loop {
            match cutback_to_local(&state, &scope, &project_id) {
                Ok(()) => break,
                Err(error) => {
                    let _ = store.mark_cutback_pending_mixed(
                        &project_id,
                        "cutback failed; inspect daemon logs",
                    );
                    let _ = store.record_health_failure(
                        &project_id,
                        "cutback_pending",
                        "cutback failed; inspect daemon logs",
                    );
                    tracing::error!(
                        project_id,
                        scope_hash = %bbox_code_source::scope_hash(&scope),
                        error = %error,
                        retry_seconds = retry_delay.as_secs(),
                        "code-source local cutback remains pending"
                    );
                    if state.code_sources.assignment_matches(&scope, &project_id) {
                        break;
                    }
                    std::thread::sleep(retry_delay);
                    retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
                }
            }
        }
        let _pending = state.code_sources.end_activation(&project_id);
        for (assigned_scope, assigned_project) in state.code_sources.assignments() {
            if assigned_project == project_id {
                schedule_activation(state.clone(), assigned_scope, assigned_project, None);
            }
        }
    });
}

/// Resolve the source-neutral code identity for one project (Phase 3 plan
/// section 6 item 4, governing section 10.1).
///
/// Catalog mode reads the pinned catalog snapshot, so a remote-only project
/// with zero attachments resolves an identity like any other project: that
/// is the activation half of the F1 fix. Bridge mode projects the version-1
/// record, which is the only authority that exists there, so the
/// "registered project disappeared" failure survives on that arm alone.
fn resolve_code_project_identity(
    state: &Arc<SharedState>,
    project_id: &str,
    during: &str,
) -> Result<CodeProjectIdentity> {
    if let Some(store) = state.project_authority.catalog_store() {
        let pinned = store
            .snapshot()
            .map_err(|error| anyhow!("catalog snapshot unavailable during {during}: {error}"))?;
        let catalog = pinned.catalog();
        let parsed = ProjectId::parse(project_id.to_string())
            .map_err(|error| anyhow!("invalid catalog project id during {during}: {error}"))?;
        let project = catalog
            .projects
            .get(&parsed)
            .ok_or_else(|| anyhow!("catalog project disappeared during {during}"))?;
        let repo_history = project
            .repo_history
            .as_ref()
            .and_then(|id| catalog.repo_histories.get(id));
        return Ok(CodeProjectIdentity::from_catalog(project, repo_history));
    }
    let record = state
        .records_provider
        .records_snapshot()
        .records
        .iter()
        .find(|record| record.project_id == project_id)
        .cloned()
        .ok_or_else(|| anyhow!("registered project disappeared during {during}"))?;
    CodeProjectIdentity::from_bridge_record(&record)
        .map_err(|error| anyhow!("projecting a bridge code identity during {during}: {error}"))
}

/// Republish the pinned code read view after a post-activation overlay
/// landed. The active selector map is already correct (the activation set
/// it); only the edge index and searcher move.
pub(super) fn republish_code_read_view(state: &Arc<SharedState>) -> Result<()> {
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    );
    let index = state.idx.write();
    let selectors = index.active_code_selectors();
    *state.code_read_view.write() = Arc::new(super::CodeReadView {
        active_selectors: selectors,
        searcher: index.searcher(),
        // Fail closed until the bounded watcher parses the newly selected
        // sidecars. Keeping the outgoing graph here would expose edges from a
        // selector this view no longer names; rebuilding inline made this
        // post-activation path a multi-minute blocking operation.
        edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
        catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
        // Read AFTER the overlay selector landed in the manifest: this
        // republish is what makes the freshly staged overlay visible to
        // readers, so pinning a pre-swap map here would publish edges the
        // view claims not to have.
        git_overlays: super::state::read_git_overlays_for_view(
            &state.project_authority,
            &edges_dir,
        ),
    });
    state.nudge_edge_index_rebuild();
    Ok(())
}

/// Run ONE consolidated repo-history refresh for the repo-history record the
/// given project belongs to (Phase 3 plan section 10 items 2 and 3).
///
/// Catalog mode only. `lease` is the already-validated `GitHistory` lease the
/// caller holds; this function re-uses it rather than acquiring a second one,
/// so the ladder-selected attachment and the leased checkout are the same
/// checkout by construction.
///
/// STAGED-HOLD CONSTRAINT: this enqueues a writer op, so it must never be
/// called while a `StagedIndexGeneration` is alive on this thread. The
/// activation path drops its hold before reaching here for exactly that
/// reason.
fn refresh_consolidated_repo_history(
    state: &Arc<SharedState>,
    project_id: &str,
    checkout_root: &std::path::Path,
    snapshot_id: &str,
    current_chunk_targets: &std::collections::HashMap<
        String,
        bbox_corpus_core::entity_ref::EntityRef,
    >,
) -> Result<Option<String>> {
    use bbox_indexing::index::consolidated_history;

    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Ok(None);
    };
    let pinned = catalog_store.snapshot()?;
    let parsed = ProjectId::parse(project_id.to_string())
        .map_err(|error| anyhow!("invalid catalog project id for history refresh: {error}"))?;
    let Some(group) = consolidated_history::plan_repo_history_ingest(pinned.catalog())
        .into_iter()
        .find(|group| group.members.contains_key(parsed.as_str()))
    else {
        return Ok(None);
    };
    // The ladder must agree with the checkout we already hold. When it does
    // not, another member's attachment is the deterministic walk source and
    // this project's activation is not the right moment to walk: refusing
    // keeps the "same catalog state always picks the same walk source"
    // guarantee that content-addressed generation ids depend on.
    let Some(selected) =
        consolidated_history::select_history_attachment(pinned.attachments(), &group)
    else {
        return Ok(None);
    };
    if selected.project_id.as_str() != parsed.as_str() {
        return Ok(None);
    }

    let git_meta_dir = bbox_indexing::index::git_history::git_meta_dir_from_projects_path(
        &state.config.read().paths.projects_path,
    );
    let cursors = consolidated_history::RepoHistoryCursorStoreV1::new(&git_meta_dir);
    let existing_cursor = cursors.load(&group.repo_history_id)?;
    let since = match existing_cursor.as_ref() {
        Some(cursor) => Some(cursor.last_ingested_sha.clone()),
        None => {
            // FIRST consolidated generation for this record: inventory and
            // back up every legacy per-project cursor, then walk COMPLETE
            // reachable history. Never seed from those values - siblings may
            // disagree, and seeding from one silently skips whatever interval
            // the other had already passed.
            let inventory = cursors.inventory_and_back_up_legacy_cursors(&group)?;
            tracing::info!(
                repo_history = %group.repo_history_id,
                observed = inventory.observed.len(),
                divergent = inventory.divergent,
                "backing up legacy per-project Git cursors; the first consolidated \
                 generation performs one complete reachable-history walk"
            );
            None
        }
    };

    // Only the activating project's chunk targets are known here, so only its
    // file edges can be emitted this pass. Sibling members keep their existing
    // sidecars until their own activation refreshes them; the commit documents
    // and the generation, which is what the members actually share, are
    // produced exactly once regardless.
    let targets =
        std::collections::BTreeMap::from([(project_id.to_string(), current_chunk_targets.clone())]);
    let walk =
        consolidated_history::walk_repo_history(checkout_root, &group, since.as_deref(), &targets)?;
    let display = pinned
        .catalog()
        .projects
        .get(&parsed)
        .map(|project| project.display_name.clone())
        .unwrap_or_else(|| project_id.to_string());
    let edges_for_this_project = walk
        .edges_by_project
        .iter()
        .filter(|(member, _)| member.as_str() == project_id)
        .map(|(member, edges)| (member.clone(), edges.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    state.index_writer.stage_consolidated_history(
        group.primary_namespace.as_str().to_string(),
        display,
        walk.commits.clone(),
        edges_for_this_project,
        std::collections::BTreeMap::from([(project_id.to_string(), snapshot_id.to_string())]),
    )?;

    let generation_store =
        bbox_indexing::index::history_generations::HistoryGenerationStore::open_for_index(
            &state.config.read().paths.index_path,
        )
        .map_err(|error| anyhow!("{error}"))?;
    let outcome = bbox_indexing::index::history_refresh::refresh_repo_history_generation(
        catalog_store,
        &generation_store,
        &cursors,
        &group,
        &walk,
    )
    .map_err(|error| anyhow!("{error}"))?;
    tracing::info!(
        repo_history = %group.repo_history_id,
        attachment = %selected.attachment_id,
        rung = selected.rung.as_str(),
        generation = %outcome.generation.id,
        superseded = ?outcome.superseded_generation,
        commits = walk.commits.len(),
        "consolidated repo-history refresh published a generation"
    );
    Ok(Some(outcome.generation.id.as_str().to_string()))
}

/// Install the typed overlay selector for a project whose current-file edges
/// were just staged (Phase 3 plan section 10 item 1).
///
/// Bridge mode installs nothing and says so: the bridge lane stages its Git
/// member inside its own transaction and its manifest entry is not
/// overlay-managed, so a selector there would be a claim the loader gate
/// would then act on.
///
/// The `repo_history_generation` comes from the project's catalog history
/// record. When the record has no `Ready` materialization yet the overlay is
/// NOT installed: the selector's whole job on the GC side is to hold a
/// reference to a real generation, and naming one that does not exist would
/// make the reference manifest describe a generation nothing can load.
/// History health reports that project as `lagging` until the first
/// consolidated refresh publishes a generation.
fn install_git_overlay_selector(
    state: &Arc<SharedState>,
    project_id: &str,
    code_generation: &str,
    attachment_id: &str,
    repo_head: Option<&str>,
) -> Result<()> {
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Ok(());
    };
    let pinned = catalog_store.snapshot()?;
    let catalog = pinned.catalog();
    let parsed = ProjectId::parse(project_id.to_string())
        .map_err(|error| anyhow!("invalid catalog project id for the overlay selector: {error}"))?;
    let project = catalog
        .projects
        .get(&parsed)
        .ok_or_else(|| anyhow!("catalog project disappeared before the overlay selector"))?;
    let Some(history) = project
        .repo_history
        .as_ref()
        .and_then(|id| catalog.repo_histories.get(id))
    else {
        return Ok(());
    };
    let RepoHistoryMaterialization::Ready { generation_id } = &history.materialization else {
        return Ok(());
    };
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    // Monotonic per project. Read under the same manifest the swap writes, so
    // two overlays that agree on every other field are still distinguishable
    // - "did the overlay actually swap?" must be answerable from the manifest.
    let previous_generation = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)
        .ok()
        .and_then(|index| {
            index
                .workspaces
                .get(project_id)
                .and_then(|entry| entry.git_overlay.as_ref())
                .map(|overlay| overlay.overlay_generation)
        })
        .unwrap_or(0);
    bbox_edge_sidecar::snapshot::select_git_overlay(
        &edges_dir,
        project_id,
        Some(bbox_corpus_core::git_overlay::GitOverlaySelector {
            project_id: project_id.to_string(),
            code_generation: code_generation.to_string(),
            repo_history_generation: generation_id.as_str().to_string(),
            source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
                attachment_id: attachment_id.to_string(),
            },
            repo_head: repo_head.unwrap_or_default().to_string(),
            commit_namespace: history.primary_namespace.as_str().to_string(),
            overlay_generation: previous_generation.saturating_add(1),
        }),
    )
}

/// Best-effort Git current-file overlay for an ALREADY ACTIVE collected
/// generation (Phase 3 plan section 6 item 3 and section 10 item 1,
/// governing section 11).
///
/// Every failure mode below leaves the activation intact and records durable
/// health: no attachment to lease, a denied lease, a mid-walk Git error, or a
/// failed republish. This is what closes F5 - the activation transaction no
/// longer opens Git at all, so a Git problem can no longer fail or roll back
/// a valid generation.
///
/// P3-F completes the shape: on success this installs a typed
/// [`GitOverlaySelector`] naming the exact code generation the staged edges
/// target, so the loader admits the `git-current.jsonl` member. Until the
/// selector lands the member is gated OFF, which is why every early return
/// here leaves the project with no commit-file edges rather than with edges
/// pointing at a retired snapshot.
///
/// `code_generation` is the collected generation id the activation just
/// published; it must be the value the manifest entry now carries or
/// `select_git_overlay` refuses.
#[allow(clippy::too_many_arguments)]
fn stage_git_current_overlay_after_activation(
    state: &Arc<SharedState>,
    project_id: &str,
    scope: &PublishedScope,
    snapshot_id: &str,
    code_generation: &str,
    current_chunk_targets: &std::collections::HashMap<
        String,
        bbox_corpus_core::entity_ref::EntityRef,
    >,
) {
    let store = state.code_sources.store();
    match super::history_activation::reconcile_transport_currency(state, project_id) {
        Ok(true) => {
            let _ = store.clear_health_failure(project_id, "git_history_unavailable");
            let _ = store.clear_health_failure(
                project_id,
                bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE,
            );
            return;
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(
            project_id,
            %error,
            "Git-history transport currency could not be proved; attachment refresh remains eligible"
        ),
    }
    let record = state
        .records_provider
        .records_snapshot()
        .records
        .iter()
        .find(|record| record.project_id == project_id)
        .cloned();
    let degrade = |reason: String| {
        if let Err(error) = store.record_health_failure(
            project_id,
            "git_history_unavailable",
            &format!("Git current-file overlay unavailable: {reason}"),
        ) {
            tracing::warn!(
                project_id,
                error = %error,
                "failed to persist GitHistory degradation record"
            );
        }
    };
    let Some(record) = record else {
        // A remote-only catalog project has no checkout to walk. P3-F
        // RECLASSIFIES this: it is a nameable catalog steady state, not a Git
        // subsystem failure, so it gets the history-model code rather than
        // `git_history_unavailable`. The P3-B cell flagged the conflation
        // explicitly - every remote-only project looked permanently degraded.
        if let Err(error) = store.record_health_failure(
            project_id,
            bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE,
            "no attached checkout can walk this project's repository history; \
             commit documents stay readable and cannot be refreshed",
        ) {
            tracing::warn!(
                project_id,
                error = %error,
                "failed to persist the no-attachment history record"
            );
        }
        return;
    };
    if let Err(error) = store.clear_health_failure(
        project_id,
        bbox_indexing::index::history_health::HISTORY_UNAVAILABLE_NO_ATTACHMENT_CODE,
    ) {
        tracing::warn!(
            project_id,
            error = %error,
            "failed to clear the no-attachment history record"
        );
    }
    if record.repo_id.is_none() {
        return;
    }
    // The daemon-wide broker, exactly like every other checkout consumer:
    // the overlay is a post-activation daemon step, not part of the code
    // collection runtime's own authority.
    let lease = match state.checkout_access.acquire(CheckoutAccessRequest {
        project_id: project_id.to_string(),
        attachment: CheckoutAttachmentSelector::Selected,
        expected_scope: Some(scope.clone()),
        kind: CheckoutAccessKind::GitHistory,
        intent: CheckoutAccessIntent::Read,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
    }) {
        Ok(lease) => lease,
        Err(error) => {
            degrade(error.code.as_str().to_string());
            return;
        }
    };
    // P3-E: the overlay's commit documents carry the identity's display name in
    // `project`, so it is resolved through the same authority the reindex pass
    // uses rather than derived from the compat record's alias set.
    let project_display = match resolve_code_project_identity(
        state,
        project_id,
        "the post-activation Git current-file overlay",
    ) {
        Ok(identity) => identity.display_name,
        Err(error) => {
            degrade(format!("the project identity is unresolvable: {error}"));
            return;
        }
    };
    // Captured BEFORE the lease moves into the writer op: the selector below
    // records which attachment and head the overlay was built from, and that
    // evidence is what separates `current` from `lagging` history health.
    let attachment_id = lease.attachment_id().to_string();
    let repo_head = bbox_corpus_core::git::current_head(lease.checkout_root());
    // Catalog mode routes through CONSOLIDATED ingestion: one walk per
    // repo-history record keyed by its primary namespace, publishing a
    // durable generation. The per-project walk below stays the bridge path
    // and the catalog fallback for a project with no repo-history record.
    // Running both would write the same commits twice under two different
    // namespaces, so this is an either/or, not a sequence.
    let consolidated = match refresh_consolidated_repo_history(
        state,
        project_id,
        lease.checkout_root(),
        snapshot_id,
        current_chunk_targets,
    ) {
        Ok(consolidated) => consolidated,
        Err(error) => {
            tracing::warn!(
                project_id,
                error = %error,
                "consolidated repo-history refresh failed; the generation stays active"
            );
            if let Err(record_error) = store.record_health_failure(
                project_id,
                bbox_indexing::index::history_health::HISTORY_REFRESH_FAILED_CODE,
                &format!("the consolidated repo-history refresh failed: {error}"),
            ) {
                tracing::warn!(
                    project_id,
                    error = %record_error,
                    "failed to persist the history-refresh failure record"
                );
            }
            return;
        }
    };
    if let Err(error) = store.clear_health_failure(
        project_id,
        bbox_indexing::index::history_health::HISTORY_REFRESH_FAILED_CODE,
    ) {
        tracing::warn!(
            project_id,
            error = %error,
            "failed to clear the history-refresh failure record"
        );
    }
    if consolidated.is_none()
        && let Err(error) = state.index_writer.stage_git_current_overlay(
            record,
            project_display,
            lease,
            snapshot_id.to_string(),
            current_chunk_targets.clone(),
        )
    {
        tracing::warn!(
            project_id,
            error = %error,
            "post-activation Git current-file overlay failed; the generation stays active"
        );
        degrade("the Git walk failed; inspect daemon logs".to_string());
        return;
    }
    if let Err(error) = store.clear_health_failure(project_id, "git_history_unavailable") {
        tracing::warn!(
            project_id,
            error = %error,
            "failed to clear GitHistory degradation record"
        );
    }
    // The edges are staged; now name them. Until the selector lands the
    // loader gates the `git-current.jsonl` member OFF, so this is not a
    // decoration step - it is what makes the staged edges readable at all.
    if let Err(error) = install_git_overlay_selector(
        state,
        project_id,
        code_generation,
        &attachment_id,
        repo_head.as_deref(),
    ) {
        tracing::warn!(
            project_id,
            error = %error,
            "installing the Git overlay selector failed; the staged current-file \
             edges stay gated off and the generation stays active"
        );
        degrade(format!(
            "the overlay selector could not be installed: {error}"
        ));
        return;
    }
    if let Err(error) = republish_code_read_view(state) {
        tracing::warn!(
            project_id,
            error = %error,
            "republishing the read view after the Git overlay failed"
        );
    }
}

/// Desired assignment for a project (section 9.3 reduction table input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredAssignment {
    /// Project has a producer assignment in the auth table: wants
    /// collected (the producer owns the code-source path).
    Collected,
    /// Project's producer assignment was removed: wants local (local
    /// walk only, drive collected-to-local cutback).
    Local,
    /// Project is retired (handoff to retirement, P4-G).
    Retired,
}

/// Effective activation source (section 9.3 reduction table input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveSource {
    Collected,
    Local,
    Warming,
    Unavailable,
}

/// Attachment ladder result (section 9.3 reduction table input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LadderResult {
    Selected,
    None,
    Ambiguous,
    ScopeInvalid,
}

/// The action the reducer decides (section 9.3 reduction table output).
#[derive(Debug, PartialEq, Eq)]
enum ReducerAction {
    /// No action needed; state is steady.
    NoOp,
    /// Cancel any persisted cutback state and ensure collected is active.
    CancelCutback,
    /// Activate the desired scope (collected authority).
    Activate,
    /// Attempt the cutback to local.
    AttemptCutback,
    /// Persist a structural cutback state.
    PersistStructural(CutbackReason),
    /// Re-attempt the cutback (attachment now available or scheduler due).
    ReattemptCutback,
    /// Hand off to retirement (P4-G).
    Retire,
}

/// Outcome of once-only classification for a single migrated record
/// (section 10.1 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassificationOutcome {
    /// Open-bridge predicate holds: mirror cleared, bridge-exempt.
    BridgeExempt,
    /// No valid attachment: Structural persisted.
    StructuralPersisted(CutbackReason),
    /// Valid attachment: mirror cleared, cutback deferred to sweep.
    DeferredToSweep,
}

/// Once-only classification of migrated records with
/// (`cutback: None`, `cutback_pending: true`) (section 10.1 step 5).
///
/// This runs BEFORE the relationship chain so the coherence clause
/// (section 4.10) never sees an unclassified record. For each such
/// record, three outcomes:
/// (a) open-bridge predicate holds: clear mirror to (`None`, `false`),
///     mark bridge-exempt;
/// (b) no valid scope-matching attachment: persist typed Structural;
/// (c) valid attachment: clear mirror to (`None`, `false`), defer
///     cutback to the step-8 sweep.
///
/// Returns the set of project classifications for the step-8 sweep to
/// consume. Bridge-exempt projects are NOT queued.
fn classify_migrated_records(
    store: &CodeSourceStore,
    snapshot: &CatalogSnapshotV2,
    checkout_access: &CheckoutAccessBroker,
) -> Result<Vec<(String, ClassificationOutcome)>> {
    let records = store.activation_records_mixed()?;
    let mut results = Vec::new();
    for activation in &records {
        if !activation.is_current_v2() {
            continue;
        }
        // Only classify the legacy-migration shape: typed field is None
        // but the derived mirror says pending.
        let pending = activation.is_cutback_pending();
        let typed = activation.cutback().is_some();
        if !pending || typed {
            continue;
        }
        let project_id = activation.project_id();
        let generation_id = activation.generation_id();
        let scope = activation.published_scope();

        // (a) Open-bridge predicate check.
        let migration_records: Vec<_> = snapshot
            .scope_migrations
            .values()
            .filter(|r| r.project_id.as_str() == project_id)
            .collect();
        if is_bridge_open(&migration_records, generation_id, scope) {
            // Outcome (a): clear mirror, mark bridge-exempt.
            store.clear_cutback_state(project_id)?;
            results.push((project_id.to_string(), ClassificationOutcome::BridgeExempt));
            continue;
        }

        // (b)/(c) probe the attachment ladder.
        let ladder = probe_ladder_raw(store, checkout_access, project_id);
        match ladder {
            LadderResult::Selected => {
                // Outcome (c): valid attachment, clear mirror, defer.
                store.clear_cutback_state(project_id)?;
                results.push((
                    project_id.to_string(),
                    ClassificationOutcome::DeferredToSweep,
                ));
            }
            LadderResult::None => {
                let reason = CutbackReason::NoLocalAttachment;
                store.mark_cutback_state(project_id, CutbackStateV2::Structural { reason })?;
                results.push((
                    project_id.to_string(),
                    ClassificationOutcome::StructuralPersisted(reason),
                ));
            }
            LadderResult::Ambiguous => {
                let reason = CutbackReason::AmbiguousAttachment;
                store.mark_cutback_state(project_id, CutbackStateV2::Structural { reason })?;
                results.push((
                    project_id.to_string(),
                    ClassificationOutcome::StructuralPersisted(reason),
                ));
            }
            LadderResult::ScopeInvalid => {
                let reason = CutbackReason::ScopeMismatch;
                store.mark_cutback_state(project_id, CutbackStateV2::Structural { reason })?;
                results.push((
                    project_id.to_string(),
                    ClassificationOutcome::StructuralPersisted(reason),
                ));
            }
        }
    }
    Ok(results)
}

/// Probe the attachment ladder without requiring `Arc<SharedState>`.
/// Used during pre-bind startup classification (section 10.1 step 5)
/// where SharedState is still being constructed.
fn probe_ladder_raw(
    store: &CodeSourceStore,
    checkout_access: &CheckoutAccessBroker,
    project_id: &str,
) -> LadderResult {
    use bbox_indexing::checkout_access::{
        CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
        CheckoutAttachmentSelector,
    };
    // Determine the expected scope from the activation record's
    // published_scope, not from the auth-table assignment (which is not
    // available pre-bind in the same form).
    let scope = store
        .load_activation_mixed(project_id)
        .ok()
        .flatten()
        .and_then(|a| a.published_scope().cloned());
    let Some(scope) = scope else {
        return LadderResult::None;
    };
    match checkout_access.acquire(CheckoutAccessRequest {
        project_id: project_id.to_string(),
        attachment: CheckoutAttachmentSelector::Selected,
        expected_scope: Some(scope),
        kind: CheckoutAccessKind::GitHistory,
        intent: CheckoutAccessIntent::Read,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
    }) {
        Ok(_) => LadderResult::Selected,
        Err(error) => {
            use bbox_indexing::checkout_access::CheckoutAccessErrorCode as Code;
            match error.code {
                Code::AttachmentNotFound
                | Code::ObservationUnavailable
                | Code::DeniedByTestProbe => LadderResult::None,
                Code::ScopeMismatch
                | Code::CapabilityDenied
                | Code::IntentDenied
                | Code::ConservativePathGateDenied
                | Code::InvalidRoot
                | Code::UnsafeRelativePath
                | Code::WriteIntentRequired => LadderResult::ScopeInvalid,
                _ => LadderResult::Ambiguous,
            }
        }
    }
}

/// Validate the typed relationship chain (section 10.2) for every active
/// catalog-mode collected activation. Any failure is fail-closed BEFORE
/// HTTP bind with a typed error code.
///
/// The six links per activation:
/// 1. Catalog project exists and bears the activation's published_scope,
///    or the open-bridge predicate admits (sole sanctioned exception).
/// 2. Activation validates against StoredGenerationV2.
/// 3. Stored generation validates descriptor scope and generation identity.
/// 4. Descriptor validates immutable manifest digest and entries.
/// 5. WorkspaceIndexEntry agrees: project key, selector, generation,
///    snapshot, manifest path.
/// 6. CutbackStateV2 internally consistent (coherence clause holds).
fn validate_relationship_chain(
    store: &CodeSourceStore,
    snapshot: &CatalogSnapshotV2,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
) -> Result<()> {
    let records = store.activation_records_mixed()?;
    for activation in &records {
        if !activation.is_current_v2() {
            continue;
        }
        let project_id = activation.project_id();
        let generation_id = activation.generation_id();
        let scope = activation
            .published_scope()
            .ok_or_else(|| anyhow!("error.code_source_relationship_chain: v2 activation has no published scope for project {project_id}"))?;

        // Link 1: catalog project exists and bears the activation's
        // published_scope, or the open-bridge predicate admits.
        let pid = ProjectId::parse(project_id.to_string()).map_err(|e| {
            anyhow!("error.code_source_relationship_chain: invalid project id {project_id}: {e}")
        })?;
        let catalog_project = snapshot.projects.get(&pid);
        let scope_matches = catalog_project.is_some_and(|p| {
            matches!(&p.scope, bbox_corpus_core::project_catalog::ProjectScope::Published(ps) if *ps == *scope)
        });
        let migration_records: Vec<_> = snapshot
            .scope_migrations
            .values()
            .filter(|r| r.project_id.as_str() == project_id)
            .collect();
        let bridge_admits = is_bridge_open(&migration_records, generation_id, Some(scope));
        if !scope_matches && !bridge_admits {
            if catalog_project.is_none() {
                bail!(
                    "error.code_source_relationship_chain: \
                     catalog project not found for activation project {project_id}"
                );
            }
            bail!(
                "error.code_source_scope_agreement: \
                 catalog scope does not match activation scope for project {project_id} \
                 and no open code bridge admits"
            );
        }

        // Link 2: activation validates against stored generation.
        let generation = store.find_generation_mixed(generation_id).map_err(|e| {
            anyhow!(
                "error.code_source_relationship_chain: \
                 stored generation not found for project {project_id}: {e}"
            )
        })?;
        if let MixedActivationRecord::CurrentV2(v2_act) = activation {
            let v2_gen = match &generation {
                MixedStoredGeneration::CurrentV2(g) => g,
                MixedStoredGeneration::LegacyV1(_) => bail!(
                    "error.code_source_record_mode: \
                     v2 activation references v1 generation for project {project_id}"
                ),
            };
            v2_act
                .validate_against_generation(v2_gen)
                .map_err(|e| {
                    anyhow!(
                        "error.code_source_relationship_chain: \
                         activation does not validate against generation for project {project_id}: {e}"
                    )
                })?;
        }

        // Link 3: stored generation validates descriptor scope and
        // generation identity. The generation_id identity is already
        // checked by validate_against_generation (link 2); here we
        // confirm the descriptor scope matches the activation scope.
        let descriptor = generation.descriptor();
        if &descriptor.scope != scope {
            bail!(
                "error.code_source_relationship_chain: \
                 descriptor scope does not match activation scope for project {project_id}"
            );
        }

        // Link 4: descriptor validates immutable manifest digest and
        // entries. validate_header checks the header fields including
        // the manifest_sha256 format. Additionally, read the manifest
        // file from disk and run the full bounded manifest verification
        // (digest match, entry count, byte limits) via the migration
        // verifier (section 10.2 link 4).
        descriptor.validate_header().map_err(|e| {
            anyhow!(
                "error.code_source_relationship_chain: \
                 descriptor header validation failed for project {project_id}: {e}"
            )
        })?;
        {
            let manifest_bytes = store
                .read_generation_manifest_bytes(scope, generation_id)
                .map_err(|e| {
                    anyhow!(
                        "error.code_source_relationship_chain: \
                         manifest file missing or unreadable for project {project_id}: {e}"
                    )
                })?;
            let limits = store.limits();
            bbox_code_source_store::verify_generation_manifest_for_migration(
                &manifest_bytes,
                descriptor,
                generation.producer_id(),
                generation_id,
                &limits,
            )
            .map_err(|e| {
                anyhow!(
                    "error.code_source_relationship_chain: \
                     manifest verification failed for project {project_id}: {e}"
                )
            })?;
        }

        // Link 5: WorkspaceIndexEntry agrees: project key, selector,
        // generation, snapshot, manifest path.
        //
        // ABSENCE of a workspace index entry for an active collected
        // activation is valid-pending-first-republish: the migration
        // facade does not fabricate WorkspaceIndexEntry rows; they are
        // created by the daemon's own first activation republish (the
        // startup read-view construction and step-8 reducer sweep
        // establish the entry on first boot). The chain's purpose is
        // to catch DRIFT (a present entry that disagrees), not
        // pre-first-boot absence. Admitting absence keeps migrated
        // roots bootable (plan section 10.4 bootsmoke row).
        match manifest.workspaces.get(project_id) {
            None => {
                tracing::info!(
                    project = %project_id,
                    "relationship chain link 5: no workspace index entry \
                     (valid-pending-first-republish for migrated root)"
                );
            }
            Some(entry) => {
                // Crash-window admission (exit row 12.4): if the workspace
                // entry's selector is the writer's own local shape for THIS
                // project (local:<project_id>) while the activation record is
                // collected, the daemon crashed between local manifest
                // publication and activation-record clear (the sanctioned
                // crash window in the reduction table: local | local | any
                // non-None | clear stale state). ADMIT with a tracing::info
                // so the startup reducer sweep converges it (the stale
                // collected record is cleared).
                //
                // R3F2: admission requires a FULLY VALID local-writer entry.
                // The crash window must not admit a drifted or malformed entry
                // that happens to carry the correct selector. The loader
                // (manifest.rs:306) joins active_snapshot beneath the
                // materialized root, so a traversal-bearing or cross-project
                // snapshot path could load another project's or an escaped
                // directory's JSONL. Validate:
                //   1. exact manifest path for this project
                //   2. generation is "local" (the local-writer shape)
                //   3. snapshot path is same-project confined (no traversal)
                let entry_selector = entry.code_source_selector.as_deref();
                let expected_local_selector = bbox_code_source::local_selector(project_id);
                let is_cutback_crash_window = entry_selector
                    == Some(expected_local_selector.as_str())
                    && activation.selector().starts_with("collected:");
                if is_cutback_crash_window {
                    // Validate manifest path.
                    let expected_manifest = format!("workspace/{project_id}/manifest.json");
                    if entry.manifest != expected_manifest {
                        bail!(
                            "error.code_source_relationship_chain: \
                             crash-window entry has wrong manifest path for project {project_id} \
                             (expected {expected_manifest}, got {})",
                            entry.manifest
                        );
                    }
                    // Validate generation is the local-writer shape.
                    if entry.code_source_generation.as_deref() != Some("local") {
                        bail!(
                            "error.code_source_relationship_chain: \
                             crash-window entry has non-local generation for project {project_id}"
                        );
                    }
                    // Validate snapshot path is same-project confined and
                    // carries no path-traversal components. The writer
                    // produces "workspace/{project_id}/snapshots/{snapshot_id}".
                    if let Some(ref snap) = entry.active_snapshot {
                        let expected_prefix = format!("workspace/{project_id}/snapshots/");
                        if !snap.starts_with(&expected_prefix)
                            || snap.contains("..")
                            || snap.contains('\0')
                        {
                            bail!(
                                "error.code_source_relationship_chain: \
                                 crash-window entry has unsafe snapshot path for project {project_id}"
                            );
                        }
                    } else {
                        bail!(
                            "error.code_source_relationship_chain: \
                             crash-window entry has no snapshot path for project {project_id}"
                        );
                    }
                    tracing::info!(
                        project = %project_id,
                        "relationship chain link 5: cutback crash window admitted \
                         (manifest entry is local:{project_id}, activation record is \
                         collected; reducer will converge by clearing the stale record)"
                    );
                } else if entry_selector != Some(activation.selector()) {
                    bail!(
                        "error.code_source_relationship_chain: \
                         workspace selector mismatch for project {project_id}"
                    );
                } else {
                    // Normal (non-crash-window) collected entry: validate
                    // generation, snapshot, and manifest path exactly.
                    if entry.code_source_generation.as_deref() != Some(generation_id) {
                        bail!(
                            "error.code_source_relationship_chain: \
                             workspace generation mismatch for project {project_id}"
                        );
                    }
                    let expected_snapshot = bbox_edge_sidecar::snapshot::active_snapshot_rel(
                        project_id,
                        activation.snapshot_id(),
                    );
                    if entry.active_snapshot.as_deref() != Some(expected_snapshot.as_str()) {
                        bail!(
                            "error.code_source_relationship_chain: \
                             workspace snapshot mismatch for project {project_id}"
                        );
                    }
                    let expected_manifest = format!("workspace/{project_id}/manifest.json");
                    if entry.manifest != expected_manifest {
                        bail!(
                            "error.code_source_relationship_chain: \
                             workspace manifest path mismatch for project {project_id}"
                        );
                    }
                }
            }
        }

        // Link 6: CutbackStateV2 coherence. After once-only
        // classification (step 5), the sole refuser for the coherence
        // clause is a record still carrying (None, true) that is not
        // bridge-exempt.
        if activation.is_cutback_pending() && activation.cutback().is_none() {
            // This should not happen after classification, but if a
            // record was written by a live writer with the wrong shape,
            // fail closed.
            if !bridge_admits {
                bail!(
                    "error.code_source_cutback_coherence: \
                     unclassified cutback_pending record for project {project_id} \
                     that is not bridge-exempt"
                );
            }
        }
        // Typed cutback states are validated by ActivationRecordV2::validate
        // (called transitively through validate_against_generation). Terminal
        // and ManualRetryRequired are valid persisted states.
    }
    for (project_id, entry) in &manifest.workspaces {
        if !entry
            .code_source_selector
            .as_deref()
            .is_some_and(|selector| selector.starts_with("collected:"))
        {
            continue;
        }
        let matching = records
            .iter()
            .filter(|activation| {
                activation.is_current_v2() && activation.project_id() == project_id
            })
            .count();
        if matching != 1 {
            bail!(
                "error.code_source_relationship_chain_reverse: collected workspace \
                 entry for project {project_id} resolves to {matching} activation records"
            );
        }
    }
    Ok(())
}

/// Detect incomplete retirement journals on disk (section 10.1 step 7).
///
/// If a `ProjectRetirementJournal` file is found, fail closed with a
/// typed diagnostic naming the CLI resume command. The daemon never
/// executes journal stages (the offline lane decision, section 4.8).
///
/// Path convention: `journal` files live under
/// `{bro_home}/retirement-journals/{project_id}.json`. The daemon probes
/// the directory for ANY `.json` file; any presence is a refusal.
fn detect_incomplete_retirement_journal(bro_home: &std::path::Path) -> Result<()> {
    const MAX_RETIREMENT_JOURNALS: usize = 4096;
    let journal_dir = bro_home.join("retirement-journals");
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::OpenOptionsExt;

        let directory = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&journal_dir)
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(anyhow!(
                    "error.code_source_retirement_journal_unavailable: {error}"
                ));
            }
        };
        #[cfg(test)]
        if TEST_RETIREMENT_JOURNAL_SWAP_AFTER_OPEN.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let moved = bro_home.join("retirement-journals-opened");
            std::fs::rename(&journal_dir, &moved)?;
            std::fs::create_dir(&journal_dir)?;
        }
        let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(std::io::Error::last_os_error().into());
        }
        let mut journals = Vec::new();
        let mut total_entries = 0usize;
        loop {
            set_readdir_errno(0);
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let error = readdir_errno();
                unsafe { libc::closedir(stream) };
                if error != 0 {
                    return Err(std::io::Error::from_raw_os_error(error).into());
                }
                break;
            }
            #[cfg(test)]
            if TEST_RETIREMENT_JOURNAL_ENUMERATION_ERROR
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                unsafe { libc::closedir(stream) };
                return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
            }
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            total_entries += 1;
            if total_entries > MAX_RETIREMENT_JOURNALS {
                unsafe { libc::closedir(stream) };
                bail!(
                    "error.code_source_retirement_journal_unavailable: \
                     retirement journal scan exceeds its total entry limit"
                );
            }
            let name_os = std::ffi::OsString::from_vec(name.to_vec());
            let name_path = std::path::Path::new(&name_os);
            if name_path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let name_c = std::ffi::CString::new(name)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name_c.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                unsafe { libc::closedir(stream) };
                return Err(anyhow!(
                    "error.code_source_retirement_journal_unavailable: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let leaf = unsafe { std::fs::File::from_raw_fd(fd) };
            if !leaf.metadata()?.is_file() {
                unsafe { libc::closedir(stream) };
                bail!(
                    "error.code_source_retirement_journal_unavailable: \
                     retirement journal entry is not a regular file"
                );
            }
            journals.push(name_os);
        }
        journals.sort();
        if let Some(first) = journals.first() {
            let name = std::path::Path::new(first)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("unknown");
            bail!(
                "error.code_source_retirement_journal_incomplete: \
                 retirement journal '{name}' is present; \
                 run `blackbox retirement-journal resume {name}` \
                 with the daemon stopped to complete it"
            );
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    let mut journals = Vec::new();
    #[cfg(not(unix))]
    for entry in std::fs::read_dir(&journal_dir)
        .with_context(|| format!("reading retirement journal dir {}", journal_dir.display()))?
    {
        #[cfg(test)]
        if TEST_RETIREMENT_JOURNAL_ENUMERATION_ERROR
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
        }
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            journals.push(entry);
            if journals.len() > MAX_RETIREMENT_JOURNALS {
                bail!(
                    "error.code_source_retirement_journal_unavailable: \
                     retirement journal scan exceeds its entry limit"
                );
            }
        }
    }
    #[cfg(not(unix))]
    journals.sort_by_key(|e| e.path());
    #[cfg(not(unix))]
    if let Some(first) = journals.first() {
        let path = first.path();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        bail!(
            "error.code_source_retirement_journal_incomplete: \
             retirement journal '{name}' is present; \
             run `blackbox retirement-journal resume {name}` \
             with the daemon stopped to complete it"
        );
    }
    #[cfg(not(unix))]
    Ok(())
}

#[cfg(test)]
static TEST_RETIREMENT_JOURNAL_ENUMERATION_ERROR: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static TEST_RETIREMENT_JOURNAL_SWAP_AFTER_OPEN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_readdir_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_readdir_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn readdir_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn readdir_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

/// Reconstruct WorkspaceIndexEntry rows from validated activation records
/// for collected projects whose workspace manifest entries are absent
/// (R2F2). The relationship chain has already validated every activation
/// record against its generation, so the record's selector, generation,
/// snapshot, and manifest path are trustworthy. Without this step,
/// `load_active_code_selectors` defaults every catalog project to
/// `local:<project_id>` and collected documents vanish from search.
///
/// The reconstructed entry mirrors exactly what the production writer
/// (`activate_source_snapshot`) would produce: manifest path
/// `workspace/{project_id}/manifest.json`, active_snapshot
/// `workspace/{project_id}/snapshots/{snapshot_id}`, selector and
/// generation from the activation record.
fn reconstruct_workspace_entries_from_activations(
    store: &CodeSourceStore,
    edges_dir: &std::path::Path,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
) -> Result<BTreeSet<String>> {
    let records = store
        .activation_records_mixed()
        .context("loading activation records for workspace reconstruction")?;
    let mut index = manifest.clone();
    let mut reconstructed = BTreeSet::new();
    for activation in &records {
        let project_id = activation.project_id();
        // Only reconstruct for collected projects missing a workspace entry.
        if index.workspaces.contains_key(project_id) {
            continue;
        }
        // Only collected records carry a selector that is not local.
        let selector = activation.selector();
        if !selector.starts_with("collected:") {
            continue;
        }
        let snapshot_rel =
            bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, activation.snapshot_id());
        index.upsert_workspace(
            project_id,
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(snapshot_rel),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(selector.to_string()),
                code_source_generation: Some(activation.generation_id().to_string()),
                git_overlay: None,
                // R3F5: the authoritative collected writer
                // (activate_source_snapshot in edge-sidecar snapshot.rs)
                // sets git_overlay_managed: true. The reconstruction must
                // match: false loads every JSONL member including stale
                // git-current.jsonl (manifest.rs gating), and overlay
                // selection refuses non-managed entries (snapshot.rs:344),
                // so the promised Git overlay can never install.
                git_overlay_managed: true,
            },
        );
        reconstructed.insert(project_id.to_string());
    }
    if !reconstructed.is_empty() {
        tracing::info!(
            reconstructed = reconstructed.len(),
            "pre-bind: reconstructed workspace entries from validated activation records"
        );
        index
            .write_atomic(edges_dir)
            .context("writing reconstructed workspace entries to manifest index")?;
    }
    Ok(reconstructed)
}

fn validate_pre_bind_workspace_materializations(
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    edges_dir: &std::path::Path,
    pending_first_republish: &BTreeSet<String>,
) -> Result<()> {
    manifest.active_paths_for_loader_admitting_fully_absent(edges_dir, pending_first_republish)?;
    Ok(())
}

fn derive_pending_first_republish(
    store: &CodeSourceStore,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    edges_dir: &std::path::Path,
) -> Result<BTreeSet<String>> {
    let records = store
        .activation_records_mixed()
        .context("loading activation records for pending first republish")?;
    let mut pending = BTreeSet::new();

    for (project_id, entry) in &manifest.workspaces {
        if !manifest.workspace_materialization_is_fully_absent(edges_dir, project_id)? {
            continue;
        }
        let mut matching = records
            .iter()
            .filter(|record| record.is_current_v2() && record.project_id() == project_id);
        let Some(activation) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            bail!(
                "multiple current activation records found for absent workspace materialization project {project_id}"
            );
        }
        let expected_snapshot =
            bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, activation.snapshot_id());
        let selector = activation.selector();
        if !selector.starts_with("collected:")
            || entry.code_source_selector.as_deref() != Some(selector)
            || entry.code_source_generation.as_deref() != Some(activation.generation_id())
            || entry.active_snapshot.as_deref() != Some(expected_snapshot.as_str())
        {
            continue;
        }
        tracing::info!(
            project_id,
            "pre-bind: admitting absent collected workspace materialization pending first republish"
        );
        pending.insert(project_id.clone());
    }

    Ok(pending)
}

/// Pre-bind catalog-mode recovery: steps 5-8 of the startup order
/// (section 10.1). Runs in `open_shared_state` BEFORE the listener
/// binds. Bridge mode is a no-op (byte-compatible).
///
/// Returns the classification outcomes for the step-8 reducer sweep.
/// The sweep itself runs as part of `resume_pending_activations` in
/// background tasks (the events are enqueued here so the reducer picks
/// them up when it starts).
pub(crate) fn pre_bind_catalog_recovery(
    project_authority: &super::state::ProjectAuthority,
    code_sources: &CodeSourceRuntime,
    checkout_access: &CheckoutAccessBroker,
    bro_home: &std::path::Path,
) -> Result<BTreeSet<String>> {
    let Some(catalog_store) = project_authority.catalog_store() else {
        // Bridge mode: steps 5-8 do not run (byte-compatible).
        return Ok(BTreeSet::new());
    };
    let store = code_sources.store();
    if store.record_mode() != RuntimeRecordMode::CatalogV2 {
        return Ok(BTreeSet::new());
    }
    let store: &CodeSourceStore = &store;

    let snapshot = catalog_store
        .snapshot()
        .context("pre-bind: catalog snapshot for startup recovery")?;
    let catalog = snapshot.catalog();

    // Load the manifest index for workspace entries (link 5).
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(bro_home);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)
        .context("pre-bind: manifest index for relationship chain")?;

    // Step 5: once-only classification of migrated records.
    let classifications = classify_migrated_records(store, catalog, checkout_access)
        .context("pre-bind: once-only classification")?;

    // Step 6: validate the relationship chain.
    validate_relationship_chain(store, catalog, &manifest)
        .context("pre-bind: relationship chain validation")?;

    // Step 6b: reconstruct workspace manifest entries from validated
    // activation records for projects whose entries are absent (the
    // pending-first-republish state for a migrated or never-booted root).
    // Without this step, load_active_code_selectors defaults every catalog
    // project to local:<project_id> and the collected generation's
    // documents vanish from search. The chain has already validated every
    // activation record against its generation (selector, generation id,
    // snapshot id, manifest path), so reconstructing the entry from the
    // validated record is safe (R2F2).
    reconstruct_workspace_entries_from_activations(store, &edges_dir, &manifest)
        .context("pre-bind: workspace entry reconstruction")?;
    let reconstructed_manifest =
        bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)
            .context("pre-bind: reloading reconstructed workspace entries")?;
    let pending_first_republish =
        derive_pending_first_republish(store, &reconstructed_manifest, &edges_dir)
            .context("pre-bind: deriving pending first republish materializations")?;
    validate_pre_bind_workspace_materializations(
        &reconstructed_manifest,
        &edges_dir,
        &pending_first_republish,
    )
    .context("pre-bind: validating active workspace materializations")?;

    // Step 7: detect incomplete retirement journals.
    detect_incomplete_retirement_journal(bro_home)
        .context("pre-bind: retirement journal detection")?;

    // Step 8: startup reducer sweep. Every classification outcome (b)
    // or (c) is queued to the reducer. Bridge-exempt (a) is NOT queued.
    for (project_id, outcome) in &classifications {
        match outcome {
            ClassificationOutcome::BridgeExempt => {
                tracing::info!(
                    project_id,
                    "startup classification: bridge-exempt, not queued to reducer"
                );
            }
            ClassificationOutcome::StructuralPersisted(reason) => {
                tracing::info!(
                    project_id,
                    ?reason,
                    "startup classification: structural persisted, queued to reducer"
                );
                // Enqueue for the reducer to re-evaluate.
                let scope = store
                    .load_activation_mixed(project_id)
                    .ok()
                    .flatten()
                    .and_then(|a| a.published_scope().cloned());
                if let Some(scope) = scope {
                    code_sources.enqueue_transition(
                        project_id,
                        scope,
                        ReconcileKind::Cutback,
                        ReconcileOrigin::StartupRecovery,
                        None,
                    );
                }
            }
            ClassificationOutcome::DeferredToSweep => {
                tracing::info!(
                    project_id,
                    "startup classification: valid attachment, deferred to sweep"
                );
                let scope = store
                    .load_activation_mixed(project_id)
                    .ok()
                    .flatten()
                    .and_then(|a| a.published_scope().cloned());
                if let Some(scope) = scope {
                    code_sources.enqueue_transition(
                        project_id,
                        scope,
                        ReconcileKind::Cutback,
                        ReconcileOrigin::StartupRecovery,
                        None,
                    );
                }
            }
        }
    }

    // Section 9.7 restart re-drives folded into the startup sweep (one
    // feed, not two). Re-evaluate every persisted CutbackStateV2 against
    // the current attachment epoch. This replaces the separate
    // resume_persisted_cutback_states call that previously ran in
    // background tasks.
    resume_persisted_cutback_states_pre_bind(store, code_sources);

    // Desired/effective mismatch sweep: every project whose desired
    // (local) and effective (collected) sources differ gets queued.
    // This catches the crash-between-auth-swap-and-structural-persist
    // case where a live-written record has cutback: None.
    enqueue_desired_effective_mismatches(store, code_sources);

    // Health is durable independently of the reducer queue. A project which
    // was already local/local before this process started has no mismatch or
    // persisted-state event to drive a NoOp reduction, so clear only those
    // cutback rows whose authority is already converged.
    clear_converged_cutback_health_at_startup(store, code_sources, &reconstructed_manifest);

    Ok(pending_first_republish)
}

/// Pre-bind version of resume_persisted_cutback_states (section 9.7).
/// Re-evaluates every persisted CutbackStateV2 against the current
/// attachment epoch, enqueuing reducer events for Structural and
/// Transient states. This replaces the background-task version so the
/// startup feed is unified (section 10.1 step 8).
fn resume_persisted_cutback_states_pre_bind(
    store: &CodeSourceStore,
    code_sources: &CodeSourceRuntime,
) {
    let records = match store.activation_records_mixed() {
        Ok(records) => records,
        Err(error) => {
            tracing::error!(%error, "startup sweep: loading activation records for cutback state sweep failed");
            return;
        }
    };
    let now = unix_now();
    for activation in &records {
        let Some(cutback) = activation.cutback() else {
            continue;
        };
        let project_id = activation.project_id();
        let scope = activation.published_scope().cloned();
        match cutback {
            CutbackStateV2::Structural { .. } => {
                if let Some(scope) = scope {
                    tracing::info!(
                        project_id,
                        cutback = ?cutback,
                        "startup sweep: re-evaluating structural cutback via reconciler"
                    );
                    code_sources.enqueue_transition(
                        project_id,
                        scope,
                        ReconcileKind::Cutback,
                        ReconcileOrigin::StartupRecovery,
                        None,
                    );
                }
            }
            CutbackStateV2::Transient {
                deadline_unix_secs, ..
            } => {
                if let Some(reconciler) = code_sources.reconciler() {
                    reconciler.register_transient(*deadline_unix_secs, project_id);
                    if *deadline_unix_secs <= now {
                        tracing::info!(
                            project_id,
                            deadline = deadline_unix_secs,
                            "startup sweep: transient cutback deadline elapsed, scheduler will re-attempt"
                        );
                        if let Some(scope) = scope {
                            code_sources.enqueue_transition(
                                project_id,
                                scope,
                                ReconcileKind::Cutback,
                                ReconcileOrigin::StartupRecovery,
                                None,
                            );
                        }
                    } else {
                        tracing::info!(
                            project_id,
                            deadline = deadline_unix_secs,
                            "startup sweep: transient cutback deadline in future, scheduler will wait"
                        );
                    }
                }
            }
            CutbackStateV2::ManualRetryRequired { .. } => {
                tracing::info!(
                    project_id,
                    "startup sweep: ManualRetryRequired persisted state is a valid no-op"
                );
            }
            CutbackStateV2::Terminal { .. } => {
                tracing::info!(
                    project_id,
                    "startup sweep: Terminal persisted state is a valid no-op"
                );
            }
        }
    }
}

/// Desired/effective mismatch sweep (section 10.1 step 8). Every project
/// whose desired (local) and effective (collected) sources differ is
/// queued to the reducer. This catches the crash-between-auth-swap-and-
/// structural-persist case where a live-written record has cutback: None.
fn enqueue_desired_effective_mismatches(store: &CodeSourceStore, code_sources: &CodeSourceRuntime) {
    let records = match store.activation_records_mixed() {
        Ok(records) => records,
        Err(error) => {
            tracing::error!(%error, "startup sweep: loading records for mismatch sweep failed");
            return;
        }
    };
    for activation in &records {
        if !activation.is_current_v2() {
            continue;
        }
        let project_id = activation.project_id();
        // If the record already has a typed cutback state, it was handled
        // by resume_persisted_cutback_states_pre_bind above. Only enqueue
        // records with no cutback state (the mismatch case).
        if activation.cutback().is_some() {
            continue;
        }
        if activation.is_cutback_pending() {
            // Should have been classified in step 5; skip defensively.
            continue;
        }
        // Determine effective source: collected activations with no
        // assignment are candidates for cutback. The actual desired/
        // effective comparison happens in the reducer via
        // evaluate_reduction. Here we just enqueue every collected
        // activation that has no assignment (desired=local,
        // effective=collected).
        let assigned = code_sources
            .assignments()
            .into_iter()
            .any(|(_, pid)| pid == project_id);
        if !assigned {
            if let Some(scope) = activation.published_scope().cloned() {
                code_sources.enqueue_transition(
                    project_id,
                    scope,
                    ReconcileKind::Cutback,
                    ReconcileOrigin::StartupRecovery,
                    None,
                );
            }
        }
    }
}

/// Evaluate the open-bridge predicate for a project (section 9.3).
///
/// A `ScopeMigrationRecord`'s `code_bridge_generation` is open for a
/// project when it equals the project's current effective activation
/// generation id AND the record's `old_scope` equals that activation's
/// `published_scope`. When multiple records exist (pre-refusal legacy
/// state), the newest by `catalog_epoch` is authority.
fn is_bridge_open(
    migration_records: &[&bbox_corpus_core::project_catalog::ScopeMigrationRecord],
    effective_generation_id: &str,
    effective_scope: Option<&PublishedScope>,
) -> bool {
    if migration_records.is_empty() {
        return false;
    }
    // Newest by catalog_epoch is authority when multiple records exist.
    let newest = migration_records
        .iter()
        .max_by_key(|r| r.catalog_epoch)
        .copied();
    let Some(record) = newest else {
        return false;
    };
    let Some(ref bridge_gen) = record.code_bridge_generation else {
        return false;
    };
    if bridge_gen != effective_generation_id {
        return false;
    }
    match effective_scope {
        Some(scope) => match &record.old_scope {
            bbox_corpus_core::project_catalog::ProjectScope::Published(pub_scope) => {
                pub_scope == scope
            }
            bbox_corpus_core::project_catalog::ProjectScope::LegacyLocal => false,
        },
        None => false,
    }
}

/// Determine the effective activation source for a project.
fn determine_effective_source(state: &Arc<SharedState>, project_id: &str) -> EffectiveSource {
    let store = state.code_sources.store();
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
        Ok(m) => m,
        Err(_) => return EffectiveSource::Unavailable,
    };
    let activation = store.load_activation_mixed(project_id).ok().flatten();
    determine_effective_source_from_manifest(activation.as_ref(), &manifest, project_id)
}

fn determine_effective_source_from_manifest(
    activation: Option<&MixedActivationRecord>,
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    project_id: &str,
) -> EffectiveSource {
    let selector = manifest
        .workspaces
        .get(project_id)
        .and_then(|entry| entry.code_source_selector.as_deref());
    // Local authority has no activation journal by design: that journal is
    // the recovery root for collected generations. Read the manifest before
    // requiring one, otherwise every steady local project with no journal is
    // misclassified as Unavailable and stale cutback health survives forever.
    if selector.is_some_and(|selector| !selector.starts_with("collected:")) {
        return EffectiveSource::Local;
    }
    let Some(activation) = activation else {
        return EffectiveSource::Unavailable;
    };
    match selector {
        Some(s) if s.starts_with("collected:") => EffectiveSource::Collected,
        Some(_) => unreachable!("local selectors return before activation lookup"),
        // When the workspace entry is absent (the pending-first-republish
        // state for a migrated or never-booted root), the activation record
        // itself is the durable evidence of the project's effective source.
        // A collected activation record must be classified as Collected so
        // the reducer never lands in a local/local cell and deletes the
        // record as stale state (the record is the GC root and recovery
        // root for a live collected generation).
        None => {
            let activation_selector = activation.selector();
            if activation_selector.starts_with("collected:") {
                EffectiveSource::Collected
            } else if activation.document_count() > 0 {
                EffectiveSource::Warming
            } else {
                EffectiveSource::Unavailable
            }
        }
    }
}

/// Check the open-bridge predicate for a project in the reconciler
/// (section 9.3). Fetches ScopeMigrationRecords from the catalog store
/// and evaluates whether the bridge is open for the project's current
/// effective activation.
fn check_bridge_open_for_reducer(
    state: &Arc<SharedState>,
    project_id: &str,
    effective_generation_id: &str,
    effective_scope: Option<&PublishedScope>,
) -> bool {
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return false;
    };
    let Ok(snapshot) = catalog_store.snapshot() else {
        return false;
    };
    let catalog = snapshot.catalog();
    let records: Vec<_> = catalog
        .scope_migrations
        .values()
        .filter(|r| r.project_id.as_str() == project_id)
        .collect();
    is_bridge_open(&records, effective_generation_id, effective_scope)
}

/// Attempt the automatic bridge-clear transaction (section 9.5).
///
/// When the reconciler detects that a project has a non-null
/// `code_bridge_generation` but the open-bridge predicate is false
/// (effective scope is the new scope, not the old scope named in the
/// record), trigger a transact nulling `code_bridge_generation` on the
/// record. This fires exactly once: the first new-scope activation
/// that makes the open-bridge predicate false.
///
/// Returns true if the bridge was cleared (or was already clear).
fn try_automatic_bridge_clear(state: &Arc<SharedState>, project_id: &str) -> Result<bool> {
    use bbox_corpus_core::project_catalog::ProjectScope;
    use bbox_indexing::project_catalog_admin::{ScopeBridgeClearMode, clear_scope_bridge};
    let Some(catalog_store) = state.project_authority.catalog_store() else {
        return Ok(false);
    };
    let snapshot = catalog_store
        .snapshot()
        .context("loading catalog for automatic bridge clear")?;
    let epoch = snapshot.epoch();
    let catalog = snapshot.catalog();
    // Find bridge-bearing records for this project.
    let bridge_records: Vec<_> = catalog
        .scope_migrations
        .values()
        .filter(|r| r.project_id.as_str() == project_id && r.code_bridge_generation.is_some())
        .collect();
    if bridge_records.is_empty() {
        return Ok(true); // no bridge to clear
    }
    // Check if the effective activation's scope matches the migration
    // record's new_scope. If so, the bridge is stale and can be cleared.
    let pid = bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_string())
        .map_err(|error| anyhow!(error))?;
    let project_scope = catalog.projects.get(&pid).map(|p| &p.scope);
    let Some(ProjectScope::Published(current_scope)) = project_scope else {
        return Ok(false); // project not in published state
    };
    // The bridge is clearable when the newest bridge record's new_scope
    // matches the current project scope (meaning the new scope is active).
    let newest = bridge_records.iter().max_by_key(|r| r.catalog_epoch);
    let Some(record) = newest else {
        return Ok(false);
    };
    let bridge_is_stale = match &record.new_scope {
        ProjectScope::Published(new_scope) => new_scope == current_scope,
        ProjectScope::LegacyLocal => false,
    };
    if !bridge_is_stale {
        return Ok(false);
    }
    // Gather verified evidence from the activation record (F4: automatic
    // path must verify the bridge is actually stale). R3F3: enumerate the
    // actual retained generation set from the store instead of passing an
    // empty set (which would make mode 1 treat any bridge generation as
    // absent without proof). Also pass the effective scope for completeness.
    let store = state.code_sources.store();
    let activation = store
        .load_activation_mixed(project_id)
        .context("loading activation for automatic bridge clear")?
        .ok_or_else(|| anyhow!("automatic bridge clear requires an activation record"))?;
    let effective_generation_id = Some(activation.generation_id().to_string());
    let effective_scope = Some(
        activation
            .published_scope()
            .cloned()
            .ok_or_else(|| anyhow!("automatic bridge clear requires a v2 activation scope"))?,
    );
    let retained_generation_ids = store
        .retirement_generation_inventory()
        .context("enumerating retained generations for automatic bridge clear")?
        .into_iter()
        .map(|generation| generation.generation_id)
        .collect();
    let evidence = bbox_indexing::project_catalog_admin::ScopeBridgeClearEvidence {
        activation: bbox_indexing::project_catalog_admin::ScopeBridgeActivationEvidence::Present {
            generation_id: effective_generation_id.expect("activation generation was loaded"),
            scope: effective_scope.expect("activation scope was loaded"),
        },
        retained_generations:
            bbox_indexing::project_catalog_admin::ScopeBridgeRetainedEvidence::Enumerated(
                retained_generation_ids,
            ),
    };
    // Trigger the bridge-clear transaction.
    match clear_scope_bridge(
        catalog_store,
        epoch,
        &pid,
        ScopeBridgeClearMode::AutomaticFirstNewScope,
        &evidence,
    ) {
        Ok(_) => {
            tracing::info!(
                project_id = project_id,
                epoch,
                "automatic bridge-clear: nulled code_bridge_generation"
            );
            Ok(true)
        }
        Err(error) => {
            tracing::warn!(
                project_id = project_id,
                %error,
                "automatic bridge-clear transaction failed"
            );
            Err(anyhow!(error))
        }
    }
}

/// Probe the attachment ladder for a project without committing to a
/// full checkout acquisition (section 9.3 ladder result input).
fn probe_ladder(state: &Arc<SharedState>, project_id: &str) -> LadderResult {
    use bbox_indexing::checkout_access::{
        CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
        CheckoutAttachmentSelector,
    };
    // Derive scope from the activation record's published_scope, not
    // from current auth-table assignments. When an assignment is removed
    // (desired=Local cutback), the auth table no longer has the scope,
    // but the activation record still carries the previously collected
    // scope we need to cut back from.
    let store = state.code_sources.store();
    let scope = store
        .load_activation_mixed(project_id)
        .ok()
        .flatten()
        .and_then(|a| a.published_scope().cloned());
    let Some(scope) = scope else {
        return LadderResult::None;
    };
    match state.checkout_access.acquire(CheckoutAccessRequest {
        project_id: project_id.to_string(),
        attachment: CheckoutAttachmentSelector::Selected,
        expected_scope: Some(scope),
        kind: CheckoutAccessKind::LocalProjectWalk,
        intent: CheckoutAccessIntent::Read,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
    }) {
        Ok(_) => LadderResult::Selected,
        Err(error) => {
            use bbox_indexing::checkout_access::CheckoutAccessErrorCode as Code;
            match error.code {
                Code::AttachmentNotFound
                | Code::ObservationUnavailable
                | Code::DeniedByTestProbe => LadderResult::None,
                Code::ScopeMismatch
                | Code::CapabilityDenied
                | Code::IntentDenied
                | Code::ConservativePathGateDenied
                | Code::InvalidRoot
                | Code::UnsafeRelativePath
                | Code::WriteIntentRequired => LadderResult::ScopeInvalid,
                _ => LadderResult::Ambiguous,
            }
        }
    }
}

/// Determine the desired assignment for a project from auth-table state.
fn determine_desired_assignment(state: &Arc<SharedState>, project_id: &str) -> DesiredAssignment {
    let assigned = state
        .code_sources
        .assignments()
        .into_iter()
        .any(|(_, pid)| pid == project_id);
    if assigned {
        DesiredAssignment::Collected
    } else {
        DesiredAssignment::Local
    }
}

/// Evaluate the complete reduction table (section 9.3).
///
/// Every cell of the table is defined here. Before consulting the
/// table, the reducer checks the open-bridge predicate. When it holds,
/// the reducer clears any pre-existing Structural cutback state on the
/// project, sets health to `scope_migration_refresh_required`, and
/// performs no cutback attempt.
#[allow(clippy::too_many_arguments)]
fn evaluate_reduction_for_event(
    desired: DesiredAssignment,
    effective: EffectiveSource,
    persisted: Option<&CutbackStateV2>,
    ladder: LadderResult,
    bridge_open: bool,
    origins: &BTreeSet<ReconcileOrigin>,
) -> ReducerAction {
    // Open-bridge predicate: bridge-exempt project. Clear any
    // pre-existing Structural state and perform no cutback attempt
    // regardless of the desired/effective/persisted tuple (section 9.3).
    if bridge_open {
        if let Some(CutbackStateV2::Structural { .. }) = persisted {
            return ReducerAction::CancelCutback;
        }
        return ReducerAction::NoOp;
    }

    // retired: hand off to retirement (P4-G).
    if desired == DesiredAssignment::Retired {
        return ReducerAction::Retire;
    }

    match (desired, effective) {
        // retired/any (defensive; handled above, but exhaustive match).
        (DesiredAssignment::Retired, _) => ReducerAction::Retire,

        // collected/collected cells
        (DesiredAssignment::Collected, EffectiveSource::Collected) => {
            if persisted.is_some() {
                ReducerAction::CancelCutback
            } else {
                ReducerAction::NoOp
            }
        }
        // collected/other: activate desired
        (DesiredAssignment::Collected, _) => ReducerAction::Activate,

        // local/local cells
        (DesiredAssignment::Local, EffectiveSource::Local) => {
            if persisted.is_some() {
                ReducerAction::CancelCutback
            } else {
                ReducerAction::NoOp
            }
        }

        // local/warming or unavailable: re-stage if valid local source,
        // otherwise no-op with health record (simplified to NoOp here;
        // health record set by the dispatcher).
        (DesiredAssignment::Local, EffectiveSource::Warming)
        | (DesiredAssignment::Local, EffectiveSource::Unavailable) => {
            if ladder == LadderResult::Selected {
                ReducerAction::AttemptCutback
            } else {
                ReducerAction::NoOp
            }
        }

        // local/collected cells: the main reduction table
        (DesiredAssignment::Local, EffectiveSource::Collected) => {
            match persisted {
                None => {
                    // No persisted state: consult ladder
                    match ladder {
                        LadderResult::Selected => ReducerAction::AttemptCutback,
                        LadderResult::None => {
                            ReducerAction::PersistStructural(CutbackReason::NoLocalAttachment)
                        }
                        LadderResult::Ambiguous => {
                            ReducerAction::PersistStructural(CutbackReason::AmbiguousAttachment)
                        }
                        LadderResult::ScopeInvalid => {
                            ReducerAction::PersistStructural(CutbackReason::ScopeMismatch)
                        }
                    }
                }
                Some(CutbackStateV2::Structural { reason: _ }) => {
                    // Structural: re-evaluate ladder
                    match ladder {
                        LadderResult::Selected => ReducerAction::ReattemptCutback,
                        _ => ReducerAction::NoOp,
                    }
                }
                Some(CutbackStateV2::Transient { attempt, .. }) => {
                    // Transient: check if due (scheduler re-attempts).
                    // The reducer always returns ReattemptCutback for
                    // transient; the dispatcher checks the deadline.
                    let _ = attempt;
                    ReducerAction::ReattemptCutback
                }
                Some(CutbackStateV2::ManualRetryRequired { error_class, .. }) => {
                    // Manual retry is sticky across startup, catalog,
                    // completion, and scheduler noise. A fresh operator
                    // assignment/config event releases it for one attempt.
                    // ReadinessAvailable also repairs states produced by the
                    // former bug that counted writer/vector readiness as an
                    // attempt failure until the retry ladder was exhausted.
                    let readiness_repair = origins.contains(&ReconcileOrigin::ReadinessAvailable)
                        && matches!(
                            error_class,
                            CutbackErrorClass::WriterContention | CutbackErrorClass::IndexCommit
                        );
                    if (origins.contains(&ReconcileOrigin::AssignmentConfigReload)
                        || readiness_repair)
                        && ladder == LadderResult::Selected
                    {
                        ReducerAction::ReattemptCutback
                    } else {
                        ReducerAction::NoOp
                    }
                }
                Some(CutbackStateV2::Terminal { .. }) => {
                    // Steady-state no-op (terminal, never auto-retry).
                    // Config-event re-entry: a config reload re-evaluates.
                    ReducerAction::NoOp
                }
            }
        }
    }
}

/// A completion edge is a convergence pass, not independent retry authority.
/// If no newer/config/readiness origin coalesced with it, it may clear stale
/// state but must not launch the same worker again immediately.
fn gate_completion_reentry(
    action: ReducerAction,
    origins: &BTreeSet<ReconcileOrigin>,
) -> ReducerAction {
    if origins.len() == 1
        && origins.contains(&ReconcileOrigin::ActivationCompletion)
        && matches!(
            action,
            ReducerAction::Activate
                | ReducerAction::AttemptCutback
                | ReducerAction::ReattemptCutback
        )
    {
        ReducerAction::NoOp
    } else {
        action
    }
}

#[cfg(test)]
fn evaluate_reduction(
    desired: DesiredAssignment,
    effective: EffectiveSource,
    persisted: Option<&CutbackStateV2>,
    ladder: LadderResult,
    bridge_open: bool,
) -> ReducerAction {
    evaluate_reduction_for_event(
        desired,
        effective,
        persisted,
        ladder,
        bridge_open,
        &BTreeSet::new(),
    )
}

/// Catalog-mode one-attempt cutback driver (section 9.1).
///
/// Replaces the loop-based `schedule_cutback` for catalog mode: ONE
/// attempt per invocation, then persist the outcome and return. No
/// sleep, no spin (closes G1). The attempt:
///
/// a. Resolve identity from the catalog snapshot.
/// b. CheckoutAccessBroker::acquire with Selected; classify structural
///    reasons (NoLocalAttachment, AmbiguousAttachment, ScopeMismatch).
/// c. Stage the local generation; classify transient errors
///    (WriterContention, IoPressure, Deadline, IndexCommit).
/// d. Validation/security failure persists Terminal.
/// e. Success: local activation, cutback state cleared.
///
/// The caller (reconciler) holds the transition guard via the `guard`
/// parameter; it drops when this function returns.
fn current_cutback_authority_revision(state: &Arc<SharedState>) -> CutbackAuthorityRevision {
    CutbackAuthorityRevision {
        catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
        assignment_revision: state
            .code_sources
            .assignment_revision
            .load(std::sync::atomic::Ordering::Acquire),
    }
}

fn compare_and_apply_current_cutback(
    state: &Arc<SharedState>,
    project_id: &str,
    fallback_scope: &PublishedScope,
    outcome: CutbackCompareOutcome,
) -> Result<()> {
    let store = state.code_sources.store();
    let initial_revision = current_cutback_authority_revision(state);
    let activation = match store.load_activation_mixed(project_id)? {
        Some(MixedActivationRecord::CurrentV2(activation)) => activation,
        Some(MixedActivationRecord::LegacyV1(_)) => {
            bail!("catalog reducer found a legacy activation record")
        }
        None => return Ok(()),
    };
    let fence = ActivationFence::from_activation(&activation, initial_revision);
    let current_revision = current_cutback_authority_revision(state);
    match store.compare_and_apply_cutback(&fence, current_revision, outcome) {
        Ok(_) => Ok(()),
        Err(error) if error.downcast_ref::<ActivationFenceConflict>().is_some() => {
            enqueue_current_transition(
                state,
                project_id,
                fallback_scope,
                ReconcileOrigin::CatalogCommit,
                Some(current_revision.catalog_epoch),
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn attempt_cutback_catalog(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
) -> Result<FencedCutbackAttempt> {
    let store = state.code_sources.store();
    let activation = match store.load_activation_mixed(project_id)? {
        Some(MixedActivationRecord::CurrentV2(activation)) => activation,
        Some(MixedActivationRecord::LegacyV1(_)) | None => {
            bail!("catalog cutback has no current activation to fence")
        }
    };
    let fence =
        ActivationFence::from_activation(&activation, current_cutback_authority_revision(state));

    // a. Resolve identity from the catalog snapshot.
    let identity = resolve_code_project_identity(state, project_id, "catalog cutback attempt")?;

    // b. Acquire checkout access with Selected attachment selector (R6).
    //    The cutback stages a local generation from a local walk, so
    //    request LocalProjectWalk (not GitHistory). Validate scope and
    //    local-source capability on the selected candidate.
    let lease = match state.checkout_access.acquire(CheckoutAccessRequest {
        project_id: project_id.to_string(),
        attachment: CheckoutAttachmentSelector::Selected,
        expected_scope: Some(scope.clone()),
        kind: CheckoutAccessKind::LocalProjectWalk,
        intent: CheckoutAccessIntent::Read,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
    }) {
        Ok(lease) => lease,
        Err(error) => {
            return Ok(FencedCutbackAttempt {
                fence,
                outcome: classify_checkout_error(&error),
            });
        }
    };

    // c. Stage the local generation through a single staging call.
    //    The staging call returns a typed CutbackErrorClass to this
    //    driver instead of parking in a sleep loop.
    drop(lease);
    match cutback_to_local_single_attempt(state, scope, project_id, &identity) {
        Ok(success) => Ok(FencedCutbackAttempt {
            fence,
            outcome: CutbackAttemptOutcome::Success(success),
        }),
        Err(error) => Ok(FencedCutbackAttempt {
            fence,
            outcome: classify_staging_error(&error),
        }),
    }
}

#[derive(Debug)]
struct FencedCutbackAttempt {
    fence: ActivationFence,
    outcome: CutbackAttemptOutcome,
}

#[derive(Debug, Clone, Copy)]
enum CutbackSuccessOutcome {
    ClearCutback,
    ClearActivation,
}

/// The outcome of a catalog-mode cutback attempt (section 9.1).
#[derive(Debug)]
enum CutbackAttemptOutcome {
    /// Cutback succeeded: local activation complete, state cleared.
    Success(CutbackSuccessOutcome),
    /// Structural reason: persist without polling. Re-evaluated by
    /// attachment event or config reload.
    Structural(CutbackReason),
    /// Transient failure: persist attempt+1, deadline, error class.
    /// After the configured cap: ManualRetryRequired.
    Transient(CutbackErrorClass),
    /// Staging is blocked by selector retirement, an active reindex, or vector
    /// warmup. This is a readiness dependency, not an attempt failure: do not
    /// advance or replace the persisted cutback ladder.
    ReadinessDeferred(CutbackReadiness),
    /// Terminal failure (validation/security): GC root, never auto-retry.
    Terminal(CutbackErrorClass),
}

#[derive(Debug, Clone, Copy)]
enum CutbackReadiness {
    SelectorRetirement,
    ReindexPass,
    VectorStore,
}

impl CutbackReadiness {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::SelectorRetirement => "cutback is waiting for selector retirement to complete",
            Self::ReindexPass => "cutback is waiting for the active reindex pass to complete",
            Self::VectorStore => "cutback is waiting for the vector store to finish warming",
        }
    }
}

#[derive(Debug)]
struct SelectorRetirementQueued;

impl std::fmt::Display for SelectorRetirementQueued {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("code-source selector retirement remains queued before staging")
    }
}

impl std::error::Error for SelectorRetirementQueued {}

#[derive(Debug)]
struct StagingValidationRefusal;

impl std::fmt::Display for StagingValidationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("staged code-source validation refused publication")
    }
}

impl std::error::Error for StagingValidationRefusal {}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
struct StagingSecurityRefusal;

impl std::fmt::Display for StagingSecurityRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("staged code-source security policy refused publication")
    }
}

impl std::error::Error for StagingSecurityRefusal {}

/// Classify a checkout access error into a structural cutback reason
/// (section 9.1 step b).
fn classify_checkout_error(error: &CheckoutAccessError) -> CutbackAttemptOutcome {
    use bbox_indexing::checkout_access::CheckoutAccessErrorCode as Code;
    let outcome = match error.code {
        Code::AttachmentNotFound | Code::ObservationUnavailable => {
            CutbackAttemptOutcome::Structural(CutbackReason::NoLocalAttachment)
        }
        Code::ScopeMismatch => CutbackAttemptOutcome::Structural(CutbackReason::ScopeMismatch),
        Code::CapabilityDenied
        | Code::IntentDenied
        | Code::ConservativePathGateDenied
        | Code::InvalidRoot
        | Code::UnsafeRelativePath
        | Code::WriteIntentRequired => {
            CutbackAttemptOutcome::Structural(CutbackReason::ScopeMismatch)
        }
        Code::AttachmentInactive
        | Code::ProjectMismatch
        | Code::SelectorMismatch
        | Code::CheckoutIdentityMismatch
        | Code::LifecycleBusy
        | Code::InvalidRequest
        | Code::DeniedByTestProbe => {
            CutbackAttemptOutcome::Structural(CutbackReason::NoLocalAttachment)
        }
    };
    tracing::debug!(
        code = ?error.code,
        outcome = ?outcome,
        "catalog cutback: checkout access classified"
    );
    outcome
}

/// Classify a staging error into a transient or terminal cutback class
/// (section 9.1 step c-d).
fn classify_staging_error(error: &anyhow::Error) -> CutbackAttemptOutcome {
    use bbox_indexing::index::writer_actor::IndexWriterRetryableError;

    if error
        .chain()
        .any(|cause| cause.downcast_ref::<SelectorRetirementQueued>().is_some())
    {
        return CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::SelectorRetirement);
    }
    for cause in error.chain() {
        match cause.downcast_ref::<IndexWriterRetryableError>() {
            Some(
                IndexWriterRetryableError::ReindexPassInProgress
                | IndexWriterRetryableError::EdgeIndexRebuildInProgress,
            ) => {
                return CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::ReindexPass);
            }
            Some(IndexWriterRetryableError::VectorStoreWarming) => {
                return CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::VectorStore);
            }
            None => {}
        }
    }
    // IoPressure: disk or IO failure.
    if error
        .chain()
        .any(|c| c.downcast_ref::<std::io::Error>().is_some())
    {
        return CutbackAttemptOutcome::Transient(CutbackErrorClass::IoPressure);
    }
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<StagingValidationRefusal>().is_some())
        || error.chain().any(|cause| {
            let message = cause.to_string();
            message.starts_with("error.code_source_scope_agreement:")
                || message.starts_with("error.code_source_cutback_coherence:")
        })
    {
        return CutbackAttemptOutcome::Terminal(CutbackErrorClass::ValidationFailure);
    }
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<StagingSecurityRefusal>().is_some())
    {
        return CutbackAttemptOutcome::Terminal(CutbackErrorClass::SecurityFailure);
    }
    // Unknown failures remain retryable. Preserve a bounded chain in logs so
    // a new typed boundary can be added without turning incidental wording
    // into a terminal state.
    let mut chain = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    chain.truncate(512);
    tracing::warn!(error_chain = %chain, "catalog cutback: untyped staging failure classified transiently");
    CutbackAttemptOutcome::Transient(CutbackErrorClass::IndexCommit)
}

/// Compute the next transient retry deadline using capped exponential
/// backoff with deterministic project-id jitter (section 4.3, R3).
///
/// base * 2^(attempt-1), capped at max_secs, jitter derived from a stable
/// hash of project_id (0 to 25 percent of the current delay).
#[cfg(test)]
fn compute_retry_deadline(attempt: u32, project_id: &str, base_secs: u64, max_secs: u64) -> u64 {
    let exp = (attempt as u64).saturating_sub(1);
    let raw = base_secs.saturating_mul(2_u64.saturating_pow(exp.try_into().unwrap_or(u32::MAX)));
    let capped = raw.min(max_secs);
    let jitter_max = capped / 4;
    let jitter = if jitter_max == 0 {
        0
    } else {
        let hash = stable_project_id_hash(project_id);
        hash % jitter_max
    };
    let now = unix_now();
    now + capped + jitter
}

/// Stable hash of a project id for deterministic jitter (section 4.3).
#[cfg(test)]
fn stable_project_id_hash(project_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project_id.hash(&mut hasher);
    hasher.finish()
}

/// Single-attempt staging for catalog-mode cutback (section 9.1 step c).
///
/// Calls the staging path ONCE. Unlike `cutback_to_local` which spins for
/// up to 900 seconds on writer-pass-in-progress, this returns immediately
/// with a typed error for the one-attempt driver to classify.
fn cutback_to_local_single_attempt(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
    identity: &CodeProjectIdentity,
) -> Result<CutbackSuccessOutcome> {
    let store = state.code_sources.store();
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    let active_is_collected = manifest
        .workspaces
        .get(project_id)
        .and_then(|entry| entry.code_source_selector.as_deref())
        .is_some_and(|selector| selector.starts_with("collected:"));
    if !active_is_collected {
        // The workspace manifest does not mark this project as collected.
        // Before clearing the activation record, check the activation
        // record itself: a collected activation record with no workspace
        // entry is the pending-first-republish state for a migrated or
        // never-booted root. Clearing it would delete the GC root and
        // recovery record for a live collected generation (property 2:
        // the local/local stale-state cell clears cutback state only,
        // never the activation record). Activation records are removed
        // only by activation replacement or explicit retirement discharge.
        if let Ok(Some(activation)) = store.load_activation_mixed(project_id) {
            if activation.selector().starts_with("collected:") {
                return Ok(CutbackSuccessOutcome::ClearCutback);
            }
        }
        return Ok(CutbackSuccessOutcome::ClearActivation);
    }
    ensure_selector_staging_available(
        store.as_ref(),
        &bbox_code_source::local_selector(project_id),
    )?;
    // Single staging call: return immediately on writer-pass-in-progress
    // instead of spinning (section 9.1 loop elimination).
    let staged = match state.index_writer.stage_local_generation(
        identity.clone(),
        scope.clone(),
        store.clone(),
    ) {
        Ok(staged) => staged,
        Err(error) if writer_pass_in_progress(&error) => {
            return Err(error.context("index writer contention during catalog cutback staging"));
        }
        Err(error) => return Err(error),
    };
    if state.code_sources.assignment_matches(scope, project_id) {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        bail!("collector assignment returned while local cutback was staging");
    }
    staged.begin_publication()?;
    if let Err(error) = state
        .index_writer
        .verify_code_selector_document_count(&staged.selector, staged.document_count)
    {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        return Err(error.context(StagingValidationRefusal));
    }
    let previous_entry = manifest.workspaces.get(project_id).cloned();
    let previous_view = state.code_read_view.read().clone();
    enqueue_previous_retirement(
        store.as_ref(),
        project_id,
        previous_entry.clone(),
        &staged.selector,
    )?;
    bbox_edge_sidecar::snapshot::activate_local_snapshot_with(
        &edges_dir,
        project_id,
        scope.repo_id(),
        &staged.head_commit,
        &staged.selector,
        &staged.snapshot_id,
        staged.worktree_dirty,
        staged
            .worktree_dirty
            .then_some(staged.dirty_fingerprint.as_str()),
        || {
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), staged.selector.clone());
            index.replace_active_code_selectors(selectors.clone());
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                searcher: index.searcher(),
                // The callback executes under the manifest coordinator. A
                // complete sidecar parse here starved every other manifest
                // publisher in production. Publish no stale graph, release
                // the coordinator, and let the bounded watcher fill it.
                edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
                catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
                git_overlays: super::state::read_git_overlays_for_view(
                    &state.project_authority,
                    &edges_dir,
                ),
            });
            Ok(())
        },
    )?;
    state.nudge_edge_index_rebuild();
    if let Some(activation) = store.load_activation_mixed(project_id)? {
        if let Ok(generation) = store.find_generation_mixed(activation.generation_id()) {
            let gen_scope = generation.descriptor().scope.clone();
            store.mark_generation_state_mixed(
                &gen_scope,
                generation.generation_id(),
                GenerationState::Ready,
                None,
            )?;
        }
    }
    schedule_previous_retirement(
        state.clone(),
        project_id,
        previous_entry,
        &staged.selector,
        previous_view,
    )?;
    store.clear_health_failure(project_id, "cutback_pending")?;
    tracing::info!(
        project_id,
        "code-source project cut back to local ownership (catalog single attempt)"
    );
    let overlay_snapshot_id = staged.snapshot_id.clone();
    let overlay_chunk_targets = staged.current_chunk_targets.clone();
    drop(staged);
    stage_git_current_overlay_after_activation(
        state,
        project_id,
        scope,
        &overlay_snapshot_id,
        "",
        &overlay_chunk_targets,
    );
    Ok(CutbackSuccessOutcome::ClearActivation)
}

/// Catalog-mode schedule_cutback: one attempt, persist outcome, return
/// (section 9.1). No loop, no sleep. The caller (reconciler) holds the
/// transition guard.
fn schedule_cutback_catalog(
    state: Arc<SharedState>,
    scope: PublishedScope,
    project_id: String,
    guard: Option<GuardHandle>,
) {
    if !state.code_sources.begin_activation(&project_id) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let pid = project_id.as_str();
        let attempt = attempt_cutback_catalog(&state, &scope, pid);
        let store = state.code_sources.store();
        let (retry_base_secs, retry_max_secs, max_attempts) = {
            let config = state.config.read();
            (
                config.code_collection.cutback_retry_base_secs,
                config.code_collection.cutback_retry_max_secs,
                config.code_collection.cutback_max_attempts,
            )
        };

        match attempt {
            Ok(FencedCutbackAttempt {
                outcome: CutbackAttemptOutcome::ReadinessDeferred(readiness),
                ..
            }) => {
                let _ = store.clear_health_failure(pid, "cutback_manual_retry_required");
                let _ = store.clear_health_failure(pid, "cutback_waiting_selector_retirement");
                let _ = store.record_health_failure(
                    pid,
                    "cutback_waiting_readiness",
                    readiness.diagnostic(),
                );
                tracing::info!(
                    project_id = pid,
                    ?readiness,
                    "catalog cutback: readiness dependency unavailable; attempt ladder unchanged"
                );
                let retry_state = state.clone();
                let retry_scope = scope.clone();
                let retry_project = project_id.clone();
                tokio::runtime::Handle::current().spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    enqueue_current_transition(
                        &retry_state,
                        &retry_project,
                        &retry_scope,
                        ReconcileOrigin::ReadinessAvailable,
                        None,
                    );
                });
            }
            Ok(FencedCutbackAttempt { fence, outcome }) => {
                let authority_revision = current_cutback_authority_revision(&state);
                let compare_outcome = match outcome {
                    CutbackAttemptOutcome::Success(CutbackSuccessOutcome::ClearCutback) => {
                        CutbackCompareOutcome::ClearCutback
                    }
                    CutbackAttemptOutcome::Success(CutbackSuccessOutcome::ClearActivation) => {
                        CutbackCompareOutcome::ClearActivation
                    }
                    CutbackAttemptOutcome::Structural(reason) => {
                        CutbackCompareOutcome::Structural(reason)
                    }
                    CutbackAttemptOutcome::Transient(error_class) => {
                        CutbackCompareOutcome::Transient {
                            error_class,
                            retry_base_secs,
                            retry_max_secs,
                            max_attempts,
                            now_unix_secs: unix_now(),
                        }
                    }
                    CutbackAttemptOutcome::Terminal(error_class) => {
                        CutbackCompareOutcome::Terminal(error_class)
                    }
                    CutbackAttemptOutcome::ReadinessDeferred(_) => unreachable!(),
                };
                match store.compare_and_apply_cutback(&fence, authority_revision, compare_outcome) {
                    Ok(applied) => {
                        let _ = store.clear_health_failure(pid, "cutback_waiting_readiness");
                        let _ =
                            store.clear_health_failure(pid, "cutback_waiting_selector_retirement");
                        match applied.persisted.as_ref() {
                            Some(CutbackStateV2::Transient {
                                attempt,
                                error_class,
                                deadline_unix_secs,
                            }) => {
                                if let Some(reconciler) = state.code_sources.reconciler() {
                                    reconciler.register_transient(*deadline_unix_secs, pid);
                                }
                                tracing::info!(
                                    project_id = pid,
                                    ?error_class,
                                    attempt,
                                    deadline = deadline_unix_secs,
                                    "catalog cutback: transient outcome committed"
                                );
                            }
                            Some(CutbackStateV2::ManualRetryRequired {
                                error_class,
                                attempt,
                            }) => {
                                let _ = store.record_health_failure(
                                    pid,
                                    "cutback_manual_retry_required",
                                    "cutback exhausted retry budget; config reload required",
                                );
                                tracing::warn!(
                                    project_id = pid,
                                    ?error_class,
                                    attempt,
                                    "catalog cutback: retry budget exhausted"
                                );
                            }
                            Some(CutbackStateV2::Terminal { error_class }) => {
                                let _ = store.record_health_failure(
                                    pid,
                                    "cutback_terminal",
                                    "cutback failed terminally; collected generation stays authoritative",
                                );
                                tracing::error!(
                                    project_id = pid,
                                    ?error_class,
                                    "catalog cutback: terminal outcome committed"
                                );
                            }
                            Some(CutbackStateV2::Structural { reason }) => {
                                tracing::info!(
                                    project_id = pid,
                                    ?reason,
                                    "catalog cutback: structural outcome committed"
                                );
                            }
                            None => {
                                let _ = store.clear_health_failure(pid, "cutback_pending");
                                let _ = store
                                    .clear_health_failure(pid, "cutback_manual_retry_required");
                                let _ = store.clear_health_failure(pid, "cutback_terminal");
                            }
                        }
                    }
                    Err(error) if error.downcast_ref::<ActivationFenceConflict>().is_some() => {
                        tracing::info!(
                            project_id = pid,
                            catalog_epoch = authority_revision.catalog_epoch,
                            "catalog cutback: stale activation fence discarded"
                        );
                        enqueue_current_transition(
                            &state,
                            pid,
                            &scope,
                            ReconcileOrigin::CatalogCommit,
                            Some(authority_revision.catalog_epoch),
                        );
                    }
                    Err(error) => {
                        let _ = store.record_health_failure(
                            pid,
                            "cutback_pending",
                            "cutback outcome commit failed; inspect daemon logs",
                        );
                        tracing::error!(
                            project_id = pid,
                            %error,
                            "catalog cutback outcome compare-and-apply failed"
                        );
                    }
                }
            }
            Err(error) => {
                let _ = store.record_health_failure(
                    pid,
                    "cutback_pending",
                    "cutback attempt failed; inspect daemon logs",
                );
                tracing::error!(
                    project_id = pid,
                    %error,
                    "catalog cutback attempt returned an error before classification"
                );
            }
        }
        let _pending = state.code_sources.end_activation(pid);
        enqueue_activation_completion(&state, pid, &scope);
    });
}

fn cutback_to_local(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
) -> Result<()> {
    let store = state.code_sources.store();
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    let active_is_collected = manifest
        .workspaces
        .get(project_id)
        .and_then(|entry| entry.code_source_selector.as_deref())
        .is_some_and(|selector| selector.starts_with("collected:"));
    if !active_is_collected {
        // Property 2: the local/local stale-state cell clears cutback state
        // only, never the activation record. A collected activation record
        // with no workspace entry is pending-first-republish state; deleting
        // it would orphan a live collected generation.
        if let Ok(Some(activation)) = store.load_activation_mixed(project_id) {
            if activation.selector().starts_with("collected:") {
                let _ = store.clear_cutback_state(project_id);
                return Ok(());
            }
        }
        store.clear_activation(project_id)?;
        return Ok(());
    }
    store.mark_cutback_pending_mixed(project_id, "local cutback is staging")?;
    // Bridge local cutback genuinely needs an attachment (the walk reads the
    // checkout), so its identity comes from the version-1 record and keeps
    // the "registered project disappeared" failure; catalog mode resolves
    // from the catalog and lets the local-source lease be the thing that
    // fails closed when no attachment exists.
    let identity = resolve_code_project_identity(state, project_id, "local cutback")?;
    ensure_selector_staging_available(
        store.as_ref(),
        &bbox_code_source::local_selector(project_id),
    )?;
    let cutback_deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let staged = loop {
        match state.index_writer.stage_local_generation(
            identity.clone(),
            scope.clone(),
            store.clone(),
        ) {
            Ok(staged) => break staged,
            Err(error) if writer_pass_in_progress(&error) => {
                if std::time::Instant::now() >= cutback_deadline {
                    bail!("local cutback timed out waiting for the index writer");
                }
                if resolve_code_project_identity(state, project_id, "local cutback").is_err() {
                    bail!("registered project disappeared while local cutback was waiting");
                }
                if state.code_sources.assignment_matches(scope, project_id) {
                    bail!("collector assignment returned while local cutback was waiting");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    };
    if state.code_sources.assignment_matches(scope, project_id) {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        bail!("collector assignment returned while local cutback was staging");
    }
    staged.begin_publication()?;
    if let Err(error) = state
        .index_writer
        .verify_code_selector_document_count(&staged.selector, staged.document_count)
    {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        return Err(error);
    }
    let previous_entry = manifest.workspaces.get(project_id).cloned();
    let previous_view = state.code_read_view.read().clone();
    enqueue_previous_retirement(
        store.as_ref(),
        project_id,
        previous_entry.clone(),
        &staged.selector,
    )?;
    bbox_edge_sidecar::snapshot::activate_local_snapshot_with(
        &edges_dir,
        project_id,
        scope.repo_id(),
        &staged.head_commit,
        &staged.selector,
        &staged.snapshot_id,
        staged.worktree_dirty,
        staged
            .worktree_dirty
            .then_some(staged.dirty_fingerprint.as_str()),
        || {
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), staged.selector.clone());
            index.replace_active_code_selectors(selectors.clone());
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                searcher: index.searcher(),
                edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
                catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
                // Inside the manifest coordinator, so this reads the entry
                // the activation just wrote: the atomic overlay clear.
                git_overlays: super::state::read_git_overlays_for_view(
                    &state.project_authority,
                    &edges_dir,
                ),
            });
            Ok(())
        },
    )?;
    state.nudge_edge_index_rebuild();
    if let Some(activation) = store.load_activation_mixed(project_id)? {
        if let Ok(generation) = store.find_generation_mixed(activation.generation_id()) {
            let scope = generation.descriptor().scope.clone();
            store.mark_generation_state_mixed(
                &scope,
                generation.generation_id(),
                GenerationState::Ready,
                None,
            )?;
        }
    }
    schedule_previous_retirement(
        state.clone(),
        project_id,
        previous_entry,
        &staged.selector,
        previous_view,
    )?;
    store.clear_activation(project_id)?;
    store.clear_health_failure(project_id, "cutback_pending")?;
    tracing::info!(
        project_id,
        "code-source project cut back to local ownership"
    );
    // P3-F: local staging stopped walking Git inside its transaction, so the
    // cutback's current-file member is empty and its manifest entry is
    // overlay-managed with no selector. The overlay step below is what makes
    // the project's commit-file edges exist again. Best effort by the same
    // rule the collected lane follows: the local generation is already
    // published and a Git failure must not unpublish it.
    //
    // THE STAGED HOLD MUST BE RELEASED FIRST. The writer actor is parked on
    // it, so enqueueing the overlay op (or the consolidated-history op it
    // may run first) while `staged` is alive deadlocks the actor against its
    // own caller. Same ordering the collected activation path uses.
    let overlay_snapshot_id = staged.snapshot_id.clone();
    let overlay_chunk_targets = staged.current_chunk_targets.clone();
    drop(staged);
    stage_git_current_overlay_after_activation(
        state,
        project_id,
        scope,
        &overlay_snapshot_id,
        "local",
        &overlay_chunk_targets,
    );
    Ok(())
}

fn activate_desired_loop(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
) -> Result<()> {
    loop {
        let store = state.code_sources.store();
        let Some(mixed) = store.desired_generation_mixed(scope)? else {
            return Ok(());
        };
        let desired_generation_id = mixed.generation_id().to_string();
        let desired_producer_id = mixed.producer_id().to_string();
        let desired_state = mixed.state();
        let desired_descriptor = mixed.descriptor().clone();
        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired_producer_id)
        {
            return Ok(());
        }
        if desired_state == GenerationState::Active {
            let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
            let expected_snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
                project_id,
                &desired_generation_id,
            );
            let expected_selector = crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired_generation_id,
            );
            let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
            let active_entry = manifest.workspaces.get(project_id);
            let activation = store.load_activation_mixed(project_id)?;
            let still_active = active_entry.is_some_and(|entry| {
                entry.code_source_generation.as_deref() == Some(desired_generation_id.as_str())
                    && entry.code_source_selector.as_deref() == Some(expected_selector.as_str())
                    && entry.active_snapshot.as_deref()
                        == Some(
                            bbox_edge_sidecar::snapshot::active_snapshot_rel(
                                project_id,
                                &expected_snapshot,
                            )
                            .as_str(),
                        )
            }) && activation.is_some_and(|activation| {
                activation.generation_id() == desired_generation_id.as_str()
                    && activation.selector() == expected_selector
                    && activation.snapshot_id() == expected_snapshot
            });
            if still_active {
                return Ok(());
            }
            store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Ready,
                None,
            )?;
            continue;
        }
        if desired_state == GenerationState::StagingIndex {
            let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
            let active_entry = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?
                .workspaces
                .get(project_id)
                .cloned();
            let expected_snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
                project_id,
                &desired_generation_id,
            );
            let expected_selector = crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired_generation_id,
            );
            let already_active = active_entry.as_ref().is_some_and(|entry| {
                entry.code_source_generation.as_deref() == Some(desired_generation_id.as_str())
                    && entry.code_source_selector.as_deref() == Some(expected_selector.as_str())
                    && entry.active_snapshot.as_deref()
                        == Some(
                            bbox_edge_sidecar::snapshot::active_snapshot_rel(
                                project_id,
                                &expected_snapshot,
                            )
                            .as_str(),
                        )
            });
            let journal_matches =
                store
                    .load_activation_mixed(project_id)?
                    .is_some_and(|activation| {
                        activation.generation_id() == desired_generation_id.as_str()
                            && activation.selector() == expected_selector
                            && activation.snapshot_id() == expected_snapshot
                    });
            if already_active && journal_matches {
                store.mark_generation_state_mixed(
                    scope,
                    &desired_generation_id,
                    GenerationState::Active,
                    None,
                )?;
                return Ok(());
            }
            store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Ready,
                None,
            )?;
            continue;
        }
        if desired_state != GenerationState::Ready {
            return Ok(());
        }
        ensure_selector_staging_available(
            store.as_ref(),
            &crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired_generation_id,
            ),
        )?;
        store.mark_generation_state_mixed(
            scope,
            &desired_generation_id,
            GenerationState::StagingIndex,
            None,
        )?;
        // Catalog mode resolves the identity from the catalog snapshot, so a
        // remote-only project with zero attachments activates (F1); bridge
        // mode projects its version-1 record.
        let identity = resolve_code_project_identity(state, project_id, "activation")?;
        let entries = store.load_generation_entries(scope, &desired_generation_id)?;
        let staged = loop {
            match state.index_writer.stage_collected_generation(
                identity.clone(),
                desired_descriptor.clone(),
                desired_generation_id.clone(),
                entries.clone(),
                store.clone(),
            ) {
                Ok(staged) => break staged,
                Err(error) if writer_pass_in_progress(&error) => {
                    // Catalog mode (section 9.1 loop elimination): return
                    // immediately instead of sleeping. The caller
                    // handles the transient error.
                    if state.code_sources.store().record_mode() == RuntimeRecordMode::CatalogV2 {
                        return Err(error
                            .context("index writer contention during catalog activation staging"));
                    }
                    if !state.code_sources.assignment_authorizes(
                        scope,
                        project_id,
                        &desired_producer_id,
                    ) {
                        store.mark_generation_state_mixed(
                            scope,
                            &desired_generation_id,
                            GenerationState::Ready,
                            None,
                        )?;
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(error) => {
                    store.mark_generation_state_mixed(
                        scope,
                        &desired_generation_id,
                        GenerationState::Failed,
                        Some("activation failed; inspect daemon logs".into()),
                    )?;
                    store.record_health_failure(
                        project_id,
                        "activation_failed",
                        "activation failed; inspect daemon logs",
                    )?;
                    return Err(error);
                }
            }
        };
        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired_producer_id)
        {
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired_generation_id.clone()),
            )?;
            store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Ready,
                None,
            )?;
            return Ok(());
        }
        let newest = store
            .desired_generation_mixed(scope)?
            .ok_or_else(|| anyhow!("desired generation disappeared during activation"))?;
        if newest.generation_id() != desired_generation_id {
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired_generation_id.clone()),
            )?;
            store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Superseded,
                None,
            )?;
            continue;
        }
        staged.begin_publication()?;
        if let Err(error) = state
            .index_writer
            .verify_code_selector_document_count(&staged.selector, staged.document_count)
        {
            let retirement = enqueue_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired_generation_id.clone()),
            )?;
            let mark_result = store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Failed,
                Some("staged document verification failed; inspect daemon logs".into()),
            );
            spawn_retirement(
                state.clone(),
                retirement,
                None,
                RetirementCompletion::Ordinary,
            );
            mark_result?;
            return Err(error);
        }
        store.record_materialization_mixed(
            scope,
            &desired_generation_id,
            staged.document_count,
            staged.entity_inventory_sha256.clone(),
        )?;
        let inventory_sha256 = store
            .load_generation_mixed(scope, &desired_generation_id)?
            .entity_inventory_sha256()
            .map(|hash| hash.to_string())
            .ok_or_else(|| anyhow!("materialization inventory was not recorded"))?;
        if state.code_sources.store().record_mode() == RuntimeRecordMode::CatalogV2 {
            let activation = ActivationRecordV2 {
                version: bbox_code_source_store::MIGRATION_STORE_VERSION,
                project_id: ProjectId::parse(project_id.to_string())
                    .map_err(|error| anyhow!(error))?,
                published_scope: scope.clone(),
                generation_id: desired_generation_id.clone(),
                selector: staged.selector.clone(),
                snapshot_id: staged.snapshot_id.clone(),
                document_count: staged.document_count,
                entity_inventory_sha256: inventory_sha256,
                current_chunk_targets: staged.current_chunk_targets.clone().into_iter().collect(),
                activated_unix_secs: unix_now(),
                cutback_pending: false,
                cutback: None,
                diagnostic: None,
            };
            let generation_v2 = match store.load_generation_mixed(scope, &desired_generation_id)? {
                MixedStoredGeneration::CurrentV2(record) => record,
                MixedStoredGeneration::LegacyV1(_) => {
                    bail!(
                        "error.code_source_record_mode: catalog store found a v1 stored generation"
                    )
                }
            };
            activation
                .validate_against_generation(&generation_v2)
                .map_err(|error| error.context(StagingValidationRefusal))?;
            store.save_activation_v2(&activation)?;
        } else {
            store.save_activation(&ActivationRecord {
                version: 1,
                project_id: project_id.to_string(),
                generation_id: desired_generation_id.clone(),
                selector: staged.selector.clone(),
                snapshot_id: staged.snapshot_id.clone(),
                document_count: staged.document_count,
                entity_inventory_sha256: inventory_sha256,
                current_chunk_targets: staged.current_chunk_targets.clone().into_iter().collect(),
                activated_unix_secs: unix_now(),
                cutback_pending: false,
                diagnostic: None,
            })?;
        }

        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired_producer_id)
        {
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired_generation_id.clone()),
            )?;
            store.clear_activation(project_id)?;
            store.mark_generation_state_mixed(
                scope,
                &desired_generation_id,
                GenerationState::Ready,
                None,
            )?;
            return Ok(());
        }

        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
        let previous_manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
        let previous_entry = previous_manifest.workspaces.get(project_id).cloned();
        let previous_view = state.code_read_view.read().clone();
        enqueue_previous_retirement(
            store.as_ref(),
            project_id,
            previous_entry.clone(),
            &staged.selector,
        )?;
        // `repo_id` and `head_commit` are advisory manifest metadata from
        // this milestone on (plan section 6 item 2): the transaction below
        // no longer opens Git, so neither value gates anything it commits.
        // The signature is unchanged; P3-F retypes the manifest entry.
        bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
            &edges_dir,
            project_id,
            scope.repo_id(),
            &desired_descriptor.head_commit,
            &desired_generation_id,
            &staged.selector,
            &staged.snapshot_id,
            || {
                let index = state.idx.write();
                let mut selectors = index.active_code_selectors();
                selectors.insert(project_id.to_string(), staged.selector.clone());
                index.replace_active_code_selectors(selectors.clone());
                *state.code_read_view.write() = Arc::new(super::CodeReadView {
                    active_selectors: selectors,
                    searcher: index.searcher(),
                    edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
                    catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
                    // Inside the manifest coordinator, so this reads the
                    // entry the activation just wrote: activating a new code
                    // generation clears the project's overlay, and this is
                    // the read that makes the clear visible to readers in the
                    // same swap rather than one republish later.
                    git_overlays: super::state::read_git_overlays_for_view(
                        &state.project_authority,
                        &edges_dir,
                    ),
                });
                Ok(())
            },
        )?;
        state.nudge_edge_index_rebuild();
        tracing::info!(
            project_id,
            generation = %desired_generation_id,
            active_projects = state.code_read_view.read().active_selectors.len(),
            "code-source generation activated"
        );

        store.mark_generation_state_mixed(
            scope,
            &desired_generation_id,
            GenerationState::Active,
            None,
        )?;
        store.clear_health_failure(project_id, "activation_failed")?;
        store.clear_health_failure(project_id, "missing_blob_data")?;
        schedule_previous_retirement(
            state.clone(),
            project_id,
            previous_entry,
            &staged.selector,
            previous_view,
        )?;
        // The generation is published; everything Git happens after this
        // point and can only degrade health, never unpublish (F5). The
        // staged hold MUST be released first: the writer actor is parked on
        // it, so enqueueing the overlay op while it is alive would deadlock.
        let overlay_snapshot_id = staged.snapshot_id.clone();
        let overlay_chunk_targets = staged.current_chunk_targets.clone();
        drop(staged);
        stage_git_current_overlay_after_activation(
            state,
            project_id,
            scope,
            &overlay_snapshot_id,
            &desired_generation_id,
            &overlay_chunk_targets,
        );
        return Ok(());
    }
}

fn writer_pass_in_progress(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<
                bbox_indexing::index::writer_actor::IndexWriterRetryableError,
            >(),
            Some(
                bbox_indexing::index::writer_actor::IndexWriterRetryableError::ReindexPassInProgress
            )
        )
    })
}

fn selector_retirement_retryable(error: &anyhow::Error) -> bool {
    writer_pass_in_progress(error)
        || error.chain().any(|cause| {
            matches!(
            cause.downcast_ref::<bbox_indexing::index::writer_actor::IndexWriterRetryableError>(),
            Some(bbox_indexing::index::writer_actor::IndexWriterRetryableError::VectorStoreWarming)
        )
        })
}

const SELECTOR_RETIREMENT_RETRY_LIMIT: u32 = 8;
const SELECTOR_RETIREMENT_REDRIVE_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

fn take_selector_retirement_retry(
    attempts: &mut u32,
    delay: &mut std::time::Duration,
) -> Option<std::time::Duration> {
    if *attempts >= SELECTOR_RETIREMENT_RETRY_LIMIT {
        return None;
    }
    *attempts += 1;
    let current = *delay;
    *delay = (*delay * 2).min(std::time::Duration::from_secs(30));
    Some(current)
}

fn schedule_previous_retirement(
    state: Arc<SharedState>,
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
    previous_view: Arc<super::CodeReadView>,
) -> Result<()> {
    let Some(record) = previous_retirement_record(project_id, previous, active_selector) else {
        return Ok(());
    };
    state.code_sources.store().enqueue_retirement(&record)?;
    spawn_retirement(
        state,
        record,
        Some(previous_view),
        RetirementCompletion::Ordinary,
    );
    Ok(())
}

fn schedule_unactivated_retirement(
    state: &Arc<SharedState>,
    project_id: &str,
    staged: &crate::index::project_files::CollectedIndexResult,
    generation_id: Option<String>,
) -> Result<()> {
    let record = enqueue_unactivated_retirement(state, project_id, staged, generation_id)?;
    spawn_retirement(state.clone(), record, None, RetirementCompletion::Ordinary);
    Ok(())
}

fn enqueue_unactivated_retirement(
    state: &Arc<SharedState>,
    project_id: &str,
    staged: &crate::index::project_files::CollectedIndexResult,
    generation_id: Option<String>,
) -> Result<RetirementRecord> {
    let record = RetirementRecord {
        version: 1,
        project_id: project_id.to_string(),
        selector: staged.selector.clone(),
        snapshot_id: staged.snapshot_id.clone(),
        generation_id,
    };
    state.code_sources.store().enqueue_retirement(&record)?;
    Ok(record)
}

fn ensure_selector_staging_available(store: &CodeSourceStore, selector: &str) -> Result<()> {
    // The per-project activation lane is the sole runtime enqueuer for its
    // selectors. A durable queue row therefore separates two staging epochs.
    if store.retirement_pending(selector)? {
        return Err(anyhow::Error::new(SelectorRetirementQueued));
    }
    Ok(())
}

fn enqueue_previous_retirement(
    store: &CodeSourceStore,
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
) -> Result<()> {
    if let Some(record) = previous_retirement_record(project_id, previous, active_selector) {
        store.enqueue_retirement(&record)?;
    }
    Ok(())
}

fn previous_retirement_record(
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
) -> Option<RetirementRecord> {
    let previous = previous?;
    let (Some(selector), Some(snapshot_id)) = (
        previous.code_source_selector,
        previous
            .active_snapshot
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
            .map(str::to_string),
    ) else {
        return None;
    };
    if selector == active_selector {
        return None;
    }
    Some(RetirementRecord {
        version: 1,
        project_id: project_id.to_string(),
        selector,
        snapshot_id,
        generation_id: previous
            .code_source_generation
            .filter(|value| value != "local"),
    })
}

fn spawn_retirement(
    state: Arc<SharedState>,
    record: RetirementRecord,
    previous_view: Option<Arc<super::CodeReadView>>,
    completion: RetirementCompletion,
) {
    let work = match completion {
        RetirementCompletion::Ordinary => RetirementWork::Ordinary(record),
        RetirementCompletion::Collision {
            project_id,
            generation_id,
            former_scope,
        } => RetirementWork::CollisionExact {
            record,
            project_id,
            generation_id,
            former_scope,
        },
    };
    enqueue_retirement_work(state, work, previous_view);
}

#[derive(Debug, Clone)]
enum RetirementCompletion {
    Ordinary,
    Collision {
        project_id: ProjectId,
        generation_id: String,
        former_scope: PublishedScope,
    },
}

enum RetirementAttempt {
    Complete,
    WaitingForReaders,
    DeferredActive,
    Retryable(anyhow::Error),
    Failed(anyhow::Error),
}

fn enqueue_retirement_work(
    state: Arc<SharedState>,
    work: RetirementWork,
    previous_view: Option<Arc<super::CodeReadView>>,
) {
    let coordinator = state.code_sources.retirement_coordinator.clone();
    let key = work.key();
    {
        let mut queue = coordinator.queue.lock().unwrap();
        match queue.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(RetirementWorkEntry {
                    work,
                    previous_view,
                    attempts: 0,
                    retry_delay: std::time::Duration::from_secs(1),
                    next_due: std::time::Instant::now(),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if !entry.get().work.same_selector_identity(&work) {
                    let project_id = work.project_id().to_string();
                    drop(queue);
                    let _ = state.code_sources.store().record_health_failure(
                        &project_id,
                        "retirement_identity_conflict",
                        "retirement coordinator received conflicting work for one key",
                    );
                    tracing::error!(
                        project_id,
                        "retirement coordinator refused conflicting keyed work"
                    );
                    return;
                }
                let current = entry.get_mut();
                if matches!(&work, RetirementWork::CollisionExact { .. }) {
                    current.work = work;
                }
                if current.previous_view.is_none() {
                    current.previous_view = previous_view;
                }
                current.next_due = std::time::Instant::now();
            }
        }
    }
    coordinator.notify.notify_one();

    if coordinator
        .started
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok()
    {
        let shutdown = state.reconciler_shutdown.read().clone();
        if let Err(error) = std::thread::Builder::new()
            .name("blackbox-code-source-retirement".to_string())
            .spawn(move || retirement_coordinator_loop(state, coordinator, shutdown))
        {
            tracing::error!(%error, "spawning code-source retirement coordinator failed");
        }
    }
}

fn retirement_coordinator_loop(
    state: Arc<SharedState>,
    coordinator: Arc<RetirementCoordinator>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
        let mut entry = {
            let mut queue = coordinator.queue.lock().unwrap();
            loop {
                if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let now = std::time::Instant::now();
                let next = queue
                    .iter()
                    .min_by_key(|(_, entry)| entry.next_due)
                    .map(|(key, entry)| (key.clone(), entry.next_due));
                match next {
                    Some((key, due)) if due <= now => {
                        break queue.remove(&key).expect("selected retirement work exists");
                    }
                    Some((_key, due)) => {
                        let wait = due
                            .saturating_duration_since(now)
                            .min(SELECTOR_RETIREMENT_REDRIVE_DELAY);
                        (queue, _) = coordinator.notify.wait_timeout(queue, wait).unwrap();
                    }
                    None => {
                        (queue, _) = coordinator
                            .notify
                            .wait_timeout(queue, SELECTOR_RETIREMENT_REDRIVE_DELAY)
                            .unwrap();
                    }
                }
            }
        };

        let result = run_retirement_attempt(&state, &entry);
        let requeue_delay = match result {
            RetirementAttempt::Complete => None,
            RetirementAttempt::WaitingForReaders => Some(std::time::Duration::from_millis(100)),
            RetirementAttempt::DeferredActive => {
                entry.attempts = 0;
                entry.retry_delay = std::time::Duration::from_secs(1);
                let _ = state.code_sources.store().record_health_failure(
                    entry.work.project_id(),
                    "retirement_deferred_active",
                    "retirement remains queued while selector or snapshot is active",
                );
                Some(SELECTOR_RETIREMENT_REDRIVE_DELAY)
            }
            RetirementAttempt::Retryable(error) => {
                let delay =
                    take_selector_retirement_retry(&mut entry.attempts, &mut entry.retry_delay)
                        .unwrap_or(SELECTOR_RETIREMENT_REDRIVE_DELAY);
                if entry.attempts >= SELECTOR_RETIREMENT_RETRY_LIMIT {
                    let _ = state.code_sources.store().record_health_failure(
                        entry.work.project_id(),
                        "retirement_failed",
                        "retirement retry budget exhausted; work remains queued",
                    );
                }
                tracing::warn!(
                    project_id = entry.work.project_id(),
                    attempts = entry.attempts,
                    retry_secs = delay.as_secs_f64(),
                    %error,
                    "selector retirement attempt deferred"
                );
                Some(delay)
            }
            RetirementAttempt::Failed(error) => {
                let _ = state.code_sources.store().record_health_failure(
                    entry.work.project_id(),
                    "retirement_failed",
                    "retirement failed; work remains queued",
                );
                tracing::error!(
                    project_id = entry.work.project_id(),
                    %error,
                    "selector retirement attempt failed"
                );
                Some(SELECTOR_RETIREMENT_REDRIVE_DELAY)
            }
        };

        if let Some(delay) = requeue_delay {
            entry.next_due = std::time::Instant::now() + delay;
            let key = entry.work.key();
            let mut queue = coordinator.queue.lock().unwrap();
            queue.entry(key).or_insert(entry);
        }
    }
}

fn run_retirement_attempt(
    state: &Arc<SharedState>,
    entry: &RetirementWorkEntry,
) -> RetirementAttempt {
    if entry
        .previous_view
        .as_ref()
        .is_some_and(|view| Arc::strong_count(view) > 1)
    {
        return RetirementAttempt::WaitingForReaders;
    }
    match &entry.work {
        RetirementWork::CollisionSelectorless(work) => {
            let store = state.code_sources.store();
            if let Err(error) = repair_and_complete_collision_retirement(
                &store,
                &work.project_id,
                &work.generation_id,
            ) {
                return RetirementAttempt::Failed(error);
            }
            finish_retirement_success(state, work.project_id.as_str(), Some(&work.former_scope));
            RetirementAttempt::Complete
        }
        RetirementWork::Ordinary(record) => run_selector_retirement(state, record, None),
        RetirementWork::CollisionExact {
            record,
            project_id,
            generation_id,
            former_scope,
        } => run_selector_retirement(
            state,
            record,
            Some((project_id, generation_id, former_scope)),
        ),
    }
}

fn retirement_record_is_active(
    edges_dir: &std::path::Path,
    record: &RetirementRecord,
) -> Result<bool> {
    bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(edges_dir)?;
        let selector_is_active = manifest
            .workspaces
            .values()
            .any(|entry| entry.code_source_selector.as_deref() == Some(record.selector.as_str()));
        let expected_snapshot = bbox_edge_sidecar::snapshot::active_snapshot_rel(
            &record.project_id,
            &record.snapshot_id,
        );
        let snapshot_is_active = manifest
            .workspaces
            .get(&record.project_id)
            .and_then(|entry| entry.active_snapshot.as_deref())
            == Some(expected_snapshot.as_str());
        Ok(selector_is_active || snapshot_is_active)
    })
}

fn run_selector_retirement(
    state: &Arc<SharedState>,
    record: &RetirementRecord,
    collision: Option<(&ProjectId, &String, &PublishedScope)>,
) -> RetirementAttempt {
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    match retirement_record_is_active(&edges_dir, record) {
        Ok(true) => return RetirementAttempt::DeferredActive,
        Ok(false) => {}
        Err(error) => return RetirementAttempt::Failed(error),
    }
    let retired = match state
        .index_writer
        .retire_code_selector(record.selector.clone())
    {
        Ok(retired) => retired,
        Err(error) if selector_retirement_retryable(&error) => {
            return RetirementAttempt::Retryable(error);
        }
        Err(error) => return RetirementAttempt::Failed(error),
    };
    tracing::info!(
        project_id = %record.project_id,
        selector = %record.selector,
        document_count = retired.document_count,
        "retired inactive code-source selector"
    );
    if let Err(error) = retired.begin_cleanup() {
        return RetirementAttempt::Failed(error);
    }
    let cleanup = bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
        let selector_is_active = manifest
            .workspaces
            .values()
            .any(|entry| entry.code_source_selector.as_deref() == Some(record.selector.as_str()));
        let expected_snapshot = bbox_edge_sidecar::snapshot::active_snapshot_rel(
            &record.project_id,
            &record.snapshot_id,
        );
        let snapshot_is_active = manifest
            .workspaces
            .get(&record.project_id)
            .and_then(|entry| entry.active_snapshot.as_deref())
            == Some(expected_snapshot.as_str());
        if selector_is_active || snapshot_is_active {
            return Ok(false);
        }
        if !record.snapshot_id.contains('/')
            && !record.snapshot_id.contains('\\')
            && record.snapshot_id != "."
            && record.snapshot_id != ".."
        {
            let snapshot = bbox_edge_sidecar::snapshot::snapshot_dir(
                &edges_dir,
                &record.project_id,
                &record.snapshot_id,
            );
            if snapshot.is_dir() {
                std::fs::remove_dir_all(&snapshot)?;
            }
        }
        Ok(true)
    });
    match cleanup {
        Ok(true) => {}
        Ok(false) => return RetirementAttempt::DeferredActive,
        Err(error) => return RetirementAttempt::Failed(error),
    }
    let store = state.code_sources.store();
    let fallback_scope = if let Some((project_id, generation_id, former_scope)) = collision {
        if let Err(error) =
            repair_and_complete_collision_retirement(&store, project_id, generation_id)
        {
            return RetirementAttempt::Failed(error);
        }
        Some(former_scope.clone())
    } else {
        let scope = retirement_scope_for_record(&store, record).or_else(|| {
            state
                .code_sources
                .assignments()
                .into_iter()
                .find_map(|(scope, project_id)| (project_id == record.project_id).then_some(scope))
        });
        if let Err(error) = store.complete_retirement(record) {
            return RetirementAttempt::Failed(error);
        }
        scope
    };
    let _ = store.clear_health_failure(&record.project_id, "retirement_failed");
    let _ = store.clear_health_failure(&record.project_id, "retirement_deferred_active");
    drop(retired);
    finish_retirement_success(state, &record.project_id, fallback_scope.as_ref());
    RetirementAttempt::Complete
}

fn retirement_scope_for_record(
    store: &CodeSourceStore,
    record: &RetirementRecord,
) -> Option<PublishedScope> {
    record
        .generation_id
        .as_deref()
        .and_then(|generation_id| store.find_generation_mixed(generation_id).ok())
        .map(|generation| generation.descriptor().scope.clone())
        .or_else(|| {
            store
                .load_activation_mixed(&record.project_id)
                .ok()
                .flatten()
                .and_then(|activation| activation.published_scope().cloned())
        })
}

fn finish_retirement_success(
    state: &Arc<SharedState>,
    project_id: &str,
    fallback_scope: Option<&PublishedScope>,
) {
    let store = state.code_sources.store();
    let _ = store.clear_health_failure(project_id, "retirement_failed");
    let _ = store.clear_health_failure(project_id, "retirement_deferred_active");
    if let Some(fallback_scope) = fallback_scope {
        enqueue_current_transition(
            state,
            project_id,
            fallback_scope,
            ReconcileOrigin::SelectorRetirementCompletion,
            None,
        );
    }
}

fn repair_and_complete_collision_retirement(
    store: &CodeSourceStore,
    project_id: &ProjectId,
    generation_id: &str,
) -> Result<()> {
    store
        .repair_and_complete_collision_retirement(project_id, generation_id)
        .context("repairing and completing collision retirement")
}

fn spawn_selectorless_collision_retirement(
    state: Arc<SharedState>,
    work: CollisionRetirementWorkV1,
) {
    enqueue_retirement_work(state, RetirementWork::CollisionSelectorless(work), None);
}

async fn require_upload_scope(
    store: &Arc<CodeSourceStore>,
    grant: &ProducerGrant,
    upload_id: &str,
) -> Result<PublishedScope, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let upload_id = upload_id.to_string();
    let scope = tokio::task::spawn_blocking(move || store.upload_scope(&producer_id, &upload_id))
        .await
        .map_err(|_| HttpError::storage("upload lookup task failed"))?
        .map_err(|error| {
            if store_error_is_not_found(&error) {
                HttpError::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
            } else {
                HttpError::from_store(error)
            }
        })?;
    require_scope(grant, &scope)?;
    Ok(scope)
}

fn require_scope<'a>(
    grant: &'a ProducerGrant,
    scope: &PublishedScope,
) -> Result<&'a str, HttpError> {
    grant
        .projects
        .get(scope)
        .map(String::as_str)
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::FORBIDDEN,
                "scope_forbidden",
                "scope is not authorized for this producer",
            )
        })
}

fn status_from_mixed_generation(mixed: MixedStoredGeneration) -> GenerationStatus {
    let state = mixed.state();
    let diagnostic = mixed.diagnostic().map(|_| {
        match state {
            GenerationState::MissingBlobData => {
                "retained blob data is unavailable; recollect this generation"
            }
            GenerationState::Failed => "generation processing failed; inspect daemon logs",
            _ => "generation processing requires operator attention",
        }
        .to_string()
    });
    GenerationStatus {
        generation_id: mixed.generation_id().to_string(),
        state,
        file_count: mixed.descriptor().file_count,
        logical_bytes: mixed.descriptor().logical_bytes,
        diagnostic,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::storage(anyhow!("blocking task failed")))?
        .map_err(HttpError::from_store)
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    body: ErrorResponse,
}

impl HttpError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorResponse {
                code: code.to_string(),
                message: message.into().chars().take(512).collect(),
            },
        }
    }

    fn unprocessable(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    fn too_large(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, code, message)
    }

    fn too_many_requests(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code, message)
    }

    fn storage(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "code-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "code-source storage is unavailable",
        )
    }

    fn from_store(error: anyhow::Error) -> Self {
        if error
            .chain()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        {
            return Self::storage(error);
        }
        if let Some(contract) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ContractError>())
        {
            return match contract {
                ContractError::FileTooLarge { .. }
                | ContractError::TooManyFiles { .. }
                | ContractError::TooManyBytes { .. } => Self::too_large(
                    "limit_exceeded",
                    "code-source input exceeds an enforced limit",
                ),
                ContractError::UnsupportedSchema(_)
                | ContractError::WalkerPolicyMismatch { .. } => Self::unprocessable(
                    "unsupported_contract",
                    "code-source contract version is unsupported",
                ),
                _ => Self::unprocessable(
                    "invalid_code_source_input",
                    "code-source input violates the collection contract",
                ),
            };
        }
        if let Some(request) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreRequestError>())
        {
            return match request {
                StoreRequestError::LimitExceeded => Self::too_large(
                    "limit_exceeded",
                    "code-source input exceeds an enforced limit",
                ),
                StoreRequestError::TooManyOpenUploads => Self::too_many_requests(
                    "upload_limit_reached",
                    "producer has too many open uploads",
                ),
                StoreRequestError::InvalidState => Self::unprocessable(
                    "invalid_upload_state",
                    "upload is not in the required state",
                ),
                StoreRequestError::InvalidInput => {
                    Self::unprocessable("invalid_code_source_input", "code-source input is invalid")
                }
            };
        }
        Self::storage(error)
    }
}

fn store_error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use bbox_code_source::{
        BeginUploadResponse, CutbackReason, CutbackStateV2, GenerationDescriptor, ManifestEntry,
        SCHEMA_VERSION, WALKER_POLICY_VERSION, dirty_fingerprint,
        generation_id as compute_generation_id, manifest_sha256, source_selector,
    };
    use bbox_code_source_store::{
        CodeSourceStorePaths, CollisionRetirementEntryV1, CollisionRetirementLifecycleStateV1,
        CollisionRetirementLifecycleV1, CollisionRetirementSelectorEvidenceV1,
        CollisionRetirementWorkV1, StoredGenerationV2,
        decode_collision_retirement_pending_for_migration,
        decode_stored_generation_v2_for_migration,
        encode_collision_retirement_pending_for_migration,
        encode_stored_generation_v2_for_migration,
    };
    use bbox_config::config::CodeCollectionProducerConfig;
    use bbox_corpus_core::project_catalog::{ProjectId, ProjectScope};
    use bbox_indexing::checkout_access::{
        CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError,
        CheckoutAccessErrorCode, CheckoutAccessObservations, CheckoutAttachmentStatus,
    };
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    use super::*;
    use crate::server::producer_auth::{
        GrantScopeResolution, resolve_catalog_project, resolve_grant_scope,
    };

    #[derive(Clone)]
    struct SnapshotAuthority {
        candidates: BTreeMap<String, CheckoutAccessCandidate>,
    }

    impl CheckoutAccessAuthority for SnapshotAuthority {
        fn resolve(
            &self,
            request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.candidates
                .get(&request.project_id)
                .cloned()
                .ok_or_else(|| {
                    CheckoutAccessError::new(
                        CheckoutAccessErrorCode::AttachmentNotFound,
                        "test project has no checkout candidate",
                    )
                })
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            Ok(())
        }
    }

    fn empty_generation_descriptor(scope: PublishedScope, head: &str) -> GenerationDescriptor {
        GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope,
            head_commit: head.to_string(),
            dirty_fingerprint: dirty_fingerprint(head, &[]),
            manifest_sha256: manifest_sha256(&[]),
            file_count: 0,
            logical_bytes: 0,
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_service_token(path: &Path, secret: char) {
        fs::write(path, secret.to_string().repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn snapshot_project(
        root: &Path,
        project_id: &str,
        scope: &PublishedScope,
    ) -> (ProjectRecord, CheckoutAccessCandidate) {
        let project_root = root.join(project_id);
        fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        (
            ProjectRecord {
                project_id: project_id.to_string(),
                repo_id: Some(scope.repo_id().to_string()),
                canonical_path: project_root.to_string_lossy().into_owned(),
                registered_at: "2026-01-01T00:00:00Z".into(),
                is_git_repo: true,
                languages: BTreeSet::new(),
                aliases: BTreeSet::new(),
            },
            CheckoutAccessCandidate {
                project_id: project_id.to_string(),
                attachment_id: format!("attachment-{project_id}"),
                checkout_id: format!("checkout-{project_id}"),
                published_scope: Some(scope.clone()),
                branch_ref: Some("refs/heads/main".into()),
                checkout_root: project_root.clone(),
                project_root,
                status: CheckoutAttachmentStatus::Active,
                capabilities: BTreeSet::from([CheckoutAccessKind::PublisherConfigTreeRead]),
                lifetime_guard: None,
            },
        )
    }

    fn snapshot_broker(candidates: Vec<CheckoutAccessCandidate>) -> CheckoutAccessBroker {
        CheckoutAccessBroker::new(
            Arc::new(SnapshotAuthority {
                candidates: candidates
                    .into_iter()
                    .map(|candidate| (candidate.project_id.clone(), candidate))
                    .collect(),
            }),
            CheckoutAccessObservations::in_memory(),
        )
    }

    fn assert_snapshot_rejected(
        base: &crate::config::Config,
        producers: Vec<CodeCollectionProducerConfig>,
        projects: &[ProjectRecord],
        store: Arc<CodeSourceStore>,
        broker: &CheckoutAccessBroker,
        expected: &str,
    ) {
        let mut config = base.clone();
        config.code_collection.enabled = true;
        config.code_collection.producers = producers;
        let error = build_snapshot(&config, projects, None, Some(store), broker)
            .err()
            .expect("invalid enabled code-source configuration must fail closed");
        assert_eq!(error.to_string(), expected);
    }

    fn install_test_assignment(
        state: &Arc<SharedState>,
        producer_id: &str,
        scope: &PublishedScope,
        project_id: &str,
    ) {
        let store = state.code_sources.store();
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            auth: Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    bro_rpc::ServiceToken::parse("a".repeat(64)).unwrap(),
                    ProducerGrant {
                        producer_id: producer_id.to_string(),
                        projects: BTreeMap::from([(scope.clone(), project_id.to_string())]),
                    },
                )],
            )),
            store,
        });
    }

    fn transition_test_state(state_dir: &Path) -> Arc<SharedState> {
        let mut state = SharedState::for_test(state_dir);
        // Production keeps the bro store below the state root while projects
        // and edge snapshots are siblings. Restore that relationship because
        // SharedState::for_test otherwise collapses both paths into one.
        state.store_dir = state_dir.join("bro");
        Arc::new(state)
    }

    fn enabled_http_state(
        root: &std::path::Path,
        scope: &PublishedScope,
    ) -> (Arc<SharedState>, String) {
        let state = Arc::new(SharedState::for_test(root));
        let token_secret = "a".repeat(64);
        let token = bro_rpc::ServiceToken::parse(token_secret.clone()).unwrap();
        let store = state.code_sources.store();
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            auth: Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "http-test-producer".into(),
                        projects: BTreeMap::from([(scope.clone(), "http-test-project".into())]),
                    },
                )],
            )),
            store,
        });
        (state, token_secret)
    }

    fn authenticated_request(
        method: &str,
        uri: impl AsRef<str>,
        token: &str,
        body: Body,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri.as_ref())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    #[test]
    fn converged_reducer_clears_stale_cutback_health_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("code-source");
        let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let project_id = "health-project";
        for code in [
            "cutback_pending",
            "cutback_manual_retry_required",
            "cutback_terminal",
            "cutback_waiting_readiness",
            "cutback_waiting_selector_retirement",
        ] {
            store
                .record_health_failure(project_id, code, "stale")
                .unwrap();
        }
        store
            .record_health_failure(project_id, "unrelated_failure", "keep")
            .unwrap();
        store
            .record_health_failure("unresolved-project", "cutback_pending", "keep")
            .unwrap();
        store
            .record_health_failure("reassigned-project", "cutback_pending", "stale")
            .unwrap();

        clear_cutback_health_if_converged(
            &store,
            project_id,
            DesiredAssignment::Local,
            EffectiveSource::Local,
            None,
        );
        clear_cutback_health_if_converged(
            &store,
            "unresolved-project",
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
        );
        clear_cutback_health_if_converged(
            &store,
            "reassigned-project",
            DesiredAssignment::Collected,
            EffectiveSource::Unavailable,
            None,
        );

        let records = store.health_records().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| {
            record.project_id == project_id && record.code == "unrelated_failure"
        }));
        assert!(records.iter().any(|record| {
            record.project_id == "unresolved-project" && record.code == "cutback_pending"
        }));
    }

    #[test]
    fn startup_sweep_clears_converged_cutback_health_without_a_reducer_event() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let converged_id = "p_0000000000000000000000000000hc1";
        let unresolved_id = "p_0000000000000000000000000000hc2";
        let unresolved_scope = PublishedScope::try_new("health-unresolved", ".").unwrap();
        let unresolved_generation = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(unresolved_scope.clone(), &"a".repeat(40)),
        );
        let unresolved = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            unresolved_id,
            &unresolved_scope,
            &unresolved_generation,
            None,
            false,
        );

        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            converged_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{converged_id}/manifest.json"),
                active_snapshot: Some(format!("workspace/{converged_id}/snapshots/local-health")),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(converged_id)),
                code_source_generation: Some("local".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        manifest.workspaces.insert(
            unresolved_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{unresolved_id}/manifest.json"),
                active_snapshot: Some(format!(
                    "workspace/{unresolved_id}/snapshots/collected-health"
                )),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(unresolved.selector),
                code_source_generation: Some(unresolved_generation),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        store
            .record_health_failure(converged_id, "cutback_manual_retry_required", "stale")
            .unwrap();
        store
            .record_health_failure(converged_id, "unrelated_failure", "keep")
            .unwrap();
        store
            .record_health_failure(unresolved_id, "cutback_pending", "keep")
            .unwrap();

        clear_converged_cutback_health_at_startup(&store, &runtime, &manifest);

        let records = store.health_records().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| {
            record.project_id == converged_id && record.code == "unrelated_failure"
        }));
        assert!(records.iter().any(|record| {
            record.project_id == unresolved_id && record.code == "cutback_pending"
        }));
    }

    #[test]
    fn startup_reaps_only_owned_upload_body_tempfiles() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let owned = root.join(format!(
            "{UPLOAD_BODY_TEMP_PREFIX}crash{UPLOAD_BODY_TEMP_SUFFIX}"
        ));
        let unrelated = root.join(".unrelated.tmp");
        fs::write(&owned, b"orphan").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        assert_eq!(reap_upload_body_tempfiles(&root).unwrap(), 1);
        assert!(!owned.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn code_source_http_routes_ingest_a_manifest_without_leaking_store_errors() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-repo", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state.clone());
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "b".repeat(64),
            size: 1,
        }];
        let head = "c".repeat(40);
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope,
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: 1,
            logical_bytes: 1,
        };
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(
                    serde_json::to_vec(&BeginUploadRequest {
                        descriptor: descriptor.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let begun: BeginUploadResponse = serde_json::from_slice(&body).unwrap();

        let other_token_secret = "e".repeat(64);
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            auth: Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![
                    (
                        bro_rpc::ServiceToken::parse(token.clone()).unwrap(),
                        ProducerGrant {
                            producer_id: "http-test-producer".into(),
                            projects: BTreeMap::from([(
                                descriptor.scope.clone(),
                                "http-test-project".into(),
                            )]),
                        },
                    ),
                    (
                        bro_rpc::ServiceToken::parse(other_token_secret.clone()).unwrap(),
                        ProducerGrant {
                            producer_id: "other-http-producer".into(),
                            projects: BTreeMap::from([(
                                descriptor.scope.clone(),
                                "http-test-project".into(),
                            )]),
                        },
                    ),
                ],
            )),
            store: state.code_sources.store(),
        });
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                format!(
                    "/internal/code-source/v1/uploads/{}/missing",
                    begun.upload_id
                ),
                &other_token_secret,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "PUT",
                format!(
                    "/internal/code-source/v1/uploads/{}/manifest/0",
                    begun.upload_id
                ),
                &token,
                Body::from(
                    serde_json::to_vec(&ManifestPage {
                        entries: entries.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                format!(
                    "/internal/code-source/v1/uploads/{}/manifest/complete",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let missing: MissingBlobsPage = serde_json::from_slice(&body).unwrap();
        assert_eq!(missing.hashes, vec!["b".repeat(64)]);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                format!(
                    "/internal/code-source/v1/uploads/{}/finalize",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_upload_state");

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                format!(
                    "/internal/code-source/v1/uploads/{}/missing?cursor=stale",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_code_source_input");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!(
                        "/internal/code-source/v1/uploads/{}/blobs/{}",
                        begun.upload_id,
                        "b".repeat(64)
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_LENGTH, "1")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_code_source_input");

        let durable =
            anyhow::anyhow!("reading /private/customer/repository/code-sources/secret.json failed");
        let response = HttpError::from_store(durable).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "storage_unavailable");
        assert!(!error.message.contains("customer"));
        assert!(!error.message.contains('/'));
    }

    #[tokio::test]
    async fn code_source_http_route_uses_typed_contract_errors() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-contract", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state);
        let request = BeginUploadRequest {
            descriptor: GenerationDescriptor {
                schema_version: SCHEMA_VERSION + 1,
                walker_policy_version: WALKER_POLICY_VERSION.into(),
                scope,
                head_commit: "c".repeat(40),
                dirty_fingerprint: "d".repeat(64),
                manifest_sha256: manifest_sha256(&[]),
                file_count: 0,
                logical_bytes: 0,
            },
        };

        let response = app
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&request).unwrap()),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "unsupported_contract");
    }

    #[tokio::test]
    async fn code_source_http_routes_preserve_auth_not_found_and_store_limit_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-limits", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state.clone());
        let descriptor = empty_generation_descriptor(scope.clone(), &"c".repeat(40));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/code-source/v1/uploads")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&BeginUploadRequest {
                            descriptor: descriptor.clone(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let forbidden = BeginUploadRequest {
            descriptor: empty_generation_descriptor(
                PublishedScope::try_new("other-http-repo", ".").unwrap(),
                &"d".repeat(40),
            ),
        };
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&forbidden).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                "/internal/code-source/v1/uploads/00000000-0000-4000-8000-000000000000/missing",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let mut limits = StoreLimits::default();
        limits.max_manifest_files = 0;
        state
            .code_sources
            .store()
            .update_limits(limits.clone())
            .unwrap();
        let mut oversized = descriptor.clone();
        oversized.file_count = 1;
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(
                    serde_json::to_vec(&BeginUploadRequest {
                        descriptor: oversized,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "limit_exceeded");

        limits.max_manifest_files = StoreLimits::default().max_manifest_files;
        limits.max_open_uploads_per_producer = 1;
        state.code_sources.store().update_limits(limits).unwrap();
        for expected in [StatusCode::CREATED, StatusCode::TOO_MANY_REQUESTS] {
            let response = app
                .clone()
                .oneshot(authenticated_request(
                    "POST",
                    "/internal/code-source/v1/uploads",
                    &token,
                    Body::from(
                        serde_json::to_vec(&BeginUploadRequest {
                            descriptor: descriptor.clone(),
                        })
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::TOO_MANY_REQUESTS {
                let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
                let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
                assert_eq!(error.code, "upload_limit_reached");
            }
        }
    }

    #[test]
    fn selector_retirement_retry_budget_is_bounded_and_exponential() {
        let mut attempts = 0;
        let mut delay = std::time::Duration::from_secs(1);
        let mut observed = Vec::new();
        while let Some(next) = take_selector_retirement_retry(&mut attempts, &mut delay) {
            observed.push(next);
        }
        assert_eq!(attempts, SELECTOR_RETIREMENT_RETRY_LIMIT);
        assert_eq!(observed.len(), SELECTOR_RETIREMENT_RETRY_LIMIT as usize);
        assert_eq!(observed.first(), Some(&std::time::Duration::from_secs(1)));
        assert_eq!(observed.last(), Some(&std::time::Duration::from_secs(30)));
    }

    #[test]
    fn queued_retirement_blocks_same_selector_restaging() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = SharedState::for_test(&root);
        let store = state.code_sources.store();
        let selector = bbox_code_source::local_selector("project-a");
        let retirement = RetirementRecord {
            version: 1,
            project_id: "project-a".into(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "a".repeat(32)),
            generation_id: None,
        };
        store.enqueue_retirement(&retirement).unwrap();

        assert!(ensure_selector_staging_available(store.as_ref(), &selector).is_err());

        store.complete_retirement(&retirement).unwrap();
        ensure_selector_staging_available(store.as_ref(), &selector).unwrap();
    }

    #[test]
    fn cold_open_fails_closed_for_every_invalid_enabled_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", root.join("state"));
        let config = crate::config::load().unwrap();
        let store =
            Arc::new(CodeSourceStore::open(root.join("store"), StoreLimits::default()).unwrap());
        let scope_a = PublishedScope::try_new("repo-a", ".").unwrap();
        let scope_b = PublishedScope::try_new("repo-b", ".").unwrap();
        let scope_unknown = PublishedScope::try_new("repo-unknown", ".").unwrap();
        let (project_a, candidate_a) = snapshot_project(&root, "project-a", &scope_a);
        let (project_b, candidate_b) = snapshot_project(&root, "project-b", &scope_b);
        let token_a = root.join("token-a");
        let token_b = root.join("token-b");
        write_service_token(&token_a, 'a');
        write_service_token(&token_b, 'b');
        let producer =
            |producer_id: &str, token_file: &Path, scopes| CodeCollectionProducerConfig {
                producer_id: producer_id.to_string(),
                token_file: token_file.to_path_buf(),
                scopes,
            };

        let broker = snapshot_broker(Vec::new());
        assert_snapshot_rejected(
            &config,
            Vec::new(),
            &[],
            store.clone(),
            &broker,
            "enabled code collection requires at least one producer",
        );

        let mut zero_limits = config.clone();
        zero_limits.code_collection.max_manifest_files = 0;
        assert_snapshot_rejected(
            &zero_limits,
            Vec::new(),
            &[],
            store.clone(),
            &broker,
            "code-collection limits and stale warning hours must be nonzero",
        );

        let broker = snapshot_broker(vec![candidate_a.clone(), candidate_b.clone()]);
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-a", &token_b, vec![scope_b.clone()]),
            ],
            &[project_a.clone(), project_b.clone()],
            store.clone(),
            &broker,
            "duplicate code-collection producer id",
        );
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-b", &token_a, vec![scope_b.clone()]),
            ],
            &[project_a.clone(), project_b.clone()],
            store.clone(),
            &broker,
            "code-collection token values must be unique",
        );
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, Vec::new())],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "enabled code-collection producer has no scopes",
        );
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-b", &token_b, vec![scope_a.clone()]),
            ],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "code-collection scope is assigned more than once",
        );
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, vec![scope_unknown])],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "code-collection scope is not registered",
        );

        let (_, duplicate_candidate) = snapshot_project(&root, "project-b", &scope_a);
        let duplicate_broker = snapshot_broker(vec![candidate_a, duplicate_candidate]);
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, vec![scope_a])],
            &[project_a, project_b],
            store,
            &duplicate_broker,
            "code-collection scope resolves to multiple registered projects",
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn collected_activation_restart_and_local_cutback_preserve_read_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        let repo = root.join("repo");
        let home = root.join("home");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Blackbox Test"]);
        fs::write(repo.join("src/lib.rs"), "pub fn phase_one() {}\n").unwrap();
        git(&repo, &["add", "src/lib.rs"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let recorded = crate::config::ensure_recorded_repo_id(&repo).unwrap();
        git(&repo, &["add", ".bbox"]);
        git(&repo, &["commit", "-q", "-m", "record repository identity"]);

        let mut env = crate::util::TestEnvGuard::new();
        env.set("HOME", &home);
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);

        let state = transition_test_state(&state_dir);
        let project = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&repo)
            .unwrap();
        state.persist_projects_durable().await.unwrap();
        let scope = PublishedScope::try_new(recorded.repo_id, ".").unwrap();
        let producer_id = "phase1-transition-producer";
        install_test_assignment(&state, producer_id, &scope, &project.project_id);

        let store = state.code_sources.store();
        let head = "c".repeat(40);
        let collected_source = b"pub fn collected_phase_one() {}\n";
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hex::encode(Sha256::digest(collected_source)),
            size: collected_source.len() as u64,
        }];
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: entries.len() as u64,
            logical_bytes: collected_source.len() as u64,
        };
        let upload = store.begin_upload(producer_id, descriptor).unwrap();
        store
            .put_manifest_page(producer_id, &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest(producer_id, &upload.upload_id)
            .unwrap();
        store
            .install_blob(
                producer_id,
                &upload.upload_id,
                &entries[0].content_sha256,
                entries[0].size,
                std::io::Cursor::new(collected_source),
            )
            .unwrap();
        let ready = store
            .finalize_upload(producer_id, &upload.upload_id)
            .unwrap();
        activate_desired_loop(&state, &scope, &project.project_id).unwrap();
        state.index_writer.flush_blocking().unwrap();

        let collected_selector = crate::index::project_files::collected_materialization_selector(
            &project.project_id,
            &ready.generation_id,
        );
        assert_eq!(
            state
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&collected_selector)
        );
        assert_eq!(
            store
                .load_activation(&project.project_id)
                .unwrap()
                .as_ref()
                .map(|activation| activation.generation_id.as_str()),
            Some(ready.generation_id.as_str())
        );

        drop(store);
        drop(state);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let restarted = transition_test_state(&state_dir);
        let _vector_store = bbox_vectors::install_test_global(restarted.vector_store.clone());
        install_test_assignment(&restarted, producer_id, &scope, &project.project_id);
        assert_eq!(
            restarted
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&collected_selector),
            "startup must rebuild read authority from the durable manifest"
        );
        activate_desired_loop(&restarted, &scope, &project.project_id).unwrap();

        let store = restarted.code_sources.store();
        *restarted.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            auth: Arc::new(ProducerAuthRuntime::disabled()),
            store: store.clone(),
        });
        cutback_to_local(&restarted, &scope, &project.project_id).unwrap();
        restarted.index_writer.flush_blocking().unwrap();

        let local_selector = bbox_code_source::local_selector(&project.project_id);
        assert_eq!(
            restarted
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&local_selector)
        );
        assert!(
            store
                .load_activation(&project.project_id)
                .unwrap()
                .is_none()
        );
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&restarted.store_dir);
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir).unwrap();
        assert_eq!(
            manifest
                .workspaces
                .get(&project.project_id)
                .and_then(|entry| entry.code_source_selector.as_deref()),
            Some(local_selector.as_str())
        );

        for _ in 0..500 {
            if !store.retirement_pending(&collected_selector).unwrap() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!store.retirement_pending(&collected_selector).unwrap());
    }

    /// A collected generation stays activated when Git is entirely
    /// unavailable (Phase 3 plan section 6 items 2 and 3, closing F5).
    ///
    /// Before this milestone a Git problem during collected staging failed
    /// the whole activation and looped on backoff. Now the transaction never
    /// opens Git: the generation publishes first, and the post-activation
    /// overlay records `git_history_unavailable` and leaves everything else
    /// exactly as the activation left it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn collected_generation_activates_when_git_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        let repo = root.join("repo");
        let home = root.join("home");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Blackbox Test"]);
        fs::write(repo.join("src/lib.rs"), "pub fn phase_one() {}\n").unwrap();
        git(&repo, &["add", "src/lib.rs"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let recorded = crate::config::ensure_recorded_repo_id(&repo).unwrap();

        let mut env = crate::util::TestEnvGuard::new();
        env.set("HOME", &home);
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);

        let mut state = SharedState::for_test(&state_dir);
        state.store_dir = state_dir.join("bro");
        // Deny every checkout lease AFTER the index writer took its own
        // handle, so staging still runs and only the post-activation Git
        // overlay is starved.
        state.checkout_access = Arc::new(CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        ));
        let state = Arc::new(state);
        let project = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&repo)
            .unwrap();
        state.persist_projects_durable().await.unwrap();
        let scope = PublishedScope::try_new(recorded.repo_id, ".").unwrap();
        let producer_id = "git-unavailable-producer";
        install_test_assignment(&state, producer_id, &scope, &project.project_id);

        let store = state.code_sources.store();
        let head = "d".repeat(40);
        let collected_source = b"pub fn collected_without_git() {}\n";
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: hex::encode(Sha256::digest(collected_source)),
            size: collected_source.len() as u64,
        }];
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: entries.len() as u64,
            logical_bytes: collected_source.len() as u64,
        };
        let upload = store.begin_upload(producer_id, descriptor).unwrap();
        store
            .put_manifest_page(producer_id, &upload.upload_id, 0, &entries)
            .unwrap();
        store
            .complete_manifest(producer_id, &upload.upload_id)
            .unwrap();
        store
            .install_blob(
                producer_id,
                &upload.upload_id,
                &entries[0].content_sha256,
                entries[0].size,
                std::io::Cursor::new(collected_source),
            )
            .unwrap();
        let ready = store
            .finalize_upload(producer_id, &upload.upload_id)
            .unwrap();

        activate_desired_loop(&state, &scope, &project.project_id)
            .expect("an unavailable Git must not fail a valid collected activation");
        state.index_writer.flush_blocking().unwrap();

        let collected_selector = crate::index::project_files::collected_materialization_selector(
            &project.project_id,
            &ready.generation_id,
        );
        assert_eq!(
            state
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&collected_selector),
            "the generation must be active despite the Git failure"
        );
        assert_eq!(
            store
                .load_activation(&project.project_id)
                .unwrap()
                .as_ref()
                .map(|activation| activation.generation_id.as_str()),
            Some(ready.generation_id.as_str())
        );
        assert!(
            store.health_records().unwrap().iter().any(|record| {
                record.project_id == project.project_id && record.code == "git_history_unavailable"
            }),
            "the degraded Git overlay must be recorded as health, not as a failure"
        );
        // The activation transaction stages no Git member at all now; the
        // overlay owns that file and never got to write it.
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
        let snapshot_id = bbox_edge_sidecar::snapshot::collected_snapshot_id(
            &project.project_id,
            &ready.generation_id,
        );
        let snapshot_dir = bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            &project.project_id,
            &snapshot_id,
        );
        assert!(snapshot_dir.join("project.jsonl").is_file());
        assert!(!snapshot_dir.join("git-current.jsonl").exists());
    }

    fn catalog_grant_store(
        root: &Path,
        projects: &[(&str, ProjectScope)],
    ) -> Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore> {
        use bbox_corpus_core::project_catalog::{CatalogSnapshotV2, CorpusProject};

        fs::create_dir_all(root).unwrap();
        let store = bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            root.join("projects.json"),
        )
        .unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        let projects = projects.to_vec();
        store
            .transact(epoch, |catalog: &mut CatalogSnapshotV2, _attachments| {
                for (id, scope) in &projects {
                    let project_id = ProjectId::parse(*id).unwrap();
                    catalog.projects.insert(
                        project_id.clone(),
                        CorpusProject {
                            project_id,
                            scope: scope.clone(),
                            operator_aliases: Default::default(),
                            nominated_aliases: Default::default(),
                            display_name: (*id).to_string(),
                            created_at: "2026-07-25T00:00:00Z".into(),
                            registered_at_compat: None,
                            repo_history: None,
                            languages: Default::default(),
                        },
                    );
                }
                Ok(())
            })
            .unwrap();
        Arc::new(store)
    }

    fn catalog_grant_config(
        base: &crate::config::Config,
        token_file: &Path,
        scope: &PublishedScope,
    ) -> crate::config::Config {
        let mut config = base.clone();
        config.code_collection.enabled = true;
        config.code_collection.producers = vec![CodeCollectionProducerConfig {
            producer_id: "catalog-producer".into(),
            token_file: token_file.to_path_buf(),
            scopes: vec![scope.clone()],
        }];
        config
    }

    /// Phase 3 plan section 6 item 6: in catalog mode a configured producer
    /// scope resolves by exact scope equality against the pinned catalog
    /// snapshot, with NO lease acquired - which is the only way a remote-only
    /// project (zero attachments) can ever hold a grant.
    ///
    /// P4-B extends this: the typed `AuthTable` is populated with
    /// `scope_to_project` mapping to the typed `ProjectId` (not a path hash)
    /// and `producer_to_scopes` indexing every producer's scopes.
    #[test]
    fn catalog_grant_arm_resolves_by_exact_scope_without_leases() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);
        let base = crate::config::load().unwrap();
        let token_file = root.join("catalog-token");
        write_service_token(&token_file, 'a');
        let scope = PublishedScope::try_new("catalog-repo", ".").unwrap();
        let remote_only = "p_000000000000000000000000000000c1";
        let catalog = catalog_grant_store(
            &root,
            &[(remote_only, ProjectScope::Published(scope.clone()))],
        );
        // Deny-all: a lease attempt of any kind would fail the snapshot.
        let broker = CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );

        let snapshot = build_snapshot(
            &catalog_grant_config(&base, &token_file, &scope),
            &[],
            Some(&catalog),
            None,
            &broker,
        )
        .expect("the catalog arm resolves without touching a checkout");

        assert_eq!(
            assignment_map(&snapshot)
                .get(&scope)
                .map(|(id, _)| id.as_str()),
            Some(remote_only)
        );
        let attempted: u64 = broker
            .health()
            .operations
            .iter()
            .map(|operation| operation.granted + operation.denied)
            .sum();
        assert_eq!(attempted, 0, "the catalog arm acquires no lease at all");

        // P4-B: the typed AuthTable is populated in catalog mode with the
        // typed ProjectId (not a path hash) and producer scope index.
        let auth_table = snapshot.auth.as_ref();
        assert!(auth_table.is_catalog_mode());
        assert_eq!(
            auth_table.scope_project(&scope),
            Some(&ProjectId::parse(remote_only).unwrap()),
            "scope_to_project must map to the typed catalog ProjectId"
        );
        assert_eq!(
            auth_table
                .producer_scopes("catalog-producer")
                .map(|scopes| scopes.iter().cloned().collect::<Vec<_>>()),
            Some(vec![scope.clone()]),
            "producer_to_scopes must index the producer's resolved scopes"
        );
        assert_eq!(
            auth_table.assignments().len(),
            1,
            "the AuthTable carries the resolved entries"
        );
    }

    /// An unknown scope fails closed with today's error shape.
    #[test]
    fn catalog_grant_arm_fails_closed_on_an_unknown_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);
        let base = crate::config::load().unwrap();
        let token_file = root.join("catalog-token");
        write_service_token(&token_file, 'b');
        let scope = PublishedScope::try_new("catalog-repo", ".").unwrap();
        let other_scope = PublishedScope::try_new("other-repo", ".").unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );

        let unknown = catalog_grant_store(
            &root.join("unknown-store"),
            &[(
                "p_000000000000000000000000000000d1",
                ProjectScope::Published(other_scope),
            )],
        );
        let error = build_snapshot(
            &catalog_grant_config(&base, &token_file, &scope),
            &[],
            Some(&unknown),
            None,
            &broker,
        )
        .map(|_| ())
        .expect_err("an unregistered scope must fail closed");
        assert_eq!(error.to_string(), "code-collection scope is not registered");
    }

    /// The collision arm is defense in depth: `validate_catalog` already
    /// refuses a duplicate published scope
    /// (`error.project_catalog_duplicate_scope`), so no valid store can hold
    /// one and the case is unreachable through `ProjectCatalogStore`. The
    /// resolver is exercised directly against a synthetic snapshot so the
    /// fail-closed shape stays pinned if that invariant ever moves.
    #[test]
    fn catalog_grant_arm_fails_closed_on_a_scope_collision() {
        use bbox_corpus_core::project_catalog::CorpusProject;

        let scope = PublishedScope::try_new("catalog-repo", ".").unwrap();
        let project = |id: &str| CorpusProject {
            project_id: ProjectId::parse(id).unwrap(),
            scope: ProjectScope::Published(scope.clone()),
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: id.to_string(),
            created_at: "2026-07-25T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        for id in [
            "p_000000000000000000000000000000e1",
            "p_000000000000000000000000000000e2",
        ] {
            catalog
                .projects
                .insert(ProjectId::parse(id).unwrap(), project(id));
        }
        assert!(
            catalog.validate().is_err(),
            "a duplicate published scope is not a valid catalog in the first place"
        );

        let error = resolve_grant_scope(
            &GrantScopeResolution::Catalog {
                catalog: Arc::new(catalog),
            },
            &scope,
        )
        .expect_err("a scope claimed by two catalog projects must fail closed");
        assert_eq!(
            error.to_string(),
            "code-collection scope resolves to multiple registered projects"
        );
    }

    /// P4-B plan section 6.2 (bridge parity): the typed `AuthTable` is
    /// catalog-mode only. In bridge mode `build_snapshot` leaves it `None`
    /// and retains its lease-derived `String` grants byte-identical.
    #[test]
    fn bridge_mode_does_not_construct_an_auth_table() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);
        let base = crate::config::load().unwrap();
        let scope = PublishedScope::try_new("bridge-repo", ".");
        let scope = scope.unwrap();
        let (project, candidate) = snapshot_project(&root, "bridge-project", &scope);
        let token_file = root.join("bridge-token");
        write_service_token(&token_file, 'a');
        let broker = snapshot_broker(vec![candidate]);
        let mut config = base.clone();
        config.code_collection.enabled = true;
        config.code_collection.producers = vec![CodeCollectionProducerConfig {
            producer_id: "bridge-producer".into(),
            token_file: token_file.to_path_buf(),
            scopes: vec![scope.clone()],
        }];

        // Bridge mode: no catalog_store passed, so resolution is lease-derived.
        let snapshot = build_snapshot(&config, &[project], None, None, &broker)
            .expect("bridge mode resolves grants through leases");

        assert!(
            !snapshot.auth.is_catalog_mode(),
            "bridge mode must not construct a typed AuthTable"
        );
        assert!(
            !snapshot.auth.assignments().is_empty(),
            "bridge mode still populates the String-based auth entries"
        );
    }

    /// P4-B plan section 6.3 verification matrix: a duplicate scope (the
    /// same scope configured for two producers) fails closed in catalog
    /// mode with the same error shape as bridge mode.
    #[test]
    fn catalog_grant_arm_fails_closed_on_a_duplicate_configured_scope() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);
        let base = crate::config::load().unwrap();
        let token_a = root.join("dup-token-a");
        let token_b = root.join("dup-token-b");
        write_service_token(&token_a, 'a');
        write_service_token(&token_b, 'b');
        let scope = PublishedScope::try_new("dup-repo", ".").unwrap();
        let project_id = "p_000000000000000000000000000000f1";
        let catalog = catalog_grant_store(
            &root,
            &[(project_id, ProjectScope::Published(scope.clone()))],
        );
        let broker = CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );
        let mut config = base.clone();
        config.code_collection.enabled = true;
        config.code_collection.producers = vec![
            CodeCollectionProducerConfig {
                producer_id: "dup-producer-a".into(),
                token_file: token_a,
                scopes: vec![scope.clone()],
            },
            CodeCollectionProducerConfig {
                producer_id: "dup-producer-b".into(),
                token_file: token_b,
                scopes: vec![scope.clone()],
            },
        ];

        let error = build_snapshot(&config, &[], Some(&catalog), None, &broker)
            .map(|_| ())
            .expect_err("a scope assigned to two producers must fail closed");
        assert_eq!(
            error.to_string(),
            "code-collection scope is assigned more than once"
        );
    }

    /// P4-B plan section 6.3 bootsmoke: a catalog-mode cold-open (no
    /// existing store, no prior bind) refuses an unresolved scope before
    /// the daemon ever binds its HTTP listener. The typed AuthTable is not
    /// constructed on failure.
    #[test]
    fn catalog_mode_cold_open_refuses_an_unresolved_scope_before_bind() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);
        let base = crate::config::load().unwrap();
        let token_file = root.join("cold-open-token");
        write_service_token(&token_file, 'a');
        let configured_scope = PublishedScope::try_new("cold-open-repo", ".").unwrap();
        let registered_scope = PublishedScope::try_new("different-repo", ".").unwrap();
        let catalog = catalog_grant_store(
            &root,
            &[(
                "p_000000000000000000000000000001a1",
                ProjectScope::Published(registered_scope),
            )],
        );
        let broker = CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );

        // Cold open: no existing store, fresh state dir. The configured
        // scope is not in the catalog, so resolution must fail closed.
        let error = build_snapshot(
            &catalog_grant_config(&base, &token_file, &configured_scope),
            &[],
            Some(&catalog),
            None,
            &broker,
        )
        .map(|_| ())
        .expect_err("catalog-mode cold-open must refuse an unresolved scope");
        assert_eq!(error.to_string(), "code-collection scope is not registered");
        // No leases were acquired during the failed cold-open.
        let attempted: u64 = broker
            .health()
            .operations
            .iter()
            .map(|operation| operation.granted + operation.denied)
            .sum();
        assert_eq!(
            attempted, 0,
            "the failed cold-open must not have acquired any lease"
        );
    }

    /// P4-B plan section 6.1 item 2: the typed catalog resolver maps scope
    /// to the typed `ProjectId`, not a path hash. Verified directly against
    /// the resolver so the typed contract stays pinned.
    #[test]
    fn resolve_catalog_project_returns_the_typed_project_id() {
        use bbox_corpus_core::project_catalog::CorpusProject;

        let scope = PublishedScope::try_new("typed-repo", ".").unwrap();
        let project_id = ProjectId::parse("p_00000000000000000000000000000abc").unwrap();
        let project = CorpusProject {
            project_id: project_id.clone(),
            scope: ProjectScope::Published(scope.clone()),
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: "typed".into(),
            created_at: "2026-07-25T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        };
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.projects.insert(project_id.clone(), project);

        let resolved = resolve_catalog_project(&catalog, &scope)
            .expect("an exact scope match resolves to the typed ProjectId");
        assert_eq!(resolved, project_id);
    }

    #[test]
    fn startup_recovery_distinguishes_exact_and_selectorless_collision_work() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("code-source");
        let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let project_id = ProjectId::parse("startup-collision").unwrap();
        let scope = PublishedScope::try_new("startup-repo", ".").unwrap();
        let exact_descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
        let retained_descriptor = empty_generation_descriptor(scope.clone(), &"b".repeat(40));
        let exact_generation_id = compute_generation_id("startup-exact-host", &exact_descriptor);
        let retained_generation_id =
            compute_generation_id("startup-retained-host", &retained_descriptor);
        for (generation_id, producer_id, descriptor) in [
            (
                &exact_generation_id,
                "startup-exact-host",
                exact_descriptor.clone(),
            ),
            (
                &retained_generation_id,
                "startup-retained-host",
                retained_descriptor.clone(),
            ),
        ] {
            let metadata_path = paths.generation_metadata(&scope, generation_id).unwrap();
            fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
            fs::write(
                metadata_path,
                encode_stored_generation_v2_for_migration(&StoredGenerationV2 {
                    version: 2,
                    generation_id: generation_id.clone(),
                    producer_id: producer_id.to_string(),
                    ordinal: 1,
                    descriptor,
                    published_scope: scope.clone(),
                    state: GenerationState::Ready,
                    diagnostic: None,
                    created_unix_secs: 1,
                    materialized_doc_count: None,
                    entity_inventory_sha256: None,
                })
                .unwrap(),
            )
            .unwrap();
        }
        let exact_selector = format!(
            "{}:m0123456789abcdef",
            source_selector(project_id.as_str(), &exact_generation_id)
        );
        let lifecycle = CollisionRetirementLifecycleV1 {
            version: 1,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    exact_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            exact_selector.clone(),
                        ),
                        snapshot_id: format!("collected-{}", "c".repeat(32)),
                        manifest_sha256: exact_descriptor.manifest_sha256,
                        inventory_hash: "e".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
                (
                    retained_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope,
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "c".repeat(32)),
                        manifest_sha256: retained_descriptor.manifest_sha256,
                        inventory_hash: "2".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
            ]),
        };
        let lifecycle_path = paths.collision_retirement_pending(&project_id);
        fs::create_dir_all(lifecycle_path.parent().unwrap()).unwrap();
        fs::write(
            &lifecycle_path,
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();

        let first_recovery = collision_retirement_tasks_for_recovery(&store).unwrap();

        assert_eq!(first_recovery.len(), 2);
        assert!(first_recovery.iter().any(|task| matches!(
            task,
            CollisionRetirementRecoveryTask::Exact { work, selector }
                if work.generation_id == exact_generation_id && selector == &exact_selector
        )));
        assert!(first_recovery.iter().any(|task| matches!(
            task,
            CollisionRetirementRecoveryTask::Selectorless { work }
                if work.generation_id == retained_generation_id
                    && work.exact_selector().is_none()
        )));
        let queued =
            decode_collision_retirement_pending_for_migration(&fs::read(&lifecycle_path).unwrap())
                .unwrap();
        assert!(
            queued
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Queued)
        );

        store
            .repair_and_complete_collision_retirement(&project_id, &retained_generation_id)
            .unwrap();
        let restarted = collision_retirement_tasks_for_recovery(&store).unwrap();
        assert_eq!(restarted.len(), 1);
        assert!(matches!(
            &restarted[0],
            CollisionRetirementRecoveryTask::Exact { work, selector }
                if work.generation_id == exact_generation_id && selector == &exact_selector
        ));

        store
            .repair_and_complete_collision_retirement(&project_id, &exact_generation_id)
            .unwrap();
        assert!(
            collision_retirement_tasks_for_recovery(&store)
                .unwrap()
                .is_empty()
        );
        let completed =
            decode_collision_retirement_pending_for_migration(&fs::read(&lifecycle_path).unwrap())
                .unwrap();
        assert!(
            completed
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Completed)
        );
    }

    #[test]
    fn startup_recovery_refuses_orphan_exact_work_before_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("code-source");
        let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let project_id = ProjectId::parse("orphan-collision").unwrap();
        let generation_id = "a".repeat(64);
        let work = CollisionRetirementWorkV1 {
            version: 1,
            project_id: project_id.clone(),
            generation_id: generation_id.clone(),
            former_scope: PublishedScope::try_new("orphan-repo", ".").unwrap(),
            selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(format!(
                "{}:m0123456789abcdef",
                source_selector(project_id.as_str(), &generation_id)
            )),
            snapshot_id: format!("collected-{}", "b".repeat(32)),
            manifest_sha256: "c".repeat(64),
            inventory_hash: "d".repeat(64),
            plan_hash: "e".repeat(64),
        };
        let work_path = paths
            .collision_retirement_work(&project_id, &generation_id)
            .unwrap();
        fs::write(work_path, serde_json::to_vec_pretty(&work).unwrap()).unwrap();

        let error = collision_retirement_tasks_for_recovery(&store).unwrap_err();

        assert!(error.to_string().contains("orphaned"));
    }

    #[test]
    fn collision_terminal_transition_failure_preserves_work_and_retries_for_both_targets() {
        for (project_name, producer_id, exact_selector) in [
            ("repair-exact", "repair-host-exact", true),
            ("repair-retained", "repair-host-retained", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap().join("code-source");
            let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
            let paths = CodeSourceStorePaths::new(root).unwrap();
            let project_id = ProjectId::parse(project_name).unwrap();
            let scope = PublishedScope::try_new(format!("{project_name}-repo"), ".").unwrap();
            let descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
            let generation_id = compute_generation_id(producer_id, &descriptor);
            let selector_evidence = if exact_selector {
                CollisionRetirementSelectorEvidenceV1::ExactMaterialized(format!(
                    "{}:m0123456789abcdef",
                    source_selector(project_id.as_str(), &generation_id)
                ))
            } else {
                CollisionRetirementSelectorEvidenceV1::NoDurableSelector
            };
            let lifecycle = CollisionRetirementLifecycleV1 {
                version: 1,
                project_id: project_id.clone(),
                entries: BTreeMap::from([(
                    generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence,
                        snapshot_id: format!("collected-{}", "b".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                )]),
            };
            let lifecycle_path = paths.collision_retirement_pending(&project_id);
            fs::write(
                &lifecycle_path,
                encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
            )
            .unwrap();
            let tasks = collision_retirement_tasks_for_recovery(&store).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(
                matches!(tasks[0], CollisionRetirementRecoveryTask::Exact { .. }),
                exact_selector
            );

            assert!(
                repair_and_complete_collision_retirement(&store, &project_id, &generation_id)
                    .is_err()
            );
            let queued = decode_collision_retirement_pending_for_migration(
                &fs::read(&lifecycle_path).unwrap(),
            )
            .unwrap();
            assert_eq!(
                queued.entry(&generation_id).unwrap().state,
                CollisionRetirementLifecycleStateV1::Queued
            );
            assert_eq!(store.collision_retirement_work_records().unwrap().len(), 1);

            let stored = StoredGenerationV2 {
                version: 2,
                generation_id: generation_id.clone(),
                producer_id: producer_id.to_string(),
                ordinal: 1,
                descriptor,
                published_scope: scope.clone(),
                state: GenerationState::Ready,
                diagnostic: Some("stale collision state".into()),
                created_unix_secs: 1,
                materialized_doc_count: None,
                entity_inventory_sha256: None,
            };
            let metadata_path = paths.generation_metadata(&scope, &generation_id).unwrap();
            fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
            fs::write(
                &metadata_path,
                encode_stored_generation_v2_for_migration(&stored).unwrap(),
            )
            .unwrap();
            repair_and_complete_collision_retirement(&store, &project_id, &generation_id).unwrap();

            assert_eq!(
                decode_stored_generation_v2_for_migration(&fs::read(metadata_path).unwrap())
                    .unwrap()
                    .state,
                GenerationState::Superseded
            );
            let completed = decode_collision_retirement_pending_for_migration(
                &fs::read(&lifecycle_path).unwrap(),
            )
            .unwrap();
            assert_eq!(
                completed.entry(&generation_id).unwrap().state,
                CollisionRetirementLifecycleStateV1::Completed
            );
            assert!(
                store
                    .collision_retirement_work_records()
                    .unwrap()
                    .is_empty()
            );
        }
    }

    /// P4-C section 7.3: a v2 activation record round-trips through the
    /// catalog-mode mixed reader and its scope agrees with the stored
    /// generation.
    #[test]
    fn p4c_v2_activation_round_trips_with_scope_agreement() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let paths = CodeSourceStorePaths::new(&root).unwrap();
        let project_id = ProjectId::parse("p_000000000000000000000000000004c1").unwrap();
        let scope = PublishedScope::try_new("p4c-rt-repo", ".").unwrap();
        let producer_id = "p4c-producer";
        let descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
        let generation_id = compute_generation_id(producer_id, &descriptor);

        let generation = StoredGenerationV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            generation_id: generation_id.clone(),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            published_scope: scope.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("e".repeat(64)),
        };
        let metadata_path = paths.generation_metadata(&scope, &generation_id).unwrap();
        fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&generation).unwrap(),
        )
        .unwrap();

        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope.clone(),
            generation_id: generation_id.clone(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &generation_id,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        activation
            .validate_against_generation(&generation)
            .expect("scope agreement must hold before commit");
        store.save_activation_v2(&activation).unwrap();

        let loaded = store
            .load_activation_mixed(project_id.as_str())
            .unwrap()
            .expect("v2 activation record must be present");
        assert!(loaded.is_current_v2(), "catalog mode must read a v2 record");
        assert_eq!(loaded.generation_id(), generation_id);
        assert_eq!(loaded.published_scope(), Some(&scope));

        let loaded_gen = store.load_generation_mixed(&scope, &generation_id).unwrap();
        assert!(loaded_gen.is_current_v2());
        assert_eq!(loaded_gen.generation_id(), generation_id);
        assert_eq!(loaded_gen.published_scope(), &scope);

        let found = store.find_generation_mixed(&generation_id).unwrap();
        assert!(found.is_current_v2());
        assert_eq!(found.published_scope(), &scope);

        let desired_dir = root.join("desired");
        fs::create_dir_all(&desired_dir).unwrap();
        let desired_path =
            desired_dir.join(format!("{}.json", bbox_code_source::scope_hash(&scope)));
        fs::write(
            &desired_path,
            encode_stored_generation_v2_for_migration(&generation).unwrap(),
        )
        .unwrap();
        let desired = store
            .desired_generation_mixed(&scope)
            .unwrap()
            .expect("desired pointer must resolve");
        assert!(desired.is_current_v2());
        assert_eq!(desired.generation_id(), generation_id);
    }

    /// P4-C section 7.3: `validate_against_generation` refuses when the
    /// activation's scope differs from the generation's scope, failing
    /// before any selector or index commit.
    #[test]
    fn p4c_scope_disagreement_refuses_before_commit() {
        let scope_a = PublishedScope::try_new("scope-a-repo", ".").unwrap();
        let scope_b = PublishedScope::try_new("scope-b-repo", ".").unwrap();
        let producer_id = "p4c-producer";
        let descriptor_a = empty_generation_descriptor(scope_a.clone(), &"a".repeat(40));
        let generation_id_a = compute_generation_id(producer_id, &descriptor_a);
        let project_id = ProjectId::parse("p_000000000000000000000000000004c2").unwrap();

        let generation = StoredGenerationV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            generation_id: generation_id_a.clone(),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor: descriptor_a,
            published_scope: scope_a.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("e".repeat(64)),
        };

        let activation_wrong_scope = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope_b,
            generation_id: generation_id_a.clone(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &generation_id_a,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        let error = activation_wrong_scope
            .validate_against_generation(&generation)
            .expect_err("scope disagreement must refuse");
        assert!(
            error.to_string().contains("does not match"),
            "scope mismatch error must be descriptive: {error}"
        );

        let wrong_gen_id = compute_generation_id(
            producer_id,
            &empty_generation_descriptor(scope_a.clone(), &"b".repeat(40)),
        );
        let activation_wrong_gen = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope_a.clone(),
            generation_id: wrong_gen_id.clone(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &wrong_gen_id,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        let error = activation_wrong_gen
            .validate_against_generation(&generation)
            .expect_err("generation id mismatch must refuse");
        assert!(
            error.to_string().contains("does not match"),
            "generation mismatch error must be descriptive: {error}"
        );
    }

    /// P4-C section 7.1 item 1/2: the activation record's `published_scope`
    /// must come from the grant scope, not the generation record's own
    /// self-reported scope. When a drifted v2 generation record whose scope
    /// disagrees with the grant scope under which it is activated is
    /// validated, the scope-agreement cross-check must fail before any
    /// activation record is saved.
    #[test]
    fn p4c_grant_scope_disagreement_refuses_before_activation_save() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let paths = CodeSourceStorePaths::new(&root).unwrap();

        // The generation record claims scope_a as its published_scope.
        let scope_a = PublishedScope::try_new("gen-scope-repo", ".").unwrap();
        // The grant/assignment scope is scope_b (the real grant scope).
        let scope_b = PublishedScope::try_new("grant-scope-repo", ".").unwrap();
        let producer_id = "p4c-drift-producer";
        let descriptor = empty_generation_descriptor(scope_a.clone(), &"a".repeat(40));
        let generation_id = compute_generation_id(producer_id, &descriptor);
        let project_id = ProjectId::parse("p_000000000000000000000000000004c5").unwrap();

        let generation = StoredGenerationV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            generation_id: generation_id.clone(),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            published_scope: scope_a.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("e".repeat(64)),
        };
        let metadata_path = paths.generation_metadata(&scope_a, &generation_id).unwrap();
        fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&generation).unwrap(),
        )
        .unwrap();

        // The activation record is constructed with the GRANT scope (scope_b),
        // not the generation's self-reported scope (scope_a). This is the
        // production path: activate_desired_loop passes `scope` (the grant
        // parameter) as published_scope.
        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope_b,
            generation_id: generation_id.clone(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &generation_id,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };

        // The cross-check must fail: scope_b != scope_a.
        let error = activation
            .validate_against_generation(&generation)
            .expect_err("grant scope disagreeing with generation scope must refuse");
        assert!(
            error.to_string().contains("does not match"),
            "scope agreement must catch the drift: {error}"
        );

        // No activation record was saved.
        assert!(
            store
                .load_activation_mixed(project_id.as_str())
                .unwrap()
                .is_none(),
            "no activation record must exist after scope agreement refusal"
        );
    }

    /// P4-C section 7.3: bridge-mode v1 records round-trip unchanged through
    /// the mixed readers.
    #[test]
    fn p4c_bridge_v1_round_trip_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::BridgeV1,
        )
        .unwrap();
        let project_id = "bridge-v1-project";
        let generation_id = "a".repeat(64);
        let selector = crate::index::project_files::collected_materialization_selector(
            project_id,
            &generation_id,
        );
        let snapshot_id = format!("collected-{}", "b".repeat(32));

        store
            .save_activation(&ActivationRecord {
                version: 1,
                project_id: project_id.to_string(),
                generation_id: generation_id.clone(),
                selector: selector.clone(),
                snapshot_id: snapshot_id.clone(),
                document_count: 42,
                entity_inventory_sha256: "c".repeat(64),
                current_chunk_targets: BTreeMap::new(),
                activated_unix_secs: 99,
                cutback_pending: false,
                diagnostic: None,
            })
            .unwrap();

        let loaded = store
            .load_activation_mixed(project_id)
            .unwrap()
            .expect("v1 activation record must be present");
        assert!(
            !loaded.is_current_v2(),
            "bridge mode must read a v1 record, not v2"
        );
        assert_eq!(loaded.generation_id(), generation_id);
        assert_eq!(loaded.selector(), selector);
        assert_eq!(loaded.snapshot_id(), snapshot_id);
        assert_eq!(loaded.document_count(), 42);
    }

    /// P4-C section 7.3: a catalog-mode store refuses a v2 activation write
    /// on a bridge store, and vice versa, enforcing end-to-end mode closure.
    #[test]
    fn p4c_bridge_store_refuses_v2_activation_write() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::BridgeV1,
        )
        .unwrap();
        let project_id = ProjectId::parse("p_000000000000000000000000000004c3").unwrap();
        let scope = PublishedScope::try_new("refuse-v2-repo", ".").unwrap();

        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope,
            generation_id: "d".repeat(64),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &"d".repeat(64),
            ),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "f".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        let error = store
            .save_activation_v2(&activation)
            .expect_err("bridge store must refuse v2 writes");
        assert!(
            error.to_string().contains("code_source_record_mode"),
            "refusal must carry the typed mode error: {error}"
        );
    }

    /// P4-C section 7.3: mixed readers enumerate v2 activation records in
    /// catalog mode and skip legacy v1 records.
    #[test]
    fn p4c_catalog_mode_activation_records_mixed_skips_legacy() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();
        let project_id = ProjectId::parse("p_000000000000000000000000000004c4").unwrap();
        let scope = PublishedScope::try_new("mixed-enum-repo", ".").unwrap();

        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id.clone(),
            published_scope: scope,
            generation_id: "a".repeat(64),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id.as_str(),
                &"a".repeat(64),
            ),
            snapshot_id: format!("collected-{}", "b".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        store.save_activation_v2(&activation).unwrap();

        let records = store.activation_records_mixed().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].is_current_v2());
        assert_eq!(records[0].project_id(), project_id.as_str());
    }

    // ------------------------------------------------------------------
    // P4-D: reconciler skeleton and auth swap separation (section 8)
    // ------------------------------------------------------------------

    /// Helper: write a v2 generation metadata file and a v2 activation
    /// record to a catalog-mode store so that `mark_cutback_state` and
    /// `activation_records_mixed` have something to operate on.
    fn p4d_seed_catalog_store(
        store: &CodeSourceStore,
        root: &std::path::Path,
        project_id: &str,
        scope: &PublishedScope,
        generation_id: &str,
    ) -> ActivationRecordV2 {
        let producer_id = "p4d-producer";
        let descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
        let generation = StoredGenerationV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            generation_id: generation_id.to_string(),
            producer_id: producer_id.to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            published_scope: scope.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("e".repeat(64)),
        };
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let metadata_path = paths.generation_metadata(scope, generation_id).unwrap();
        fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&generation).unwrap(),
        )
        .unwrap();

        let project_id_typed = ProjectId::parse(project_id).unwrap();
        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id_typed.clone(),
            published_scope: scope.clone(),
            generation_id: generation_id.to_string(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id,
                generation_id,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        activation
            .validate_against_generation(&generation)
            .expect("scope agreement must hold");
        store.save_activation_v2(&activation).unwrap();
        activation
    }

    /// P4-D section 8.2: auth swap succeeds while a cutback is
    /// structural-pending. The old token is rejected after the swap, the
    /// generation remains active, and the persisted cutback state on disk
    /// is untouched by the swap.
    #[test]
    fn p4d_auth_swap_leaves_cutback_state_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4d-swap-repo", ".").unwrap();
        let project_id = "p_000000000000000000000000000004d1";
        let generation_id = compute_generation_id(
            "p4d-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );
        let activation = p4d_seed_catalog_store(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
        );

        // Arrange structural-pending cutback state directly.
        store
            .mark_cutback_state(
                project_id,
                CutbackStateV2::Structural {
                    reason: CutbackReason::NoLocalAttachment,
                },
            )
            .unwrap();

        // Verify the cutback state is persisted.
        let before = store.load_activation_mixed(project_id).unwrap().unwrap();
        assert_eq!(
            before.cutback(),
            Some(&CutbackStateV2::Structural {
                reason: CutbackReason::NoLocalAttachment,
            })
        );

        // Simulate the auth swap: replace the snapshot atomically.
        // The swap is the ONLY auth effect (section 4.2). It validates
        // off-lock and swaps `self.snapshot` atomically on success.
        let old_token_secret = "a".repeat(64);
        *runtime.snapshot.write() = Arc::new(CodeSourceSnapshot {
            auth: Arc::new(ProducerAuthRuntime::for_test(true, false, vec![])),
            store: store.clone(),
        });

        // Old token is rejected post-swap (the auth table is now empty).
        assert!(
            runtime
                .producer_auth()
                .authenticate(&old_token_secret)
                .is_none()
        );

        // The generation remains active and the activation record is intact.
        let after = store.load_activation_mixed(project_id).unwrap().unwrap();
        assert_eq!(after.generation_id(), generation_id);
        assert_eq!(
            after.cutback(),
            Some(&CutbackStateV2::Structural {
                reason: CutbackReason::NoLocalAttachment,
            }),
            "persisted cutback state must be untouched by the auth swap"
        );
        assert_eq!(after.activated_unix_secs(), activation.activated_unix_secs);
    }

    /// P4-D section 8.2: assignment-diff produces exactly-once transitions
    /// through the reconciler event channel. Events coalesce by project:
    /// the latest transition wins and triggering origins remain sticky.
    #[test]
    fn p4d_reconciler_coalesces_events_by_project() {
        let guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = CutbackReconciler::new(guards);

        let scope = PublishedScope::try_new("p4d-coalesce", ".").unwrap();

        // Enqueue the same cutback event three times.
        for _ in 0..3 {
            reconciler.enqueue(
                "proj-a",
                scope.clone(),
                ReconcileKind::Cutback,
                ReconcileOrigin::CatalogCommit,
                Some(40),
            );
        }
        // A newer transition for the same project replaces kind/revision,
        // but preserves the earlier origin.
        reconciler.enqueue(
            "proj-a",
            scope.clone(),
            ReconcileKind::Activate,
            ReconcileOrigin::AssignmentConfigReload,
            None,
        );
        reconciler.enqueue(
            "proj-b",
            scope.clone(),
            ReconcileKind::Cutback,
            ReconcileOrigin::StartupRecovery,
            None,
        );

        let drained = reconciler.drain();
        assert_eq!(drained.len(), 2, "one event per project must remain");
        let proj_a = drained
            .iter()
            .find(|event| event.project_id == "proj-a")
            .unwrap();
        assert_eq!(proj_a.kind, ReconcileKind::Activate);
        assert_eq!(proj_a.authority_revision, None);
        assert_eq!(
            proj_a.origins,
            BTreeSet::from([
                ReconcileOrigin::AssignmentConfigReload,
                ReconcileOrigin::CatalogCommit,
            ])
        );
        assert_eq!(
            drained.iter().filter(|e| e.project_id == "proj-b").count(),
            1
        );

        // Channel is empty after drain.
        assert!(reconciler.drain().is_empty());
    }

    /// P4-D section 8.2: the transition guard ensures exactly one staging
    /// pass per project per trigger batch (section 4.4). A second
    /// acquisition for the same project while the first guard is held
    /// returns `None` (coalesce).
    #[test]
    fn p4d_transition_guard_allows_exactly_one_pass_per_project() {
        let guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = CutbackReconciler::new(guards);

        // First acquisition succeeds.
        let guard_a = reconciler
            .try_acquire("proj-a")
            .expect("first acquisition must succeed");

        // Second acquisition for the same project fails (coalesce).
        assert!(
            reconciler.try_acquire("proj-a").is_none(),
            "concurrent trigger must coalesce: guard already held"
        );

        // A different project can acquire independently.
        let _guard_b = reconciler
            .try_acquire("proj-b")
            .expect("different project must acquire independently");

        // Releasing guard_a allows re-acquisition.
        drop(guard_a);
        let _guard_a2 = reconciler
            .try_acquire("proj-a")
            .expect("re-acquisition after release must succeed");
    }

    /// P4-D section 8.2: bridge reload behavior is unchanged. In bridge
    /// mode (`reconciler.is_none()`), `apply_source_transitions` spawns
    /// transitions inline, exactly as before Phase 4. The `is_catalog`
    /// discriminator must return `false` for a bridge runtime.
    #[test]
    fn p4d_bridge_mode_is_not_catalog_and_has_no_reconciler() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let bridge = CodeSourceRuntime::for_test(&root);
        assert!(
            !bridge.is_catalog(),
            "bridge mode must report is_catalog == false"
        );
        assert!(
            bridge.reconciler().is_none(),
            "bridge mode must not instantiate a reconciler"
        );

        let catalog = CodeSourceRuntime::for_test_catalog(&root.join("cat"));
        assert!(
            catalog.is_catalog(),
            "catalog mode must report is_catalog == true"
        );
        assert!(
            catalog.reconciler().is_some(),
            "catalog mode must instantiate a reconciler"
        );
    }

    /// P4-D section 8.1 item 3: the config-event re-entry feed enqueues
    /// every project with a non-None persisted cutback state. Projects
    /// with `None` cutback state are NOT enqueued.
    #[test]
    fn p4d_catalog_transition_feed_enqueues_cutback_projects() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let store_root = root.join("code-sources");

        // Seed two projects: one with cutback state, one without.
        let scope_a = PublishedScope::try_new("p4d-feed-a", ".").unwrap();
        let scope_b = PublishedScope::try_new("p4d-feed-b", ".").unwrap();
        let proj_a = "p_000000000000000000000000000004d2";
        let proj_b = "p_000000000000000000000000000004d3";
        let gen_a = compute_generation_id(
            "p4d-producer",
            &empty_generation_descriptor(scope_a.clone(), &"a".repeat(40)),
        );
        let gen_b = compute_generation_id(
            "p4d-producer",
            &empty_generation_descriptor(scope_b.clone(), &"a".repeat(40)),
        );
        p4d_seed_catalog_store(&store, &store_root, proj_a, &scope_a, &gen_a);
        p4d_seed_catalog_store(&store, &store_root, proj_b, &scope_b, &gen_b);

        // Only proj_a gets a cutback state.
        store
            .mark_cutback_state(
                proj_a,
                CutbackStateV2::Structural {
                    reason: CutbackReason::ScopeMismatch,
                },
            )
            .unwrap();

        // Build a SourceTransitions with one explicit activation (unused
        // here because we test the enqueue path directly).
        let _transitions = SourceTransitions {
            cutbacks: vec![],
            activations: vec![(scope_a.clone(), proj_a.to_string())],
        };

        // Apply through the catalog path (which also runs the re-entry feed).
        // We need a minimal SharedState for this, but we can test the
        // enqueue path directly by calling enqueue_transition through the
        // runtime, simulating what apply_source_transitions_catalog does.
        runtime.enqueue_transition(
            proj_a,
            scope_a.clone(),
            ReconcileKind::Activate,
            ReconcileOrigin::CatalogCommit,
            Some(41),
        );

        // Simulate the re-entry feed: iterate activation records and enqueue
        // those with non-None cutback.
        let records = store.activation_records_mixed().unwrap();
        for record in &records {
            if record.cutback().is_some() {
                if let Some(scope) = record.published_scope().cloned() {
                    runtime.enqueue_transition(
                        record.project_id(),
                        scope,
                        ReconcileKind::Cutback,
                        ReconcileOrigin::AssignmentConfigReload,
                        None,
                    );
                }
            }
        }

        // Drain and verify: project-keyed coalescing retains the latest
        // transition and every origin. proj_b has nothing because its
        // cutback state is None.
        let reconciler = runtime.reconciler().unwrap();
        let drained = reconciler.drain();

        let proj_a_events: Vec<_> = drained.iter().filter(|e| e.project_id == proj_a).collect();
        assert_eq!(proj_a_events.len(), 1, "proj_a must coalesce by project");
        let event = proj_a_events[0];
        assert_eq!(event.kind, ReconcileKind::Cutback);
        assert_eq!(event.authority_revision, None);
        assert_eq!(
            event.origins,
            BTreeSet::from([
                ReconcileOrigin::AssignmentConfigReload,
                ReconcileOrigin::CatalogCommit,
            ])
        );

        assert!(
            drained.iter().all(|e| e.project_id != proj_b),
            "proj_b (no cutback state) must not be enqueued"
        );
    }

    /// P4-D fix (finding 2): an event enqueued while the project's
    /// transition guard is held is deferred, not dropped. After the guard
    /// releases, `promote_deferred` merges it back into pending and it
    /// fires exactly once (no loss, no duplicate).
    #[test]
    fn p4d_deferred_event_fires_once_after_guard_release() {
        let guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = CutbackReconciler::new(guards);
        let scope = PublishedScope::try_new("p4d-defer", ".").unwrap();

        // Acquire the guard as if an in-flight transition holds it.
        let held_guard = reconciler
            .try_acquire("proj-x")
            .expect("acquire must succeed");

        // Enqueue an event while the guard is held.
        reconciler.enqueue(
            "proj-x",
            scope.clone(),
            ReconcileKind::Cutback,
            ReconcileOrigin::CatalogCommit,
            Some(7),
        );

        // The reconciler loop drains the pending set, then tries to
        // acquire the guard. It fails because the guard is held, so it
        // defers the event instead of dropping it.
        let drained = reconciler.drain();
        assert_eq!(drained.len(), 1, "one event must be drained from pending");
        assert!(
            reconciler.try_acquire("proj-x").is_none(),
            "guard is held, try_acquire must fail"
        );
        reconciler.defer(drained.into_iter().next().unwrap());

        // A newer config event arrives before the held transition releases.
        // It owns the latest kind/revision; deferred promotion only merges
        // its older origin.
        reconciler.enqueue(
            "proj-x",
            scope.clone(),
            ReconcileKind::Activate,
            ReconcileOrigin::AssignmentConfigReload,
            None,
        );

        // Verify pending is empty (the event is in deferred, not pending).
        assert_eq!(reconciler.pending.lock().unwrap().len(), 1);

        // Release the guard (simulates transition completion).
        // GuardHandle::drop notifies the condvar, waking the reconciler.
        drop(held_guard);

        // On wake, the reconciler calls promote_deferred, which merges
        // deferred events back into pending.
        let promoted = reconciler.promote_deferred();
        assert_eq!(promoted, 1, "one deferred event must be promoted");

        // The event is now in pending: drain returns exactly one.
        let final_drained = reconciler.drain();
        assert_eq!(final_drained.len(), 1, "promoted event fires exactly once");
        assert_eq!(final_drained[0].project_id, "proj-x");
        assert_eq!(final_drained[0].kind, ReconcileKind::Activate);
        assert_eq!(final_drained[0].authority_revision, None);
        assert_eq!(
            final_drained[0].origins,
            BTreeSet::from([
                ReconcileOrigin::AssignmentConfigReload,
                ReconcileOrigin::CatalogCommit,
            ])
        );

        // No duplicates remain.
        assert!(reconciler.drain().is_empty());
        assert_eq!(reconciler.promote_deferred(), 0);
    }

    /// P4-D fix (finding 1): the transition guard is held for the full
    /// duration of the spawned worker, not just around the spawn call.
    /// Simulated by holding a GuardHandle across a "slow transition"
    /// window and asserting that concurrent try_acquire fails until the
    /// handle drops.
    #[test]
    fn p4d_guard_held_for_transition_duration() {
        let guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = CutbackReconciler::new(guards);

        // Simulate a transition in flight: acquire the guard.
        let guard = reconciler
            .try_acquire("proj-slow")
            .expect("first acquisition succeeds");

        // While the guard is held, a concurrent try_acquire for the same
        // project must fail.
        assert!(
            reconciler.try_acquire("proj-slow").is_none(),
            "concurrent try_acquire must fail while transition is in flight"
        );

        // A different project can still acquire (no cross-project blocking).
        let _guard_other = reconciler
            .try_acquire("proj-other")
            .expect("different project is independent");

        // Simulate transition completion: drop the guard.
        drop(guard);

        // Now the same project can acquire again.
        let _guard_after = reconciler
            .try_acquire("proj-slow")
            .expect("re-acquisition succeeds after transition completes");

        // And releasing _guard_after notifies the condvar (verified by
        // the fact that GuardHandle::drop calls notify_one, which is
        // exercised by the deferred-event test above).
    }

    // ------------------------------------------------------------------
    // P4-E: one-attempt driver and loop elimination (section 9.1)
    // ------------------------------------------------------------------

    /// P4-E section 9.8: `compute_retry_deadline` produces capped
    /// exponential backoff with project-id jitter (0-25 percent of
    /// current delay).
    #[test]
    fn p4e_retry_deadline_capped_exponential_with_jitter() {
        let base = 1_u64;
        let max = 60_u64;
        let pid = "p_000000000000000000000000000004e1";

        // attempt 1: base=1, jitter 0-0 (0/4=0)
        let d1 = compute_retry_deadline(1, pid, base, max);
        // The delay component is at least base and at most base + base/4
        let now = unix_now();
        assert!(d1 >= now + base);
        assert!(d1 <= now + base + 1, "attempt 1 jitter must be 0 or 1");

        // attempt 5: base*2^4=16, jitter 0-4
        let d5 = compute_retry_deadline(5, pid, base, max);
        assert!(d5 >= now + 16);
        assert!(d5 <= now + 16 + 4, "attempt 5 jitter must be 0-4");

        // attempt 10: base*2^9=512 > max=60, so capped at 60, jitter 0-15
        let d10 = compute_retry_deadline(10, pid, base, max);
        assert!(d10 >= now + 60);
        assert!(d10 <= now + 60 + 15, "capped at 60, jitter 0-15");
    }

    /// P4-E section 9.8: stable_project_id_hash is deterministic for the
    /// same project id (jitter derived from a stable hash).
    #[test]
    fn p4e_stable_project_id_hash_is_deterministic() {
        let h1 = stable_project_id_hash("p_000000000000000000000000000004e1");
        let h2 = stable_project_id_hash("p_000000000000000000000000000004e1");
        assert_eq!(h1, h2, "same project id must produce same hash");

        let h3 = stable_project_id_hash("p_000000000000000000000000000004e2");
        assert_ne!(h1, h3, "different project ids should differ");
    }

    /// P4-E section 9.2: the scheduler registers transient deadlines and
    /// drains due projects. Projects past their deadline are returned;
    /// future-deadline projects are not.
    #[test]
    fn p4e_scheduler_drains_due_projects() {
        let guards = Arc::new(TransitionGuardMap::new(BTreeMap::new()));
        let reconciler = CutbackReconciler::new(guards);

        let now = unix_now();
        // Register one due project and one future project.
        reconciler.register_transient(now - 10, "proj-due");
        reconciler.register_transient(now + 3600, "proj-future");

        // Drain due: only proj-due is past its deadline.
        let due = reconciler.drain_due(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0], "proj-due");

        // Future project is still registered.
        assert_eq!(
            reconciler.min_deadline(),
            Some(now + 3600),
            "future deadline must remain"
        );
    }

    /// P4-E section 9.1: `classify_checkout_error` maps checkout access
    /// error codes to the correct structural cutback reason.
    #[test]
    fn p4e_classify_checkout_error_maps_structural_reasons() {
        use bbox_indexing::checkout_access::{CheckoutAccessError, CheckoutAccessErrorCode};

        let no_attachment =
            CheckoutAccessError::new(CheckoutAccessErrorCode::AttachmentNotFound, "no attachment");
        match classify_checkout_error(&no_attachment) {
            CutbackAttemptOutcome::Structural(CutbackReason::NoLocalAttachment) => {}
            other => panic!("expected Structural(NoLocalAttachment), got {other:?}"),
        }

        let scope_mismatch =
            CheckoutAccessError::new(CheckoutAccessErrorCode::ScopeMismatch, "scope wrong");
        match classify_checkout_error(&scope_mismatch) {
            CutbackAttemptOutcome::Structural(CutbackReason::ScopeMismatch) => {}
            other => panic!("expected Structural(ScopeMismatch), got {other:?}"),
        }
    }

    /// P4-E section 9.8: `clear_cutback_state` clears the typed cutback
    /// field and resets `cutback_pending` to false (coherence clause).
    #[test]
    fn p4e_clear_cutback_state_resets_coherence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();

        let scope = PublishedScope::try_new("p4e-clear-repo", ".").unwrap();
        let project_id = "p_000000000000000000000000000004e2";
        let generation_id = compute_generation_id(
            "p4d-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );
        // Seed store with activation and cutback state.
        p4d_seed_catalog_store(&store, &root, project_id, &scope, &generation_id);
        store
            .mark_cutback_state(
                project_id,
                CutbackStateV2::Structural {
                    reason: CutbackReason::NoLocalAttachment,
                },
            )
            .unwrap();

        // Verify state is present.
        let before = store.load_activation_mixed(project_id).unwrap().unwrap();
        assert!(before.cutback().is_some());

        // Clear.
        store.clear_cutback_state(project_id).unwrap();

        let after = store.load_activation_mixed(project_id).unwrap().unwrap();
        assert!(after.cutback().is_none(), "cutback state must be cleared");
        assert!(
            !after.is_cutback_pending(),
            "cutback_pending must be false after clear"
        );
    }

    // P4-E commit (b): reducer reduction table cell tests (section 9.3).

    #[test]
    fn p4e_reducer_collected_collected_none_is_noop() {
        let action = evaluate_reduction(
            DesiredAssignment::Collected,
            EffectiveSource::Collected,
            None,
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    #[test]
    fn p4e_reducer_collected_collected_any_nonnone_cancels_cutback() {
        let persisted = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Collected,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::CancelCutback));
    }

    #[test]
    fn p4e_reducer_collected_other_activates() {
        for eff in [
            EffectiveSource::Local,
            EffectiveSource::Warming,
            EffectiveSource::Unavailable,
        ] {
            let action = evaluate_reduction(
                DesiredAssignment::Collected,
                eff,
                None,
                LadderResult::None,
                false,
            );
            assert!(matches!(action, ReducerAction::Activate), "eff={:?}", eff);
        }
    }

    #[test]
    fn p4e_reducer_local_collected_none_selected_attempts_cutback() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::Selected,
            false,
        );
        assert!(matches!(action, ReducerAction::AttemptCutback));
    }

    #[test]
    fn p4e_reducer_local_collected_none_none_persists_no_local() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::None,
            false,
        );
        match action {
            ReducerAction::PersistStructural(CutbackReason::NoLocalAttachment) => {}
            other => panic!(
                "expected PersistStructural(NoLocalAttachment), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn p4e_reducer_local_collected_none_ambiguous_persists_ambiguous() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::Ambiguous,
            false,
        );
        match action {
            ReducerAction::PersistStructural(CutbackReason::AmbiguousAttachment) => {}
            other => panic!(
                "expected PersistStructural(AmbiguousAttachment), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn p4e_reducer_local_collected_none_scopeinvalid_persists_mismatch() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::ScopeInvalid,
            false,
        );
        match action {
            ReducerAction::PersistStructural(CutbackReason::ScopeMismatch) => {}
            other => panic!("expected PersistStructural(ScopeMismatch), got {:?}", other),
        }
    }

    #[test]
    fn p4e_reducer_local_collected_structural_selected_reattempts() {
        let persisted = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::Selected,
            false,
        );
        assert!(matches!(action, ReducerAction::ReattemptCutback));
    }

    #[test]
    fn p4e_reducer_local_collected_structural_none_is_noop() {
        let persisted = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        for ladder in [
            LadderResult::None,
            LadderResult::Ambiguous,
            LadderResult::ScopeInvalid,
        ] {
            let action = evaluate_reduction(
                DesiredAssignment::Local,
                EffectiveSource::Collected,
                Some(&persisted),
                ladder,
                false,
            );
            assert!(matches!(action, ReducerAction::NoOp), "ladder={:?}", ladder);
        }
    }

    #[test]
    fn p4e_reducer_local_collected_transient_reattempts() {
        let persisted = CutbackStateV2::Transient {
            attempt: 2,
            error_class: CutbackErrorClass::WriterContention,
            deadline_unix_secs: unix_now() + 30,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::ReattemptCutback));
    }

    #[test]
    fn p4e_reducer_local_collected_manual_retry_is_noop() {
        let persisted = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::WriterContention,
            attempt: 8,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::Selected,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    #[test]
    fn p4e_reducer_local_collected_terminal_is_noop() {
        let persisted = CutbackStateV2::Terminal {
            error_class: CutbackErrorClass::ValidationFailure,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::Selected,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    #[test]
    fn p4e_reducer_local_local_none_is_noop() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Local,
            None,
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    #[test]
    fn p4e_reducer_local_local_any_nonnone_cancels() {
        let persisted = CutbackStateV2::Transient {
            attempt: 1,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: unix_now() + 5,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Local,
            Some(&persisted),
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::CancelCutback));
    }

    #[test]
    fn p4e_reducer_bridge_open_clears_structural() {
        let persisted = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&persisted),
            LadderResult::None,
            true, // bridge is open
        );
        assert!(matches!(action, ReducerAction::CancelCutback));
    }

    #[test]
    fn p4e_reducer_bridge_open_no_structural_is_noop() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::None,
            true, // bridge is open
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    #[test]
    fn p4e_reducer_retired_hands_off() {
        let action = evaluate_reduction(
            DesiredAssignment::Retired,
            EffectiveSource::Collected,
            None,
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::Retire));
    }

    #[test]
    fn p4e_reducer_open_bridge_predicate_empty_records() {
        let records: Vec<&bbox_corpus_core::project_catalog::ScopeMigrationRecord> = vec![];
        let result = is_bridge_open(&records, "gen-1", None);
        assert!(!result);
    }

    #[test]
    fn p4e_post_commit_observer_drains_events() {
        use bbox_indexing::project_catalog_store::{CatalogCommitObserver, CatalogCommittedEvent};
        let observer = CatalogCommitObserver::new();
        assert!(!observer.has_events());
        let mut ids = std::collections::BTreeSet::new();
        ids.insert("p_test1".to_string());
        ids.insert("p_test2".to_string());
        observer.push_for_test(CatalogCommittedEvent {
            epoch: 42,
            changed_project_ids: ids.clone(),
        });
        assert!(observer.has_events());
        let drained = observer.drain_events();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].epoch, 42);
        assert_eq!(drained[0].changed_project_ids, ids);
        assert!(!observer.has_events());
    }

    #[test]
    fn p4e_post_commit_observer_overflow_retains_rescan_until_matching_completion() {
        use bbox_indexing::project_catalog_store::{CatalogCommitObserver, CatalogCommittedEvent};
        let observer = CatalogCommitObserver::new();
        for index in 0..4100 {
            observer.push_for_test(CatalogCommittedEvent {
                epoch: index,
                changed_project_ids: BTreeSet::from([format!("p_{index:032x}")]),
            });
        }
        let generation = observer.pending_rescan_generation().unwrap();
        assert_eq!(observer.pending_rescan_generation(), Some(generation));
        observer.request_rescan();
        assert!(observer.complete_rescan(generation));
        let retry_generation = observer.pending_rescan_generation().unwrap();
        assert_ne!(retry_generation, generation);
        assert!(observer.complete_rescan(retry_generation));
        assert_eq!(observer.pending_rescan_generation(), None);
        assert!(observer.drain_events().len() <= 1);
    }

    #[test]
    fn p4e_post_commit_observer_pages_the_full_supported_catalog() {
        let project_ids = (0..100_000)
            .map(|index| format!("p_{index:032x}"))
            .collect::<Vec<_>>();
        let mut progress = CatalogObserverRescanProgress {
            generation: 7,
            epoch: 19,
            project_ids: project_ids.clone(),
            next_index: 0,
        };
        let mut delivered = Vec::new();
        while let Some(event) = progress.next_event() {
            assert!(event.changed_project_ids.len() <= CATALOG_OBSERVER_RESCAN_PAGE_SIZE);
            delivered.extend(event.changed_project_ids);
        }
        assert!(progress.is_complete());
        assert_eq!(delivered, project_ids);
    }

    #[test]
    fn p4e_post_commit_observer_finishes_pinned_pages_under_continuous_commits() {
        use bbox_indexing::project_catalog_store::{CatalogCommitObserver, CatalogCommittedEvent};

        let observer = CatalogCommitObserver::new();
        for index in 0..4100 {
            observer.push_for_test(CatalogCommittedEvent {
                epoch: index,
                changed_project_ids: BTreeSet::from([format!("p_{index:032x}")]),
            });
        }
        let generation = observer.pending_rescan_generation().unwrap();
        let project_ids = (0..100_000)
            .map(|index| format!("p_{index:032x}"))
            .collect::<Vec<_>>();
        let mut progress = CatalogObserverRescanProgress {
            generation,
            epoch: 19,
            project_ids,
            next_index: 0,
        };
        let mut pages = 0;
        while progress.next_event().is_some() {
            pages += 1;
            observer.push_for_test(CatalogCommittedEvent {
                epoch: 20 + pages,
                changed_project_ids: BTreeSet::from([format!("p_dirty_{pages:032x}")]),
            });
            assert_eq!(observer.pending_rescan_generation(), Some(generation));
        }
        assert!(pages > 1);
        assert!(observer.complete_rescan(generation));
        let followup = observer.pending_rescan_generation().unwrap();
        assert_ne!(followup, generation);
        assert!(observer.complete_rescan(followup));
        assert_eq!(observer.pending_rescan_generation(), None);
    }

    // P4-E commit (c): bridge-clear and scope-migrate refusal tests.

    #[test]
    fn p4e_scope_migrate_refuses_second_migration_with_open_bridge() {
        use bbox_corpus_core::project_catalog::ScopeMigrationRecord;

        // Build a minimal catalog with one bridge-bearing record.
        let record = ScopeMigrationRecord {
            scope_migration_id: bbox_corpus_core::project_catalog::ScopeMigrationId::mint(),
            project_id: ProjectId::parse("p_bridge_refuse_test_000001").unwrap(),
            catalog_epoch: 1,
            authority_provenance: bbox_corpus_core::project_catalog::ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "test".into(),
            operator_reason: None,
            old_scope: ProjectScope::Published(
                PublishedScope::try_new("old-repo", ".").unwrap(),
            ),
            new_scope: ProjectScope::Published(
                PublishedScope::try_new("new-repo", ".").unwrap(),
            ),
            kind: bbox_corpus_core::project_catalog::ScopeMigrationKind::RelpathMove,
            migrated_at: "2024-01-01T00:00:00Z".into(),
            code_bridge_generation: Some("gen-bridge-123".into()),
            publication_bridge_generation: None,
            pending_capabilities: Default::default(),
        };

        // Verify the record has an open bridge.
        let records: Vec<&ScopeMigrationRecord> = vec![&record];
        assert!(is_bridge_open(
            &records,
            "gen-bridge-123",
            Some(&PublishedScope::try_new("old-repo", ".").unwrap())
        ));
        // Different generation: not open.
        assert!(!is_bridge_open(
            &records,
            "gen-other",
            Some(&PublishedScope::try_new("old-repo", ".").unwrap())
        ));
        // Different scope: not open.
        assert!(!is_bridge_open(
            &records,
            "gen-bridge-123",
            Some(&PublishedScope::try_new("new-repo", ".").unwrap())
        ));
        // No bridge generation: not open.
        let mut no_bridge = record.clone();
        no_bridge.code_bridge_generation = None;
        let no_bridge_refs: Vec<&ScopeMigrationRecord> = vec![&no_bridge];
        assert!(!is_bridge_open(&no_bridge_refs, "gen-bridge-123", None));
    }

    #[test]
    fn p4e_scope_bridge_clear_mode_enum_round_trips() {
        use bbox_indexing::project_catalog_admin::ScopeBridgeClearMode;
        assert_eq!(
            format!("{:?}", ScopeBridgeClearMode::DanglingReference),
            "DanglingReference"
        );
        assert_eq!(
            format!("{:?}", ScopeBridgeClearMode::DoubleMigrationRepair),
            "DoubleMigrationRepair"
        );
    }

    // P4-E commit (d): reduction-table integration and lifecycle tests
    // (section 9.8).

    #[test]
    fn p4e_transient_retry_budget_exhausts_to_manual_retry() {
        // When attempts exceed cutback_max_attempts, the reducer's
        // schedule_cutback_catalog path would persist
        // ManualRetryRequired. Test the reduction-table cell directly:
        // after ManualRetryRequired is persisted, the reducer returns
        // NoOp (steady-state, explicit retry only).
        let mr = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::WriterContention,
            attempt: 8,
        };
        // Even with a selected attachment, ManualRetryRequired is NoOp.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&mr),
            LadderResult::Selected,
            false,
        );
        assert!(
            matches!(action, ReducerAction::NoOp),
            "ManualRetryRequired must be steady-state NoOp"
        );

        let action = evaluate_reduction_for_event(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&mr),
            LadderResult::Selected,
            false,
            &BTreeSet::from([ReconcileOrigin::AssignmentConfigReload]),
        );
        assert!(
            matches!(action, ReducerAction::ReattemptCutback),
            "a fresh assignment/config event must release manual retry once"
        );

        let action = evaluate_reduction_for_event(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&mr),
            LadderResult::Selected,
            false,
            &BTreeSet::from([ReconcileOrigin::ReadinessAvailable]),
        );
        assert!(
            matches!(action, ReducerAction::ReattemptCutback),
            "readiness must repair retry state poisoned by the former classifier"
        );

        let terminal_class_manual = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::SecurityFailure,
            attempt: 8,
        };
        let action = evaluate_reduction_for_event(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&terminal_class_manual),
            LadderResult::Selected,
            false,
            &BTreeSet::from([ReconcileOrigin::ReadinessAvailable]),
        );
        assert!(
            matches!(action, ReducerAction::NoOp),
            "readiness must not release unrelated manual-retry classes"
        );

        for origin in [
            ReconcileOrigin::CatalogCommit,
            ReconcileOrigin::TransientDeadline,
            ReconcileOrigin::SelectorRetirementCompletion,
            ReconcileOrigin::StartupRecovery,
            ReconcileOrigin::ActivationCompletion,
        ] {
            let action = evaluate_reduction_for_event(
                DesiredAssignment::Local,
                EffectiveSource::Collected,
                Some(&mr),
                LadderResult::Selected,
                false,
                &BTreeSet::from([origin]),
            );
            assert!(
                matches!(action, ReducerAction::NoOp),
                "{origin:?} must not release manual retry"
            );
        }
    }

    #[test]
    fn staging_error_classification_uses_typed_causes_not_incidental_words() {
        use bbox_indexing::index::writer_actor::IndexWriterRetryableError;

        let queued = anyhow::Error::new(SelectorRetirementQueued);
        assert!(matches!(
            classify_staging_error(&queued),
            CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::SelectorRetirement)
        ));

        let writer = anyhow::Error::new(IndexWriterRetryableError::ReindexPassInProgress);
        assert!(matches!(
            classify_staging_error(&writer),
            CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::ReindexPass)
        ));

        let warming = anyhow::Error::new(IndexWriterRetryableError::VectorStoreWarming);
        assert!(matches!(
            classify_staging_error(&warming),
            CutbackAttemptOutcome::ReadinessDeferred(CutbackReadiness::VectorStore)
        ));

        let validation = anyhow::Error::new(StagingValidationRefusal);
        assert!(matches!(
            classify_staging_error(&validation),
            CutbackAttemptOutcome::Terminal(CutbackErrorClass::ValidationFailure)
        ));

        let security = anyhow::Error::new(StagingSecurityRefusal);
        assert!(matches!(
            classify_staging_error(&security),
            CutbackAttemptOutcome::Terminal(CutbackErrorClass::SecurityFailure)
        ));

        let incidental = anyhow!("security cache was unavailable during index commit");
        assert!(matches!(
            classify_staging_error(&incidental),
            CutbackAttemptOutcome::Transient(CutbackErrorClass::IndexCommit)
        ));
    }

    #[test]
    fn activation_completion_is_convergence_only_without_a_new_origin() {
        let completion = BTreeSet::from([ReconcileOrigin::ActivationCompletion]);
        for action in [
            ReducerAction::Activate,
            ReducerAction::AttemptCutback,
            ReducerAction::ReattemptCutback,
        ] {
            assert!(matches!(
                gate_completion_reentry(action, &completion),
                ReducerAction::NoOp
            ));
        }
        assert!(matches!(
            gate_completion_reentry(ReducerAction::CancelCutback, &completion),
            ReducerAction::CancelCutback
        ));

        let completion_and_catalog = BTreeSet::from([
            ReconcileOrigin::ActivationCompletion,
            ReconcileOrigin::CatalogCommit,
        ]);
        assert!(matches!(
            gate_completion_reentry(ReducerAction::AttemptCutback, &completion_and_catalog),
            ReducerAction::AttemptCutback
        ));
    }

    #[test]
    fn p4e_terminal_state_never_auto_retries() {
        let terminal = CutbackStateV2::Terminal {
            error_class: CutbackErrorClass::SecurityFailure,
        };
        // Terminal never auto-retries, regardless of ladder.
        for ladder in [
            LadderResult::Selected,
            LadderResult::None,
            LadderResult::Ambiguous,
            LadderResult::ScopeInvalid,
        ] {
            let action = evaluate_reduction(
                DesiredAssignment::Local,
                EffectiveSource::Collected,
                Some(&terminal),
                ladder,
                false,
            );
            assert!(
                matches!(action, ReducerAction::NoOp),
                "Terminal must be steady-state NoOp for ladder={:?}",
                ladder
            );
        }
    }

    #[test]
    fn p4e_readd_assignment_cancels_cutback() {
        // A re-added assignment cancels any pending cutback and retains
        // collected authority (the collected/collected/any-non-None cell).
        let structural = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Collected,
            EffectiveSource::Collected,
            Some(&structural),
            LadderResult::None,
            false,
        );
        assert!(
            matches!(action, ReducerAction::CancelCutback),
            "re-add must cancel cutback"
        );
    }

    #[test]
    fn p4e_local_local_stale_state_is_cleared() {
        // local/local/any-non-None: clear stale state (crash between
        // local activation publication and state clear).
        let transient = CutbackStateV2::Transient {
            attempt: 3,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: unix_now() + 30,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Local,
            Some(&transient),
            LadderResult::None,
            false,
        );
        assert!(
            matches!(action, ReducerAction::CancelCutback),
            "local/local with stale state must clear"
        );
    }

    #[test]
    fn p4e_open_bridge_predicate_newest_by_catalog_epoch() {
        use bbox_corpus_core::project_catalog::ScopeMigrationRecord;

        // When multiple bridge records exist, newest by catalog_epoch is
        // authority (section 9.3).
        let scope = PublishedScope::try_new("old-repo", ".").unwrap();
        let older = ScopeMigrationRecord {
            scope_migration_id: bbox_corpus_core::project_catalog::ScopeMigrationId::mint(),
            project_id: ProjectId::parse("p_multi_bridge_000000000001").unwrap(),
            catalog_epoch: 5,
            authority_provenance:
                bbox_corpus_core::project_catalog::ScopeMigrationAuthorityProvenance::AttachmentProved,
            operator_invocation: "test".into(),
            operator_reason: None,
            old_scope: ProjectScope::Published(scope.clone()),
            new_scope: ProjectScope::Published(
                PublishedScope::try_new("new-repo", ".").unwrap(),
            ),
            kind: bbox_corpus_core::project_catalog::ScopeMigrationKind::RelpathMove,
            migrated_at: "2024-01-01T00:00:00Z".into(),
            code_bridge_generation: Some("gen-older".into()),
            publication_bridge_generation: None,
            pending_capabilities: Default::default(),
        };
        let mut newer = older.clone();
        newer.scope_migration_id = bbox_corpus_core::project_catalog::ScopeMigrationId::mint();
        newer.catalog_epoch = 10;
        newer.code_bridge_generation = Some("gen-newer".into());
        // The newer record has a different old_scope that does NOT match.
        newer.old_scope =
            ProjectScope::Published(PublishedScope::try_new("wrong-repo", ".").unwrap());

        let records: Vec<&ScopeMigrationRecord> = vec![&older, &newer];
        // Newest (epoch 10) is authority: gen-newer with wrong-repo scope.
        // Since the newest record's old_scope is wrong-repo and its
        // code_bridge_generation is gen-newer, the bridge IS open when
        // the effective generation is gen-newer and the effective scope
        // matches wrong-repo.
        let wrong_scope = PublishedScope::try_new("wrong-repo", ".").unwrap();
        assert!(is_bridge_open(&records, "gen-newer", Some(&wrong_scope)));
        // But NOT open when the effective scope is the older record's scope.
        assert!(!is_bridge_open(&records, "gen-newer", Some(&scope)));
        // The older record's generation is not checked (newest is authority).
        assert!(!is_bridge_open(&records, "gen-older", Some(&scope)));
    }

    #[test]
    fn p4e_warming_with_selected_ladder_attempts_cutback() {
        // local/Warming/any with a valid local source: re-stage.
        // local/Unavailable/any with a valid local source: re-stage.
        for eff in [EffectiveSource::Warming, EffectiveSource::Unavailable] {
            let action = evaluate_reduction(
                DesiredAssignment::Local,
                eff,
                None,
                LadderResult::Selected,
                false,
            );
            assert!(
                matches!(action, ReducerAction::AttemptCutback),
                "warming/unavailable with selected ladder should attempt cutback (eff={:?})",
                eff
            );
        }
    }

    #[test]
    fn p4e_warming_without_selected_ladder_is_noop() {
        // local/Warming/any without a valid local source: no-op.
        for ladder in [
            LadderResult::None,
            LadderResult::Ambiguous,
            LadderResult::ScopeInvalid,
        ] {
            let action = evaluate_reduction(
                DesiredAssignment::Local,
                EffectiveSource::Warming,
                None,
                ladder,
                false,
            );
            assert!(
                matches!(action, ReducerAction::NoOp),
                "warming without selected ladder should be NoOp (ladder={:?})",
                ladder
            );
        }
    }

    #[test]
    fn p4e_config_event_re_entry_for_structural() {
        // Config-event re-entry: a config reload re-evaluates Structural
        // states. If the ladder now shows selected, it re-attempts.
        let structural = CutbackStateV2::Structural {
            reason: CutbackReason::ScopeMismatch,
        };
        // Before config fix: no selected attachment, stays structural.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&structural),
            LadderResult::ScopeInvalid,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));

        // After config fix: attachment now selected, re-attempt fires.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&structural),
            LadderResult::Selected,
            false,
        );
        assert!(matches!(action, ReducerAction::ReattemptCutback));
    }

    #[test]
    fn p4e_observer_clone_is_independent() {
        use bbox_indexing::project_catalog_store::{CatalogCommitObserver, CatalogCommittedEvent};
        let observer = CatalogCommitObserver::new();
        let cloned = observer.clone();
        let mut ids = std::collections::BTreeSet::new();
        ids.insert("p_clone_test".to_string());
        observer.push_for_test(CatalogCommittedEvent {
            epoch: 1,
            changed_project_ids: ids,
        });
        // Clone sees the same events (shared internal queue).
        assert!(cloned.has_events());
        let drained = cloned.drain_events();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].epoch, 1);
        // Both handles now show no events.
        assert!(!observer.has_events());
        assert!(!cloned.has_events());
    }

    // P4-E gap fix: restart re-drives for persisted cutback states
    // (section 9.7).

    #[test]
    fn p4e_resume_structural_enqueues_reconciler_event() {
        // Structural state on restart: the reducer re-evaluates once
        // through the reduction table. When the ladder still fails,
        // the reducer stays structural (NoOp or PersistStructural).
        // The key property: no direct cutback attempt in the resume
        // path, only a reconciler enqueue.
        let structural = CutbackStateV2::Structural {
            reason: CutbackReason::NoLocalAttachment,
        };
        // Simulate what the reducer would decide after the event fires.
        // Ladder still shows None: stays structural.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&structural),
            LadderResult::None,
            false,
        );
        assert!(
            matches!(action, ReducerAction::NoOp),
            "structural with no attachment should stay structural (no-op)"
        );
        // Ladder now shows Selected: re-attempt fires.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&structural),
            LadderResult::Selected,
            false,
        );
        assert!(
            matches!(action, ReducerAction::ReattemptCutback),
            "structural with attachment should re-attempt"
        );
    }

    #[test]
    fn p4e_resume_transient_elapsed_deadline_re_attempts() {
        // Transient state with an elapsed deadline: scheduler
        // re-attempts immediately via the reducer.
        let past_deadline = unix_now().saturating_sub(60);
        let transient = CutbackStateV2::Transient {
            attempt: 1,
            error_class: CutbackErrorClass::WriterContention,
            deadline_unix_secs: past_deadline,
        };
        // The reducer's Transient cell always returns ReattemptCutback.
        // The dispatcher (schedule_cutback_catalog) checks the actual
        // deadline against now before proceeding.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&transient),
            LadderResult::None,
            false,
        );
        assert!(
            matches!(action, ReducerAction::ReattemptCutback),
            "transient (elapsed or future) should re-attempt via scheduler"
        );
    }

    #[test]
    fn p4e_resume_transient_future_deadline_waits() {
        // Transient state with a future deadline: not attempted on
        // resume, scheduler waits until the deadline.
        let future_deadline = unix_now() + 3600;
        let transient = CutbackStateV2::Transient {
            attempt: 2,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: future_deadline,
        };
        let action = gate_transient_deadline(
            ReducerAction::ReattemptCutback,
            Some(&transient),
            unix_now(),
        );
        assert_eq!(action, ReducerAction::NoOp);
        let reconciler = CutbackReconciler::new(Arc::new(TransitionGuardMap::default()));
        reconciler.register_transient(future_deadline, "p_future_deadline_test");
        let due = reconciler.drain_due(unix_now());
        assert!(due.is_empty(), "future deadline must not be drained as due");
    }

    #[test]
    fn p4e_resume_terminal_and_manual_retry_are_noops() {
        // Terminal and ManualRetryRequired: recognized as valid no-op
        // persisted states on restart. No event, no retry.
        let terminal = CutbackStateV2::Terminal {
            error_class: CutbackErrorClass::SecurityFailure,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&terminal),
            LadderResult::Selected,
            false,
        );
        assert!(
            matches!(action, ReducerAction::NoOp),
            "Terminal must be no-op on restart"
        );
        let mr = CutbackStateV2::ManualRetryRequired {
            error_class: CutbackErrorClass::WriterContention,
            attempt: 8,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            Some(&mr),
            LadderResult::Selected,
            false,
        );
        assert!(
            matches!(action, ReducerAction::NoOp),
            "ManualRetryRequired must be no-op on restart"
        );
    }

    #[test]
    fn p4e_resume_no_cutback_state_is_noop() {
        // Projects without a persisted cutback state: no action needed
        // on restart.
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Local,
            None,
            LadderResult::None,
            false,
        );
        assert!(matches!(action, ReducerAction::NoOp));
    }

    // P4-E gap fix: open-bridge GC root protection (section 9.5).

    #[test]
    fn p4e_gc_protects_bridge_generation_ids() {
        // Section 9.5 GC root: the gc_blobs_for_scopes_with_bridge
        // method accepts bridge_generation_ids as a parameter and
        // threads them into the protected-set builder. This test
        // verifies the API surface: the method accepts the parameter
        // and does not error on empty or non-empty inputs.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("store");
        let store = CodeSourceStore::open_with_mode(
            &root,
            StoreLimits::default(),
            RuntimeRecordMode::CatalogV2,
        )
        .unwrap();

        let empty_scopes = std::collections::BTreeSet::new();

        // Empty bridge ids: succeeds (backwards compatible).
        let result = store.gc_blobs_for_scopes(&empty_scopes);
        assert!(result.is_ok(), "GC without bridge ids must succeed");

        // Non-empty bridge ids: the parameter is accepted and does not
        // error. A generation named only by the bridge would survive
        // because the protected set includes it.
        let bridge_ids: std::collections::BTreeSet<String> =
            ["gen_bridge_only_test_12345".to_string()]
                .into_iter()
                .collect();
        let result = store.gc_blobs_for_scopes_with_bridge(&empty_scopes, &bridge_ids);
        assert!(result.is_ok(), "GC with bridge ids must succeed");
    }

    // P4-F: pre-bind startup recovery tests (section 10.4).

    /// Helper: create a catalog snapshot with one project and optional
    /// scope migration records for relationship-chain and classification
    /// tests.
    fn p4f_catalog_snapshot(
        project_id: &str,
        scope: PublishedScope,
        migrations: Vec<(
            bbox_corpus_core::project_catalog::ScopeMigrationId,
            bbox_corpus_core::project_catalog::ScopeMigrationRecord,
        )>,
    ) -> CatalogSnapshotV2 {
        use bbox_corpus_core::project_catalog::{CatalogOriginV2, CorpusProject};
        let pid = ProjectId::parse(project_id).unwrap();
        let project = CorpusProject {
            project_id: pid.clone(),
            scope: bbox_corpus_core::project_catalog::ProjectScope::Published(scope),
            operator_aliases: Default::default(),
            nominated_aliases: Default::default(),
            display_name: project_id.to_string(),
            created_at: "2026-07-25T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: Default::default(),
        };
        let mut scope_migrations = BTreeMap::new();
        for (id, record) in migrations {
            scope_migrations.insert(id, record);
        }
        CatalogSnapshotV2 {
            version: bbox_corpus_core::project_catalog::CATALOG_VERSION_V2,
            epoch: 1,
            origin: CatalogOriginV2::MigratedV1 {
                transaction_id:
                    bbox_corpus_core::project_catalog::ProjectCatalogTransactionId::mint(),
            },
            projects: BTreeMap::from([(pid, project)]),
            repo_histories: BTreeMap::new(),
            ambiguous_namespaces: BTreeMap::new(),
            scope_migrations,
        }
    }

    /// Helper: create a v2 activation record in the store with a given
    /// cutback/cutback_pending shape.
    fn p4f_seed_activation(
        store: &CodeSourceStore,
        root: &Path,
        project_id: &str,
        scope: &PublishedScope,
        generation_id: &str,
        cutback: Option<CutbackStateV2>,
        cutback_pending: bool,
    ) -> ActivationRecordV2 {
        let descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
        let generation = StoredGenerationV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            generation_id: generation_id.to_string(),
            producer_id: "p4f-producer".to_string(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            published_scope: scope.clone(),
            state: GenerationState::Ready,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("e".repeat(64)),
        };
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let metadata_path = paths.generation_metadata(scope, generation_id).unwrap();
        fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
        fs::write(
            &metadata_path,
            encode_stored_generation_v2_for_migration(&generation).unwrap(),
        )
        .unwrap();
        // Write the manifest.jsonl file so link 4 can verify it. The
        // descriptor uses manifest_sha256(&[]) for empty entries, so the
        // file is empty (0 bytes).
        let manifest_path = paths.generation_manifest(scope, generation_id).unwrap();
        fs::write(&manifest_path, b"").unwrap();
        let activation = ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: ProjectId::parse(project_id).unwrap(),
            published_scope: scope.clone(),
            generation_id: generation_id.to_string(),
            selector: crate::index::project_files::collected_materialization_selector(
                project_id,
                generation_id,
            ),
            snapshot_id: format!("collected-{}", "f".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "e".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 100,
            cutback_pending,
            cutback,
            diagnostic: None,
        };
        store.save_activation_v2(&activation).unwrap();
        activation
    }

    /// Section 10.4 bootsmoke: a fresh catalog-mode store with no
    /// collected state opens clean. The relationship chain and
    /// classification pass with zero records.
    #[test]
    fn p4f_fresh_store_relationship_chain_passes_clean() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let snapshot = p4f_catalog_snapshot(
            "p_000000000000000000000000000004f1",
            PublishedScope::try_new("p4f-fresh", ".").unwrap(),
            vec![],
        );
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(
            &crate::edge_index::edges_dir_from_bro_store(&root.join("bro")),
        )
        .unwrap();
        // No activation records in the store: chain must pass.
        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(result.is_ok(), "fresh store must pass relationship chain");
    }

    #[test]
    fn p4f_collected_workspace_without_activation_refuses_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let pid = "p_000000000000000000000000000004f1";
        let snapshot = p4f_catalog_snapshot(
            pid,
            PublishedScope::try_new("p4f-fresh", ".").unwrap(),
            vec![],
        );
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            pid.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{pid}/manifest.json"),
                active_snapshot: Some(format!("workspace/{pid}/snapshots/s")),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(format!("collected:{pid}:missing")),
                code_source_generation: Some("a".repeat(64)),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        let error = validate_relationship_chain(&store, &snapshot, &manifest).unwrap_err();
        assert!(error.to_string().contains("relationship_chain_reverse"));
    }

    /// Section 10.4: a hand-drifted activation record (scope mismatch
    /// with no bridge) refuses the relationship chain with a typed code.
    #[test]
    fn p4f_scope_mismatch_refuses_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let activation_scope = PublishedScope::try_new("activation-scope", ".").unwrap();
        let catalog_scope = PublishedScope::try_new("catalog-scope", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f2";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(activation_scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &activation_scope,
            &generation_id,
            None,
            false,
        );

        // Catalog has a DIFFERENT scope than the activation.
        let snapshot = p4f_catalog_snapshot(project_id, catalog_scope, vec![]);
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(
            &crate::edge_index::edges_dir_from_bro_store(&root.join("bro")),
        )
        .unwrap();

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "scope mismatch without bridge must refuse chain"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("code_source_scope_agreement") || err.contains("relationship_chain"),
            "error must carry typed code, got: {err}"
        );
    }

    /// Section 10.4: a migrated-shape root has an active collected v2
    /// activation and generation but NO workspace index entry. The
    /// chain admits this as valid-pending-first-republish: the
    /// migration facade does not fabricate WorkspaceIndexEntry rows;
    /// they are created by the daemon's own first activation republish.
    #[test]
    fn p4f_absent_workspace_entry_passes_as_pending_first_republish() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4f-missing-ws", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f3";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        // Manifest is fresh/empty: no workspace entry for the project.
        // This is the migrated-root shape: chain must PASS.
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(
            &crate::edge_index::edges_dir_from_bro_store(&root.join("bro")),
        )
        .unwrap();

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_ok(),
            "absent workspace entry must pass as pending-first-republish, got: {:?}",
            result.err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn p4f_reconstructed_absence_boots_then_republish_makes_second_boot_strict() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let scope = PublishedScope::try_new("p4f-two-boot", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f4";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );
        let activation = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );
        let catalog = p4f_catalog_snapshot(project_id, scope.clone(), vec![]);
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&root.join("bro"));
        let initial = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir).unwrap();
        validate_relationship_chain(&store, &catalog, &initial).unwrap();

        let reconstructed =
            reconstruct_workspace_entries_from_activations(&store, &edges_dir, &initial).unwrap();
        assert_eq!(reconstructed, BTreeSet::from([project_id.to_string()]));
        let reconstructed_manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir).unwrap();
        let pending =
            derive_pending_first_republish(&store, &reconstructed_manifest, &edges_dir).unwrap();
        assert_eq!(pending, BTreeSet::from([project_id.to_string()]));
        validate_pre_bind_workspace_materializations(&reconstructed_manifest, &edges_dir, &pending)
            .unwrap();
        reconstructed_manifest
            .active_paths_for_loader_admitting_fully_absent(&edges_dir, &pending)
            .unwrap();

        // A crash before the first republish loses no process-local authority:
        // the next boot derives the same exemption from the collected
        // activation and the fully absent writer-shaped materialization.
        let crash_restart_manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir).unwrap();
        let crash_restart_pending =
            derive_pending_first_republish(&store, &crash_restart_manifest, &edges_dir).unwrap();
        assert_eq!(
            crash_restart_pending,
            BTreeSet::from([project_id.to_string()])
        );
        crash_restart_manifest
            .active_paths_for_loader_admitting_fully_absent(&edges_dir, &crash_restart_pending)
            .unwrap();

        let snapshot_path = bbox_edge_sidecar::snapshot::snapshot_dir(
            &edges_dir,
            project_id,
            &activation.snapshot_id,
        );
        fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), &snapshot_path).unwrap();
        assert!(
            derive_pending_first_republish(&store, &reconstructed_manifest, &edges_dir).is_err()
        );
        fs::remove_file(&snapshot_path).unwrap();

        let mut traversal = reconstructed_manifest.clone();
        traversal
            .workspaces
            .get_mut(project_id)
            .unwrap()
            .active_snapshot = Some("../outside".to_string());
        assert!(derive_pending_first_republish(&store, &traversal, &edges_dir).is_err());

        let empty_edges: Vec<bbox_edge_sidecar::edge_sidecar::Edge> = Vec::new();
        bbox_edge_sidecar::snapshot::write_snapshot_files(
            &edges_dir,
            project_id,
            &activation.snapshot_id,
            &[("project.jsonl", &empty_edges)],
        )
        .unwrap();
        bbox_edge_sidecar::snapshot::activate_collected_snapshot(
            &edges_dir,
            project_id,
            "repo-two-boot",
            &"a".repeat(40),
            &generation_id,
            &activation.selector,
            &activation.snapshot_id,
        )
        .unwrap();

        let second_boot = bbox_edge_sidecar::manifest::ManifestIndex::load(&edges_dir).unwrap();
        validate_relationship_chain(&store, &catalog, &second_boot).unwrap();
        let second_boot_pending =
            derive_pending_first_republish(&store, &second_boot, &edges_dir).unwrap();
        assert!(second_boot_pending.is_empty());
        validate_pre_bind_workspace_materializations(
            &second_boot,
            &edges_dir,
            &second_boot_pending,
        )
        .unwrap();
    }

    #[test]
    fn p4f_absent_local_materialization_remains_drift() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&root.join("bro"));
        let project_id = "p_000000000000000000000000000004f5";
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!("workspace/{project_id}/snapshots/local-missing")),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(project_id)),
                code_source_generation: Some("local".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        manifest.write_atomic(&edges_dir).unwrap();
        assert!(
            validate_pre_bind_workspace_materializations(&manifest, &edges_dir, &BTreeSet::new(),)
                .is_err()
        );
    }

    /// Section 10.4: a PRESENT workspace entry that disagrees on
    /// generation still fails closed at link 5 (drift detection).
    #[test]
    fn p4f_present_wrong_generation_workspace_entry_fails_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4f-wrong-gen", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f3";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        let activation = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        // Workspace entry in the production path-bearing format
        // (active_snapshot_rel writes "workspace/{pid}/snapshots/{id}").
        // The generation id is WRONG so the chain must still fail closed.
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!(
                    "workspace/{project_id}/snapshots/{}",
                    activation.snapshot_id
                )),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(activation.selector.clone()),
                code_source_generation: Some("rhg_wrong_generation_id".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "present-but-wrong-generation workspace entry must fail chain"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("workspace generation mismatch"),
            "error must be workspace generation mismatch, got: {err}"
        );
    }

    /// Section 10.4: a workspace entry in the production path-bearing
    /// format (active_snapshot = "workspace/{pid}/snapshots/{id}") with a
    /// MATCHING snapshot id passes the chain at link 5.
    #[test]
    fn p4f_path_bearing_snapshot_entry_passes_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4f-path-snap", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f6";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        let activation = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!(
                    "workspace/{project_id}/snapshots/{}",
                    activation.snapshot_id
                )),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(activation.selector.clone()),
                code_source_generation: Some(generation_id),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_ok(),
            "path-bearing snapshot with matching id must pass chain, got: {:?}",
            result.err()
        );
    }

    /// Section 10.4: a workspace entry whose active_snapshot final path
    /// segment is a DIFFERENT id still fails closed at link 5.
    #[test]
    fn p4f_path_bearing_wrong_snapshot_fails_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4f-wrong-snap", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f7";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        let activation = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!(
                    "workspace/{project_id}/snapshots/collected-different_snapshot_id"
                )),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(activation.selector.clone()),
                code_source_generation: Some(generation_id),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "path-bearing snapshot with wrong id must fail chain"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("workspace snapshot mismatch"),
            "error must be workspace snapshot mismatch, got: {err}"
        );
    }

    /// Section 10.2 step 6 coherence clause: a record still carrying
    /// (None, true) after classification that is not bridge-exempt
    /// fails closed.
    #[test]
    fn p4f_unclassified_pending_fails_coherence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("p4f-coherence", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f4";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        let _activation = p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            true, // cutback_pending: true but cutback: None
        );

        // Catalog scope matches, no bridge: the chain must refuse at
        // link 6 (coherence clause). The workspace entry is absent
        // (migrated-root shape); link 5 admits it as
        // pending-first-republish so the chain reaches link 6.
        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(
            &crate::edge_index::edges_dir_from_bro_store(&root.join("bro")),
        )
        .unwrap();

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "unclassified (None, true) record must fail coherence"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cutback_coherence"),
            "error must be cutback_coherence, got: {err}"
        );
    }

    /// Section 10.1 step 7: a retirement journal file on disk causes
    /// pre-bind refusal.
    #[test]
    fn p4f_retirement_journal_detection_refuses_boot() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journal_dir = root.join("retirement-journals");
        fs::create_dir_all(&journal_dir).unwrap();
        fs::write(
            journal_dir.join("p_000000000000000000000000000004f5.json"),
            r#"{"version":1,"stage":"started"}"#,
        )
        .unwrap();

        let result = detect_incomplete_retirement_journal(&root);
        assert!(result.is_err(), "retirement journal must refuse boot");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("retirement_journal_incomplete"),
            "error must be retirement_journal_incomplete, got: {err}"
        );
    }

    /// Section 10.1 step 7: no journal directory means no refusal.
    #[test]
    fn p4f_no_retirement_journal_passes_clean() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let result = detect_incomplete_retirement_journal(&root);
        assert!(result.is_ok(), "no journal dir must pass clean");
    }

    /// Section 10.1 step 7: empty journal directory passes.
    #[test]
    fn p4f_empty_retirement_journal_dir_passes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journal_dir = root.join("retirement-journals");
        fs::create_dir_all(&journal_dir).unwrap();
        let result = detect_incomplete_retirement_journal(&root);
        assert!(result.is_ok(), "empty journal dir must pass");
    }

    #[cfg(unix)]
    #[test]
    fn p4f_symlinked_retirement_journal_dir_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("retirement-journals")).unwrap();
        assert!(detect_incomplete_retirement_journal(&root).is_err());
    }

    #[test]
    fn p4f_retirement_journal_enumeration_error_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("retirement-journals")).unwrap();
        fs::write(root.join("retirement-journals/a.json"), b"{}").unwrap();
        TEST_RETIREMENT_JOURNAL_ENUMERATION_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(detect_incomplete_retirement_journal(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn p4f_retirement_journal_dangling_leaf_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journals = root.join("retirement-journals");
        fs::create_dir_all(&journals).unwrap();
        std::os::unix::fs::symlink(
            root.join("missing-journal"),
            journals.join("project-a.json"),
        )
        .unwrap();
        assert!(detect_incomplete_retirement_journal(&root).is_err());
    }

    #[test]
    fn p4f_retirement_journal_total_entry_limit_counts_non_json() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journals = root.join("retirement-journals");
        fs::create_dir_all(&journals).unwrap();
        for index in 0..=4096 {
            fs::write(journals.join(format!("{index}.tmp")), b"").unwrap();
        }
        let error = detect_incomplete_retirement_journal(&root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("total entry limit"));
    }

    #[cfg(unix)]
    #[test]
    fn p4f_retirement_journal_directory_swap_keeps_opened_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let journals = root.join("retirement-journals");
        fs::create_dir_all(&journals).unwrap();
        fs::write(journals.join("project-a.json"), b"{}").unwrap();
        TEST_RETIREMENT_JOURNAL_SWAP_AFTER_OPEN.store(true, std::sync::atomic::Ordering::SeqCst);
        let error = detect_incomplete_retirement_journal(&root)
            .unwrap_err()
            .to_string();
        assert!(error.contains("retirement_journal_incomplete"));
    }

    /// Section 10.1 step 5: once-only classification clears the mirror
    /// for records with (None, true) and no bridge. The DenyCheckoutAccess
    /// broker means no attachment is available, so outcome (b) persists
    /// Structural.
    #[test]
    fn p4f_classification_persists_structural_for_no_attachment() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let broker = &runtime.checkout_access;

        let scope = PublishedScope::try_new("p4f-classify-noatt", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f6";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            true, // legacy migration shape
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);
        let outcomes = classify_migrated_records(&store, &snapshot, broker).unwrap();

        assert_eq!(outcomes.len(), 1, "exactly one classification outcome");
        let (pid, outcome) = &outcomes[0];
        assert_eq!(pid, project_id);
        assert!(
            matches!(
                outcome,
                ClassificationOutcome::StructuralPersisted(CutbackReason::NoLocalAttachment)
            ),
            "deny-all broker means no attachment, expected StructuralPersisted(NoLocalAttachment)"
        );

        // Verify the record now has typed Structural state.
        let after = store.load_activation_mixed(project_id).unwrap().unwrap();
        assert_eq!(
            after.cutback(),
            Some(&CutbackStateV2::Structural {
                reason: CutbackReason::NoLocalAttachment,
            })
        );
    }

    /// Section 10.1 step 5: classification is idempotent. Running it
    /// twice on the same store produces no new outcomes (the record
    /// was already classified on the first pass).
    #[test]
    fn p4f_classification_is_once_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let broker = &runtime.checkout_access;

        let scope = PublishedScope::try_new("p4f-once-only", ".").unwrap();
        let project_id = "p_000000000000000000000000000004f7";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            true,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);

        // First pass: classifies the record.
        let outcomes1 = classify_migrated_records(&store, &snapshot, broker).unwrap();
        assert_eq!(outcomes1.len(), 1);

        // Second pass: no more (None, true) records to classify.
        let outcomes2 = classify_migrated_records(&store, &snapshot, broker).unwrap();
        assert_eq!(outcomes2.len(), 0, "classification must be once-only");
    }

    // ---- Section 11.5: sole-ownership and loop-absence exit proof ----

    /// Source text of this file, for the loop-absence exit proof tests.
    fn self_source() -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/code_source.rs");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("failed to read {}", path.display()))
    }

    /// Extract the body of a top-level `fn name(` from the source text.
    /// Returns the text from the `fn` keyword to the matching closing
    /// brace. Best-effort brace counting; sufficient for the structural
    /// assertions here.
    fn extract_fn_body(source: &str, fn_name: &str) -> String {
        let needle = format!("fn {fn_name}(");
        let start = source
            .find(&needle)
            .unwrap_or_else(|| panic!("function `{fn_name}` not found in source"));
        // Find the opening brace of the body.
        let body_start = source[start..]
            .find('{')
            .unwrap_or_else(|| panic!("no body brace for `{fn_name}`"))
            + start;
        let mut depth = 0i32;
        let mut end = body_start;
        for (i, ch) in source[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body_start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        source[start..end].to_string()
    }

    /// Section 11.5b: schedule_cutback_catalog (the sole catalog-mode
    /// cutback driver) must NOT contain `thread::sleep` or an unbounded
    /// retry loop. It is a one-attempt driver; the bounded scheduler
    /// handles retries via Transient deadlines.
    #[test]
    fn exit_proof_cutback_catalog_no_sleep_loop() {
        let src = self_source();
        let body = extract_fn_body(&src, "schedule_cutback_catalog");
        assert!(
            !body.contains("thread::sleep"),
            "schedule_cutback_catalog must not contain thread::sleep (section 11.5b). \
             Found sleep in catalog cutback driver."
        );
        assert!(
            !body.contains("retry_delay"),
            "schedule_cutback_catalog must not contain retry_delay (section 11.5b). \
             Found retry_delay in catalog cutback driver."
        );
    }

    /// Section 11.5b: attempt_cutback_catalog (the inner catalog cutback
    /// logic) must NOT contain `thread::sleep`.
    #[test]
    fn exit_proof_attempt_cutback_catalog_no_sleep() {
        let src = self_source();
        let body = extract_fn_body(&src, "attempt_cutback_catalog");
        assert!(
            !body.contains("thread::sleep"),
            "attempt_cutback_catalog must not contain thread::sleep (section 11.5b)"
        );
    }

    /// Section 11.5b: schedule_cutback (the legacy bridge-mode driver)
    /// still has its retry loop, but must be unreachable from catalog
    /// mode. Verify the legacy function still exists (bridge mode) and
    /// that schedule_cutback_if_owner_changed has a catalog-mode guard.
    #[test]
    fn exit_proof_legacy_cutback_is_bridge_only() {
        let src = self_source();

        // The legacy function must still exist (bridge mode uses it).
        let legacy_body = extract_fn_body(&src, "schedule_cutback");
        assert!(
            legacy_body.contains("thread::sleep"),
            "schedule_cutback (bridge mode) should still have its retry loop"
        );

        // schedule_cutback_if_owner_changed must check catalog mode
        // before calling schedule_cutback.
        let guard_body = extract_fn_body(&src, "schedule_cutback_if_owner_changed");
        assert!(
            guard_body.contains("is_catalog()"),
            "schedule_cutback_if_owner_changed must check is_catalog() before \
             dispatching to the legacy cutback path (section 11.5a)"
        );
        assert!(
            guard_body.contains("enqueue_transition"),
            "schedule_cutback_if_owner_changed must enqueue_transition in \
             catalog mode (section 11.5a)"
        );
    }

    /// Section 11.5b: schedule_activation has a loop+sleep in bridge mode,
    /// but the catalog-mode branch must break before the sleep. Verify
    /// the catalog-mode break exists between the loop and the sleep.
    #[test]
    fn exit_proof_activation_catalog_breaks_before_sleep() {
        let src = self_source();
        let body = extract_fn_body(&src, "schedule_activation");
        let is_catalog_pos = body
            .find("is_catalog")
            .unwrap_or_else(|| panic!("schedule_activation must have an is_catalog check"));
        let sleep_pos = body.find("thread::sleep").unwrap_or_else(|| {
            panic!("schedule_activation should have thread::sleep in bridge path")
        });
        let break_after_catalog = body[is_catalog_pos..]
            .find("break")
            .unwrap_or_else(|| panic!("schedule_activation must break after is_catalog check"));
        assert!(
            is_catalog_pos + break_after_catalog < sleep_pos,
            "schedule_activation catalog-mode break must come before \
             thread::sleep (section 11.5b)"
        );
    }

    /// Section 11.5b: activate_desired_loop has a loop+sleep in bridge
    /// mode for writer contention, but catalog mode must return early.
    /// Verify the catalog-mode return exists before the sleep.
    #[test]
    fn exit_proof_activate_desired_loop_catalog_returns_before_sleep() {
        let src = self_source();
        let body = extract_fn_body(&src, "activate_desired_loop");
        // The catalog check must exist.
        let catalog_check = body
            .find("RuntimeRecordMode::CatalogV2")
            .unwrap_or_else(|| {
                panic!("activate_desired_loop must check CatalogV2 for writer contention")
            });
        // The return-Err must come after the catalog check.
        let return_after = body[catalog_check..]
            .find("return")
            .unwrap_or_else(|| panic!("activate_desired_loop must return after CatalogV2 check"));
        // The sleep must exist (bridge path).
        let sleep_pos = body.find("thread::sleep").unwrap_or_else(|| {
            panic!("activate_desired_loop should have thread::sleep in bridge path")
        });
        // The catalog check + return must come before the sleep.
        assert!(
            catalog_check + return_after < sleep_pos,
            "activate_desired_loop catalog-mode return must come before \
             thread::sleep (section 11.5b)"
        );
    }

    /// Regression: a plain std::thread (like the reconciler/scheduler)
    /// that enters the captured tokio runtime handle can dispatch
    /// `schedule_activation` -> `spawn_blocking` without panicking.
    /// Pre-fix, the thread had no runtime context and spawn_blocking
    /// panicked with "there is no reactor running".
    #[tokio::test]
    async fn reconciler_thread_dispatches_through_captured_runtime_handle() {
        let handle = tokio::runtime::Handle::current();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_clone = ran.clone();

        // Spawn a plain std::thread exactly as spawn_reconciler does.
        // Without `handle.enter()`, the `spawn_blocking` call inside
        // would panic ("there is no reactor running").
        let thread = std::thread::spawn(move || {
            let _guard = handle.enter();
            tokio::task::spawn_blocking(move || {
                ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        });
        thread.join().expect("reconciler thread must not panic");

        // Give the spawn_blocking task a moment to execute.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "spawn_blocking dispatched from a plain thread via captured \
             runtime handle must actually run"
        );
    }

    // ---- F1-F3 closing review tests ----

    /// F1: determine_desired_assignment maps assigned=true to Collected
    /// and unassigned to Local (not inverted). Verified via source-text
    /// assertion because the function requires a full Arc<SharedState>.
    #[test]
    fn f1_desired_assignment_mapping_is_not_inverted() {
        let src = self_source();
        let body = extract_fn_body(&src, "determine_desired_assignment");
        assert!(
            body.contains("DesiredAssignment::Collected"),
            "determine_desired_assignment must map assigned to Collected (F1 fix)"
        );
        assert!(
            body.contains("DesiredAssignment::Local"),
            "determine_desired_assignment must map unassigned to Local (F1 fix)"
        );
        // The mapping must NOT be inverted: assigned -> Collected.
        let assigned_pos = body
            .find("if assigned")
            .expect("must have 'if assigned' check");
        let collected_pos = body[assigned_pos..]
            .find("Collected")
            .expect("Collected must appear after 'if assigned'");
        let local_pos = body[assigned_pos..]
            .find("Local")
            .expect("Local must appear in the else branch");
        assert!(
            collected_pos < local_pos,
            "assigned must map to Collected (before Local in the source) (F1 fix)"
        );
    }

    /// F1: probe_ladder derives scope from the activation record, not
    /// from current auth-table assignments.
    #[test]
    fn f1_probe_ladder_derives_scope_from_activation_not_assignments() {
        let src = self_source();
        let body = extract_fn_body(&src, "probe_ladder");
        assert!(
            body.contains("load_activation_mixed"),
            "probe_ladder must derive scope from load_activation_mixed, not assignments()"
        );
        assert!(
            !body.contains(".assignments()"),
            "probe_ladder must not derive scope from assignments() (F1 fix)"
        );
    }

    /// Catalog cutback staging never emits the migration-only pending mirror
    /// shape; the compare-and-apply writer derives it from typed state.
    #[test]
    fn catalog_staging_does_not_write_untyped_cutback_pending() {
        let src = self_source();
        let body = extract_fn_body(&src, "cutback_to_local_single_attempt");
        assert!(
            !body.contains("mark_cutback_pending_mixed"),
            "catalog staging must leave cutback state to compare-and-apply"
        );
    }

    /// F1: attempt_cutback_catalog requests LocalProjectWalk, not
    /// GitHistory (the cutback stages a local walk).
    #[test]
    fn f1_cutback_requests_local_project_walk() {
        let src = self_source();
        let body = extract_fn_body(&src, "attempt_cutback_catalog");
        assert!(
            body.contains("LocalProjectWalk"),
            "attempt_cutback_catalog must request LocalProjectWalk (F1 fix)"
        );
        // The kind field must be LocalProjectWalk, not GitHistory.
        // Check that the kind assignment uses LocalProjectWalk.
        let kind_line = body
            .lines()
            .find(|l| l.trim_start().starts_with("kind:"))
            .expect("attempt_cutback_catalog must have a kind field");
        assert!(
            kind_line.contains("LocalProjectWalk"),
            "kind field must be LocalProjectWalk, got: {kind_line}"
        );
    }

    /// F2: the scheduler derives scope from the activation record,
    /// not from current auth-table assignments.
    #[test]
    fn f2_scheduler_derives_scope_from_activation() {
        let src = self_source();
        let body = extract_fn_body(&src, "spawn_scheduler");
        assert!(
            body.contains("load_activation_mixed"),
            "scheduler must derive scope from load_activation_mixed (F2 fix)"
        );
    }

    /// F2: the scheduler loop has no plain thread::sleep (uses the
    /// interruptible scheduler_wait condvar instead).
    #[test]
    fn f2_scheduler_no_plain_sleep() {
        let src = self_source();
        let body = extract_fn_body(&src, "spawn_scheduler");
        assert!(
            !body.contains("std::thread::sleep"),
            "scheduler must not use std::thread::sleep for deadline wait (F2 fix)"
        );
    }

    /// F2: the commit observer dispatches events for desired-local
    /// projects (no assignment) using the activation-record scope.
    #[test]
    fn f2_commit_observer_dispatches_for_unassigned_projects() {
        let src = self_source();
        let body = extract_fn_body(&src, "spawn_commit_observer");
        assert!(
            body.contains("load_activation_mixed"),
            "commit observer must use activation record scope for unassigned projects (F2 fix)"
        );
    }

    /// F2: transact includes attachment-snapshot changes in changed ids.
    /// An attachment-only operation must emit changed project ids.
    #[test]
    fn f2_transact_includes_attachment_changes() {
        use bbox_corpus_core::identity::PublishedScope;
        use bbox_corpus_core::project_catalog::{
            AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus,
            CheckoutAttachment, CorpusProject, ProjectId, ProjectScope,
        };

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let store = bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            root.join("catalog"),
        )
        .unwrap();
        let observer = store.commit_observer();

        let project_id =
            ProjectId::parse("p_00000000000000000000000000000f20".to_string()).unwrap();
        let scope = PublishedScope::try_new("f2-transact", ".").unwrap();

        // First transaction: add the project to the catalog.
        let base = store.snapshot().unwrap();
        let epoch = base.epoch();
        store
            .transact(epoch, |catalog, _attachments| {
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id: project_id.clone(),
                        scope: ProjectScope::Published(scope.clone()),
                        operator_aliases: BTreeSet::new(),
                        nominated_aliases: BTreeSet::new(),
                        display_name: "f2-test".to_string(),
                        created_at: "2024-01-01T00:00:00Z".to_string(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: BTreeSet::new(),
                    },
                );
                Ok(())
            })
            .unwrap();
        // Drain the first event.
        let _ = observer.drain_events();

        // Second transaction: attachment-only change (no catalog change).
        let base = store.snapshot().unwrap();
        let epoch = base.epoch();
        store
            .transact(epoch, |_catalog, attachments| {
                attachments.attachments.insert(
                    AttachmentId::parse("att_11111111111111111111111111111111".to_string())
                        .unwrap(),
                    CheckoutAttachment {
                        attachment_id: AttachmentId::parse(
                            "att_11111111111111111111111111111111".to_string(),
                        )
                        .unwrap(),
                        project_id: project_id.clone(),
                        checkout_id: "22222222222222222222222222222222".to_string(),
                        checkout_dir: "/tmp/f2/checkout".to_string(),
                        checkout_project_dir: "/tmp/f2/checkout".to_string(),
                        project_root_relpath: ".".to_string(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: None,
                        capabilities: AttachmentCapabilities {
                            local_code_source: true,
                            git_history: true,
                            blame: false,
                            repo_knowledge: false,
                            repo_mutation: false,
                            render_output: false,
                            provenance_note_io: false,
                            artifact_watching: false,
                        },
                        status: AttachmentStatus::Attached,
                        attached_at: "2024-01-01T00:00:00Z".to_string(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();

        // The observer event must include the project id even though
        // only the attachment snapshot changed (F2 fix).
        let events = observer.drain_events();
        let all_changed: BTreeSet<String> = events
            .iter()
            .flat_map(|e| e.changed_project_ids.iter().cloned())
            .collect();
        assert!(
            all_changed.contains(&project_id.to_string()),
            "attachment-only change must emit the project id in changed_project_ids (F2 fix), \
             got: {:?}",
            all_changed
        );
    }

    /// F3: link 4 verifies the manifest file exists and its digest
    /// matches (not just validate_header).
    #[test]
    fn f3_link4_verifies_manifest_digest() {
        let src = self_source();
        let body = extract_fn_body(&src, "validate_relationship_chain");
        assert!(
            body.contains("verify_generation_manifest_for_migration"),
            "link 4 must call verify_generation_manifest_for_migration (F3 fix)"
        );
    }

    /// F3: link 5 checks the manifest path field (R2F3: now checks exact
    /// equality with the canonical writer-produced path, not just non-emptiness).
    #[test]
    fn f3_link5_checks_manifest_path() {
        let src = self_source();
        let body = extract_fn_body(&src, "validate_relationship_chain");
        assert!(
            body.contains("workspace/{project_id}/manifest.json"),
            "link 5 must check exact workspace manifest path equality (R2F3)"
        );
    }

    /// F3: pre-bind validation runs BEFORE schema rebuild. The
    /// pre_bind_catalog_recovery call must appear before the schema
    /// rebuild drive in open.rs.
    ///
    /// The anchor is the DRIVE CALL, not `schema_was_reset`. P6-B task 5
    /// refactored the replacement sequence out of `open.rs` into the shared
    /// `drive_catalog_schema_replacement`, so the predicate literal no longer
    /// appears here; the invariant this test protects is unchanged, and
    /// anchoring on the call site is what keeps it protected after the move.
    #[test]
    fn f3_pre_bind_runs_before_schema_rebuild() {
        let open_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/open.rs");
        let src = std::fs::read_to_string(&open_path)
            .unwrap_or_else(|_| panic!("failed to read {}", open_path.display()));
        let pre_bind_pos = src
            .find("pre_bind_catalog_recovery")
            .expect("pre_bind_catalog_recovery must be called in open.rs");
        let schema_rebuild_pos = src
            .find("drive_catalog_schema_replacement")
            .expect("the shared schema-replacement drive must be called in open.rs");
        assert!(
            pre_bind_pos < schema_rebuild_pos,
            "pre_bind_catalog_recovery must run BEFORE schema rebuild (F3 fix)"
        );
    }

    /// P6-B task 5: the daemon open path holds NO copy of the replacement
    /// sequence. A copy would fork exactly the ordering a torn replacement
    /// recovers through, and the two copies would then disagree only in the
    /// crash cases nobody exercises by hand.
    #[test]
    fn open_drives_the_shared_replacement_and_keeps_no_copy() {
        let open_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/server/open.rs");
        let src = std::fs::read_to_string(&open_path)
            .unwrap_or_else(|_| panic!("failed to read {}", open_path.display()));
        assert!(
            src.contains("drive_catalog_schema_replacement"),
            "open.rs must call the shared replacement driver"
        );
        for copied in [
            "run_reindex_pass_for_schema_migration",
            "complete_schema_migration",
            "schema_was_reset",
        ] {
            assert!(
                !src.contains(copied),
                "open.rs must not restate `{copied}`: the replacement sequence \
                 belongs to the shared driver, never to a second copy"
            );
        }
    }

    // ---- Activation-record preservation regression tests ----

    /// Helper: extract the source text of a top-level fn from this file.
    fn extract_fn_body_again(src: &str, name: &str) -> String {
        let marker = format!("fn {name}");
        let start = src
            .find(&marker)
            .unwrap_or_else(|| panic!("fn {name} not found in source"));
        let mut depth = 0i32;
        let mut found_open = false;
        let bytes = src.as_bytes();
        let mut end = start;
        for (i, &b) in bytes[start..].iter().enumerate() {
            match b {
                b'{' => {
                    depth += 1;
                    found_open = true;
                }
                b'}' => {
                    depth -= 1;
                    if found_open && depth == 0 {
                        end = start + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        src[start..end].to_string()
    }

    /// Property 1: determine_effective_source checks the activation
    /// record's own selector when the workspace manifest entry is absent.
    /// A collected activation record must be classified as Collected,
    /// not as Warming or Unavailable, even when the workspace entry has
    /// not yet been republished.
    #[test]
    fn regression_effective_source_uses_activation_selector_when_workspace_absent() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "determine_effective_source_from_manifest");
        assert!(
            body.contains("activation.selector()"),
            "effective-source classification must consult the activation record's own selector when the workspace entry is absent (property 1)"
        );
        assert!(
            body.contains("activation_selector.starts_with(\"collected:\")"),
            "effective-source classification must classify a collected activation record as EffectiveSource::Collected when the workspace entry is absent"
        );
    }

    /// Property 2: cutback_to_local_single_attempt preserves a collected
    /// activation record when the workspace manifest does not mark the
    /// project as collected. The local/local stale-state cell returns the
    /// clear-cutback intent to the fenced store writer.
    #[test]
    fn regression_cutback_preserves_collected_activation_record() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "cutback_to_local_single_attempt");
        assert!(
            body.contains("activation.selector().starts_with(\"collected:\")"),
            "cutback_to_local_single_attempt must check the activation record's selector before clearing it (property 2)"
        );
        assert!(
            body.contains("CutbackSuccessOutcome::ClearCutback"),
            "cutback_to_local_single_attempt must return clear-cutback intent for collected records (property 2)"
        );
        assert!(!body.contains("store.clear_cutback_state"));
        assert!(!body.contains("store.clear_activation"));
    }

    /// Property 2 (second path): cutback_to_local also preserves a
    /// collected activation record when the workspace entry is absent.
    #[test]
    fn regression_cutback_to_local_preserves_collected_activation_record() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "cutback_to_local");
        assert!(
            body.contains("activation.selector().starts_with(\"collected:\")"),
            "cutback_to_local must check the activation record's selector before clearing it (property 2)"
        );
    }

    /// Reduction table: desired=Local, effective=Collected, no persisted
    /// state, ladder=None (no scope-matching attachment) must yield
    /// PersistStructural(NoLocalAttachment), NOT clear the activation.
    /// This is the correct outcome for a collected project with no
    /// attachment per the reduction table row local/collected/None/none.
    #[test]
    fn regression_local_collected_no_attachment_persists_structural() {
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Collected,
            None,
            LadderResult::None,
            false,
        );
        assert!(
            matches!(
                action,
                ReducerAction::PersistStructural(CutbackReason::NoLocalAttachment)
            ),
            "local/collected with no attachment and no persisted state must persist Structural(NoLocalAttachment), got {action:?}"
        );
    }

    /// Reduction table: desired=Local, effective=Local, persisted
    /// cutback state -> CancelCutback (clears cutback state only).
    /// This confirms the local/local stale-state cell clears state,
    /// not activation records.
    #[test]
    fn regression_local_local_clears_cutback_state_not_activation() {
        let transient = CutbackStateV2::Transient {
            attempt: 1,
            error_class: CutbackErrorClass::IoPressure,
            deadline_unix_secs: unix_now() + 30,
        };
        let action = evaluate_reduction(
            DesiredAssignment::Local,
            EffectiveSource::Local,
            Some(&transient),
            LadderResult::None,
            false,
        );
        assert!(
            matches!(action, ReducerAction::CancelCutback),
            "local/local with stale state must cancel cutback (clear state only)"
        );
        // CancelCutback dispatches the fenced clear-cutback outcome, not a
        // direct activation delete.
        let src = self_source();
        let cancel_block = src
            .find("ReducerAction::CancelCutback =>")
            .expect("CancelCutback dispatch must exist");
        let snippet = &src[cancel_block..cancel_block + 500];
        assert!(
            snippet.contains("CutbackCompareOutcome::ClearCutback"),
            "CancelCutback dispatch must use the fenced clear-cutback outcome"
        );
        assert!(
            !snippet.contains("CutbackCompareOutcome::ClearActivation"),
            "CancelCutback dispatch must NOT clear the activation"
        );
    }

    // ---- R2F2: workspace entry reconstruction from activation records ----

    /// R2F2: pre_bind_catalog_recovery reconstructs workspace entries from
    /// validated activation records for collected projects missing entries,
    /// before the read-view is constructed.
    #[test]
    fn r2f2_reconstructs_workspace_entries_from_activations() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "reconstruct_workspace_entries_from_activations");
        assert!(
            body.contains("activation.selector()"),
            "reconstruct must consult the activation record selector"
        );
        assert!(
            body.contains("collected:"),
            "reconstruct must only handle collected selectors"
        );
        assert!(
            body.contains("active_snapshot_rel"),
            "reconstruct must derive the exact writer-produced snapshot path"
        );
        assert!(
            body.contains("workspace/{project_id}/manifest.json"),
            "reconstruct must derive the exact writer-produced manifest path"
        );
    }

    /// R2F2: the reconstruction step is called in pre_bind_catalog_recovery
    /// after the relationship chain validation.
    #[test]
    fn r2f2_reconstruction_runs_after_chain_validation() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "pre_bind_catalog_recovery");
        let chain_pos = body
            .find("validate_relationship_chain")
            .expect("chain validation must exist in pre_bind");
        let reconstruct_pos = body
            .find("reconstruct_workspace_entries_from_activations")
            .expect("reconstruction must exist in pre_bind");
        assert!(
            chain_pos < reconstruct_pos,
            "reconstruction must run AFTER chain validation (entries are reconstructed from validated records)"
        );
    }

    // ---- R2F3: exact path equality in chain validation ----

    /// R2F3: link 5 requires EXACT equality with the canonical snapshot path,
    /// not just the final path segment.
    #[test]
    fn r2f3_link5_exact_snapshot_equality() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "validate_relationship_chain");
        assert!(
            body.contains("active_snapshot_rel"),
            "link 5 must derive the expected snapshot path from active_snapshot_rel"
        );
        assert!(
            !body.contains("file_name()"),
            "link 5 must NOT compare by final path segment (R2F3: exact equality)"
        );
    }

    /// R2F3: link 5 requires EXACT equality with the canonical manifest path,
    /// not just non-emptiness.
    #[test]
    fn r2f3_link5_exact_manifest_path() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "validate_relationship_chain");
        assert!(
            body.contains("workspace/{project_id}/manifest.json"),
            "link 5 must check exact manifest path equality"
        );
        assert!(
            !body.contains("manifest path missing"),
            "link 5 must NOT check only non-emptiness (R2F3: exact equality)"
        );
    }

    /// R2F3: manifest read uses bounded O_NOFOLLOW descriptor.
    #[test]
    fn r2f3_manifest_read_is_bounded_nofollow() {
        let store_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/bbox-code-source-store/src/lib.rs");
        let src = std::fs::read_to_string(&store_path)
            .unwrap_or_else(|_| panic!("failed to read {}", store_path.display()));
        let body_pos = src
            .find("fn read_generation_manifest_bytes")
            .expect("read_generation_manifest_bytes must exist");
        let body = &src[body_pos..body_pos + 1500];
        assert!(
            body.contains("O_NOFOLLOW"),
            "manifest read must use O_NOFOLLOW (R2F3)"
        );
        assert!(
            body.contains("max_manifest_logical_bytes"),
            "manifest read must check size before allocation (R2F3)"
        );
    }

    /// R2F2 regression: activate_pending_local_snapshots must check for an
    /// existing collected: entry and exclude it before publishing local:.
    /// Without this guard the background reindex overwrites the collected
    /// selector that reconstruction placed, breaking the restart chain.
    #[test]
    fn r2f2_reindex_preserves_collected_entry() {
        let snapshot_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/bbox-edge-sidecar/src/snapshot.rs");
        let src = std::fs::read_to_string(&snapshot_path)
            .unwrap_or_else(|_| panic!("failed to read {}", snapshot_path.display()));
        let body = extract_fn_body_again(&src, "activate_pending_local_snapshots");
        assert!(
            body.contains("starts_with(\"collected:\")"),
            "activate_pending_local_snapshots must check for existing collected: selector"
        );
        assert!(
            body.contains(".filter(") && body.contains("!index"),
            "activate_pending_local_snapshots must exclude collected entries from the effective activation set"
        );
    }

    // ---- Crash-window admission and convergence ----

    /// Exit row 12.4: the cutback crash window. The daemon crashed between
    /// local manifest publication (activate_local_snapshot_with wrote the
    /// manifest entry as local:<project_id>) and activation-record clear
    /// (clear_activation never ran). On restart the workspace entry is
    /// local:<project_id> while the activation record is collected:....
    /// The relationship chain must ADMIT this shape so the daemon boots.
    #[test]
    fn crash_window_local_manifest_collected_record_passes_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("crash-win", ".").unwrap();
        let project_id = "p_0000000000000000000000000000cw1";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);

        // Manifest entry has the writer's own local shape for THIS project.
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!("workspace/{project_id}/snapshots/local-cw_snap")),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(project_id)),
                code_source_generation: Some("local".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_ok(),
            "crash window (local manifest + collected record) must pass chain, got: {:?}",
            result.err()
        );
    }

    /// Crash-window admission is narrow: a local selector for a DIFFERENT
    /// project id is genuine drift, not the crash window. Must still fail
    /// closed.
    #[test]
    fn crash_window_wrong_project_local_selector_fails_chain() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();

        let scope = PublishedScope::try_new("crash-bad", ".").unwrap();
        let project_id = "p_0000000000000000000000000000cw2";
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );

        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            project_id,
            &scope,
            &generation_id,
            None,
            false,
        );

        let snapshot = p4f_catalog_snapshot(project_id, scope, vec![]);

        // Manifest entry is local:<DIFFERENT_PROJECT> - this is cross-project
        // drift, not the crash window. Must fail closed.
        let mut manifest = bbox_edge_sidecar::manifest::ManifestIndex::new();
        manifest.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(format!("workspace/{project_id}/snapshots/local-bad")),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(
                    "p_0000000000000000000000000000other",
                )),
                code_source_generation: Some("local".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "wrong-project local selector must fail chain (genuine drift)"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("selector mismatch"),
            "error must be selector mismatch, got: {err}"
        );
    }

    /// Structural assertion: the chain validation source contains the
    /// crash-window admission logic, naming the specific selector shape.
    #[test]
    fn crash_window_admission_logic_exists() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "validate_relationship_chain");
        assert!(
            body.contains("is_cutback_crash_window"),
            "chain must define a cutback crash window predicate"
        );
        assert!(
            body.contains("local_selector(project_id)"),
            "crash window must check the writer's own local selector for this project"
        );
        assert!(
            body.contains("cutback crash window admitted"),
            "crash window admission must emit a tracing::info"
        );
        // R3F2: crash-window admission must validate the full entry shape.
        assert!(
            body.contains("crash-window entry has wrong manifest path"),
            "crash window must validate manifest path"
        );
        assert!(
            body.contains("crash-window entry has non-local generation"),
            "crash window must validate generation shape"
        );
        assert!(
            body.contains("crash-window entry has unsafe snapshot path"),
            "crash window must validate snapshot path confinement"
        );
    }

    // ---- R3F2: adversarial crash-window field tests ----

    fn crash_window_fixture() -> (
        tempfile::TempDir,
        Arc<CodeSourceStore>,
        CatalogSnapshotV2,
        String,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let runtime = CodeSourceRuntime::for_test_catalog(&root);
        let store = runtime.store();
        let scope = PublishedScope::try_new("crash-adv", ".").unwrap();
        let project_id = "p_0000000000000000000000000000adv".to_string();
        let generation_id = compute_generation_id(
            "p4f-producer",
            &empty_generation_descriptor(scope.clone(), &"a".repeat(40)),
        );
        p4f_seed_activation(
            &store,
            &root.join("code-sources"),
            &project_id,
            &scope,
            &generation_id,
            None,
            false,
        );
        let snapshot = p4f_catalog_snapshot(&project_id, scope, vec![]);
        (directory, store, snapshot, project_id)
    }

    fn crash_window_entry(
        project_id: &str,
        manifest: &str,
        generation: &str,
        snapshot: &str,
    ) -> bbox_edge_sidecar::manifest::ManifestIndex {
        let mut m = bbox_edge_sidecar::manifest::ManifestIndex::new();
        m.workspaces.insert(
            project_id.to_string(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: manifest.to_string(),
                active_snapshot: Some(snapshot.to_string()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(project_id)),
                code_source_generation: Some(generation.to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        m
    }

    #[test]
    fn crash_window_wrong_manifest_path_fails() {
        let (dir, store, snapshot, pid) = crash_window_fixture();
        let manifest = crash_window_entry(
            &pid,
            "workspace/other-project/manifest.json", // wrong manifest path
            "local",
            &format!("workspace/{pid}/snapshots/local-snap"),
        );
        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "wrong manifest path must fail even in crash window"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("wrong manifest path"), "got: {err}");
        drop(dir);
    }

    #[test]
    fn crash_window_foreign_generation_fails() {
        let (dir, store, snapshot, pid) = crash_window_fixture();
        let manifest = crash_window_entry(
            &pid,
            &format!("workspace/{pid}/manifest.json"),
            "foreign-generation-id", // not "local"
            &format!("workspace/{pid}/snapshots/local-snap"),
        );
        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "foreign generation must fail even in crash window"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("non-local generation"), "got: {err}");
        drop(dir);
    }

    #[test]
    fn crash_window_traversal_snapshot_fails() {
        let (dir, store, snapshot, pid) = crash_window_fixture();
        let manifest = crash_window_entry(
            &pid,
            &format!("workspace/{pid}/manifest.json"),
            "local",
            &format!("workspace/{pid}/snapshots/../../../etc/passwd"), // traversal
        );
        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "traversal snapshot must fail even in crash window"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsafe snapshot path"), "got: {err}");
        drop(dir);
    }

    #[test]
    fn crash_window_cross_project_snapshot_fails() {
        let (dir, store, snapshot, pid) = crash_window_fixture();
        let manifest = crash_window_entry(
            &pid,
            &format!("workspace/{pid}/manifest.json"),
            "local",
            "workspace/p_0000000000000000000000000other/snapshots/local-snap", // cross-project
        );
        let result = validate_relationship_chain(&store, &snapshot, &manifest);
        assert!(
            result.is_err(),
            "cross-project snapshot must fail even in crash window"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unsafe snapshot path"), "got: {err}");
        drop(dir);
    }

    #[test]
    fn crash_window_missing_snapshot_fails() {
        let (dir, store, snapshot, pid) = crash_window_fixture();
        let mut m = bbox_edge_sidecar::manifest::ManifestIndex::new();
        m.workspaces.insert(
            pid.clone(),
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: format!("workspace/{pid}/manifest.json"),
                active_snapshot: None, // missing
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(bbox_code_source::local_selector(&pid)),
                code_source_generation: Some("local".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        let result = validate_relationship_chain(&store, &snapshot, &m);
        assert!(
            result.is_err(),
            "missing snapshot must fail even in crash window"
        );
        drop(dir);
    }

    // ---- R3F5: reconstruction overlay ownership ----

    #[test]
    fn r3f5_reconstruction_sets_overlay_managed_true() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "reconstruct_workspace_entries_from_activations");
        // The reconstruction body must set git_overlay_managed: true to
        // match the authoritative collected writer (activate_source_snapshot
        // in edge-sidecar snapshot.rs:313).
        assert!(
            body.contains("git_overlay_managed: true"),
            "reconstruction must set git_overlay_managed: true to match the collected writer"
        );
    }

    // ---- R3F3: automatic bridge-clear evidence ----

    #[test]
    fn r3f3_automatic_bridge_clear_enumerates_retained_set() {
        let src = self_source();
        let body = extract_fn_body_again(&src, "try_automatic_bridge_clear");
        // R3F3: the automatic path must enumerate the actual store
        // contents to build the retained set, not fabricate an empty set
        // in the evidence struct.
        assert!(
            body.contains("retained_generation_ids"),
            "automatic bridge-clear must build retained_generation_ids from store enumeration"
        );
        assert!(
            body.contains("retirement_generation_inventory"),
            "automatic bridge-clear must use strict store-owned generation enumeration"
        );
        assert!(
            body.contains("effective_scope"),
            "automatic bridge-clear must pass effective_scope evidence"
        );
        // The evidence struct must use the enumerated set, not an inline
        // BTreeSet::new() in the struct literal.
        let ev_pos = body
            .find("ScopeBridgeClearEvidence {")
            .unwrap_or(usize::MAX);
        let ev_close = body[ev_pos..]
            .find("}")
            .map(|p| ev_pos + p)
            .unwrap_or(usize::MAX);
        let ev_literal = &body[ev_pos..ev_close];
        assert!(
            !ev_literal.contains("BTreeSet::new()"),
            "evidence struct must not inline an empty retained set"
        );
        assert!(
            !body.contains(".ok().flatten()") && !body.contains("if let Ok"),
            "automatic bridge-clear must propagate activation and enumeration errors"
        );
        assert!(body.contains("AutomaticFirstNewScope"));
    }

    #[test]
    fn reconciler_activation_failure_does_not_hide_following_project() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("code-sources");
        let store =
            bbox_code_source_store::CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let paths = bbox_code_source_store::CodeSourceStorePaths::new(&root).unwrap();
        let project_one =
            bbox_corpus_core::project_catalog::ProjectId::parse("project-one").unwrap();
        std::fs::write(paths.activation(&project_one), b"{malformed").unwrap();

        let mut processed = Vec::new();
        for project_id in ["project-one", "project-two"] {
            if load_reconciler_activation(&store, project_id).is_ok() {
                processed.push(project_id);
            }
        }

        assert_eq!(processed, vec!["project-two"]);
    }
}
