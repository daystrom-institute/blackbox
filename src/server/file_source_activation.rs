//! The connector activation lane: from a finalized generation to searchable
//! documents.
//!
//! Finalize means "the bytes are durable and the manifest agrees with its
//! descriptor". Activation is a separate, longer transaction, and its failure
//! must not be reported as a failed upload, so it runs off the request path
//! and the producer polls the status route to learn the outcome. That is the
//! same contract the code lane offers and the reason
//! [`FileGenerationStateV1::Superseded`] counts as terminal SUCCESS.
//!
//! # The four writes, and why the order is the order
//!
//! One activation touches two durable stores that share no transaction, in
//! this literal order (the shape `bbox_file_source_store` documents and its
//! tear fixtures pin):
//!
//! 1. `stage_activation` - the generation records what staging produced.
//! 2. `install_activation` - the activation record is durably installed.
//! 3. the derived edge-sidecar workspace manifest - one atomic replacement
//!    publishes the `collected:` selector and the active snapshot.
//! 4. `mark_active` - the state flip, written LAST.
//!
//! Each store is individually crash safe; the PAIR is not. Writing the flip
//! last is what makes `Active` proof that the manifest write was issued, and
//! that single fact is what lets [`recover_connector_activations`] tell a
//! lost derived write from an in-flight activation instead of guessing.
//!
//! # The staged hold
//!
//! Index staging returns a `StagedIndexGeneration` that IS a release token:
//! the writer actor parks on its release channel until the token drops, so
//! the staged documents cannot be reclaimed underneath the four writes above.
//! `begin_publication()` converts the bounded staging hold into an unbounded
//! publication hold and must be called BEFORE the first durable write, or the
//! actor can time out mid-activation. The token is dropped only after the
//! flip.
//!
//! # Project identity
//!
//! The CATALOG owns the mapping from a connector scope to a project, and this
//! lane never derives one. `project_for` is the only direction consulted; a
//! scope with no catalog project is pending onboarding, which the publication
//! routes already refuse, so reaching activation without one is a bug rather
//! than a state to paper over.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_file_source::FileGenerationStateV1;
use bbox_file_source_store::{ActivationTear, ConnectorRetirementRecord, FileSourceStore};

use super::SharedState;

/// Enqueue activation for a freshly finalized generation.
///
/// Deliberately fire-and-forget: the finalize response must not wait on index
/// staging, and a failed activation is reported through generation status and
/// the daemon log, never as a failed upload. The whole body is blocking (the
/// writer actor speaks sync channels and the store fsyncs), so it crosses
/// `spawn_blocking` rather than occupying a serving worker.
pub(crate) fn enqueue_activation(
    state: &Arc<SharedState>,
    scope: ConnectorScope,
    generation_id: String,
) {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = activate_generation(&state, &scope, &generation_id) {
            tracing::error!(
                generation = %generation_id,
                error = %error,
                "connector generation activation failed"
            );
            let store = state.file_sources.store();
            // Best effort, and deliberately not fatal: the generation's bytes
            // are durable and correct, so a failed activation leaves it
            // Failed with a diagnostic rather than destroying anything a
            // retry could use.
            if let Err(error) = store.set_state(
                &scope,
                &generation_id,
                FileGenerationStateV1::Failed,
                Some("activation failed; inspect daemon logs".into()),
            ) {
                tracing::warn!(
                    generation = %generation_id,
                    error = %error,
                    "could not record the failed connector activation"
                );
            }
        }
    });
}

/// Run one connector activation to completion.
///
/// Blocking by construction. Returns `Ok(())` when the generation is `Active`
/// and its documents are reachable under the published `collected:` selector.
pub(crate) fn activate_generation(
    state: &Arc<SharedState>,
    scope: &ConnectorScope,
    generation_id: &str,
) -> Result<()> {
    let store = state.file_sources.store();
    let generation = store
        .load_generation(scope, generation_id)?
        .ok_or_else(|| {
            anyhow!("connector generation {generation_id} disappeared before activation")
        })?;
    if generation.state == FileGenerationStateV1::Active {
        return Ok(());
    }
    let project_id = project_for_scope(state, scope)?;
    let entries = store.manifest(scope, generation_id)?;
    let identity = super::code_source::resolve_code_project_identity(
        state,
        &project_id,
        "connector activation",
    )?;

    let staged = state.index_writer.stage_connector_generation(
        identity,
        generation.descriptor.clone(),
        generation_id.to_string(),
        entries,
        store.clone(),
    )?;

    // Everything below is a durable write, so the bounded staging hold
    // becomes an unbounded publication hold first.
    staged.begin_publication()?;
    // The documents must still be exactly what staging counted. This catches
    // a concurrent pass that moved the selector's population between the ack
    // and the activation record, which would otherwise install a record whose
    // document_count is a lie.
    state
        .index_writer
        .verify_code_selector_document_count(&staged.selector, staged.document_count)?;

    // 1. What staging produced, recorded before any activation record exists.
    store.stage_activation(
        scope,
        generation_id,
        staged.document_count,
        &staged.entity_inventory_sha256,
    )?;
    // 2. The activation record. `project_id` comes from the catalog and is
    // never derived here.
    let record = store.install_activation(scope, generation_id, &project_id, &now_rfc3339())?;
    // 3. The derived manifest. Both metadata arguments are advisory on this
    // lane: there is no repository, so the connector source id names the
    // source, and `remote_watermark` occupies the slot `head_commit` occupies
    // for code sources exactly as the wire contract says it does. Neither
    // gates anything this transaction commits.
    publish_derived_manifest(
        state,
        &project_id,
        scope,
        generation_id,
        &generation.descriptor.remote_watermark,
        &staged.selector,
        &staged.snapshot_id,
    )?;
    state.nudge_edge_index_rebuild();
    // 4. The flip, last.
    store.mark_active(scope, generation_id)?;
    tracing::info!(
        project_id = %project_id,
        generation = %generation_id,
        documents = staged.document_count,
        selector = %record.selector,
        "connector generation activated"
    );
    // Release the writer lane only after the flip: the actor is parked on
    // this token, so holding it past here costs nothing and dropping it
    // earlier would let a reindex pass reclaim documents mid-activation.
    let superseded = record.superseded_generation_id.clone();
    drop(staged);
    // Retirement is enqueued only AFTER the staged hold is released. The
    // writer actor is parked on that hold, so sending it another op while the
    // token is alive would deadlock the lane against itself.
    retire_superseded_generation(state, scope, &project_id, superseded.as_deref());
    Ok(())
}

/// Delete the superseded generation's documents from the index.
///
/// Every generation mints its OWN selector (the derivation folds in the
/// generation id), so activating a new one does not overwrite the previous
/// one's documents: staging deletes only the incoming selector's term, which
/// is what makes a restage idempotent. Without an explicit retirement the
/// outgoing generation's documents stay in the index forever, one full corpus
/// per publication cycle. That is the same reason the code lane retires its
/// predecessor rather than relying on replacement.
///
/// Best effort, and deliberately AFTER the flip. The generation is already
/// published at this point, so a retirement failure must degrade to a warning
/// rather than unpublish a valid generation; the writer refuses to retire a
/// selector that is still active in the manifest, so the failure mode is a
/// retained corpus, never a live one deleted out from under readers.
///
/// A retryable refusal (writer readiness: a reindex pass holds the lane, or
/// the vector store is still warming up) is journaled and handed to the
/// redrive coordinator rather than dropped on the floor. See
/// [`ConnectorRetirementCoordinator`] and gap-7e44ee3b: before the redrive
/// existed, an activation landing during post-boot vector warming stranded
/// its predecessor's whole corpus, unreachable through the active selector,
/// forever.
///
/// Blobs and sidecar snapshots are not touched here: they are content
/// addressed and shared across generations, and their reclamation is a
/// separate sweep with its own grace window.
fn retire_superseded_generation(
    state: &Arc<SharedState>,
    scope: &ConnectorScope,
    project_id: &str,
    superseded_generation_id: Option<&str>,
) {
    let Some(superseded) = superseded_generation_id else {
        // The first generation for a scope supersedes nothing.
        return;
    };
    let selector =
        crate::index::project_files::collected_materialization_selector(project_id, superseded);
    match state.index_writer.retire_code_selector(selector.clone()) {
        Ok(retired) => {
            tracing::info!(
                project_id,
                superseded_generation = %superseded,
                documents = retired.document_count,
                "retired the superseded connector generation"
            );
            // Dropping the token releases the writer lane's retirement hold.
            drop(retired);
        }
        // A retryable refusal is a READINESS condition, not a failure: the
        // writer defers retirement while a reindex pass holds the lane or
        // while the vector store is still warming, which is the ordinary
        // shape of an activation that lands just after boot. It is logged
        // apart from a real failure so an operator reading the log is not
        // sent hunting a fault that does not exist.
        //
        // The deferral is journaled durably and handed to the redrive
        // coordinator, which retries with backoff until the readiness gate
        // opens (or the daemon restarts and boot recovery picks the journal
        // row back up). Bounded at one journal row per superseded selector,
        // which is exactly what makes it safe to keep retrying forever rather
        // than giving up: the row is idempotent to complete twice.
        Err(error) if super::code_source::selector_retirement_retryable(&error) => {
            tracing::warn!(
                project_id,
                superseded_generation = %superseded,
                error = %error,
                "retirement of the superseded connector generation was \
                 deferred by writer readiness; it is journaled for the \
                 connector retirement redrive and will be retried until it \
                 completes"
            );
            defer_connector_retirement(
                state,
                ConnectorRetirementRecord {
                    version: 1,
                    project_id: project_id.to_string(),
                    scope: scope.clone(),
                    superseded_generation_id: superseded.to_string(),
                    selector,
                    reason: error.to_string(),
                },
            );
        }
        Err(error) => tracing::warn!(
            project_id,
            superseded_generation = %superseded,
            error = %error,
            "could not retire the superseded connector generation; its \
             documents remain in the index and are not reachable through the \
             active selector"
        ),
    }
}

// ---------------------------------------------------------------------------
// Connector retirement redrive (gap-7e44ee3b)
//
// A selector retirement deferred by writer readiness (the vector store still
// warming up, or a reindex pass holding the lane) is journaled durably by
// `bbox_file_source_store::FileSourceStore::enqueue_retirement` and handed to
// this coordinator, which retries it with backoff until it completes. Boot
// recovery (`recover_connector_retirements`) re-enqueues every row still
// journaled from a prior process, so a restart mid-deferral does not lose it,
// and completing an already-completed row is a no-op
// (`FileSourceStore::complete_retirement`), so redriving an already-retired
// selector is safe.
//
// TWIN: `src/server/code_source.rs`'s `RetirementCoordinator` is the code
// lane's equivalent, wired to `bbox-code-source-store`'s retirement journal.
// This coordinator mirrors its scheduling shape (a durable journal row, an
// in-memory backoff queue, one lazily-spawned redrive thread) rather than
// reusing that type directly: the code lane's coordinator is threaded through
// `PublishedScope`, `ProjectId`, collision-retirement variants, and
// catalog-generation-state transitions that have no connector-lane
// equivalent, and the connector store's simpler single-activation-per-scope
// model (no `BridgeV1`/`CatalogV2` split, no collision lane) does not need
// any of that. Sharing only the parts that are actually the same -- the
// selector-retryable predicate `code_source::selector_retirement_retryable`
// and the underlying `retire_code_selector` writer op -- keeps this lane's
// machinery proportionate to what it actually has to redrive.
// ---------------------------------------------------------------------------

struct ConnectorRetirementEntry {
    record: ConnectorRetirementRecord,
    attempts: u32,
    retry_delay: std::time::Duration,
    next_due: std::time::Instant,
}

/// In-memory backoff scheduler for journaled connector selector retirements.
/// One per daemon, owned by [`super::file_source::FileSourceRuntime`].
pub(crate) struct ConnectorRetirementCoordinator {
    queue: std::sync::Mutex<std::collections::BTreeMap<String, ConnectorRetirementEntry>>,
    notify: std::sync::Condvar,
    started: std::sync::atomic::AtomicBool,
}

impl ConnectorRetirementCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            queue: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            notify: std::sync::Condvar::new(),
            started: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

const CONNECTOR_RETIREMENT_RETRY_LIMIT: u32 = 8;
const CONNECTOR_RETIREMENT_REDRIVE_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// Exponential backoff up to a cap, matching the code lane's cadence
/// (`SELECTOR_RETIREMENT_RETRY_LIMIT` / `SELECTOR_RETIREMENT_REDRIVE_DELAY`
/// in `code_source.rs`). Exhausting the budget does not stop the redrive: it
/// only widens the delay to the slow cadence, because giving up would
/// reintroduce the exact strand this coordinator exists to close.
fn take_connector_retirement_retry(
    attempts: &mut u32,
    delay: &mut std::time::Duration,
) -> Option<std::time::Duration> {
    if *attempts >= CONNECTOR_RETIREMENT_RETRY_LIMIT {
        return None;
    }
    *attempts += 1;
    let current = *delay;
    *delay = (*delay * 2).min(std::time::Duration::from_secs(30));
    Some(current)
}

/// Journal a deferred retirement, THEN enqueue it in memory.
///
/// The order is load-bearing: journaling first means a crash between the two
/// calls loses only the in-memory schedule, which boot recovery rebuilds from
/// the journal, never the durable fact that this selector still needs
/// retiring.
fn defer_connector_retirement(state: &Arc<SharedState>, record: ConnectorRetirementRecord) {
    if let Err(error) = state.file_sources.store().enqueue_retirement(&record) {
        tracing::error!(
            project_id = %record.project_id,
            selector = %record.selector,
            %error,
            "journaling a deferred connector retirement failed; the redrive \
             cannot recover this deferral across a restart"
        );
    }
    enqueue_connector_retirement_work(state.clone(), record);
}

fn enqueue_connector_retirement_work(state: Arc<SharedState>, record: ConnectorRetirementRecord) {
    let coordinator = state.file_sources.retirement_coordinator();
    {
        let mut queue = coordinator.queue.lock().unwrap();
        let key = record.selector.clone();
        queue
            .entry(key)
            .and_modify(|entry| entry.next_due = std::time::Instant::now())
            .or_insert_with(|| ConnectorRetirementEntry {
                record,
                attempts: 0,
                retry_delay: std::time::Duration::from_secs(1),
                next_due: std::time::Instant::now(),
            });
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
            .name("blackbox-connector-retirement".to_string())
            .spawn(move || connector_retirement_coordinator_loop(state, coordinator, shutdown))
        {
            tracing::error!(%error, "spawning connector retirement coordinator failed");
        }
    }
}

fn connector_retirement_coordinator_loop(
    state: Arc<SharedState>,
    coordinator: Arc<ConnectorRetirementCoordinator>,
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
                        break queue
                            .remove(&key)
                            .expect("selected connector retirement work exists");
                    }
                    Some((_key, due)) => {
                        let wait = due
                            .saturating_duration_since(now)
                            .min(CONNECTOR_RETIREMENT_REDRIVE_DELAY);
                        (queue, _) = coordinator.notify.wait_timeout(queue, wait).unwrap();
                    }
                    None => {
                        (queue, _) = coordinator
                            .notify
                            .wait_timeout(queue, CONNECTOR_RETIREMENT_REDRIVE_DELAY)
                            .unwrap();
                    }
                }
            }
        };

        let result = run_connector_retirement_attempt(&state, &entry.record);
        let requeue_delay = match result {
            ConnectorRetirementAttempt::Complete => None,
            ConnectorRetirementAttempt::Retryable(error) => {
                let delay =
                    take_connector_retirement_retry(&mut entry.attempts, &mut entry.retry_delay)
                        .unwrap_or(CONNECTOR_RETIREMENT_REDRIVE_DELAY);
                tracing::warn!(
                    project_id = %entry.record.project_id,
                    selector = %entry.record.selector,
                    attempts = entry.attempts,
                    retry_secs = delay.as_secs_f64(),
                    %error,
                    "connector retirement redrive attempt deferred"
                );
                Some(delay)
            }
            ConnectorRetirementAttempt::Failed(error) => {
                tracing::error!(
                    project_id = %entry.record.project_id,
                    selector = %entry.record.selector,
                    %error,
                    "connector retirement redrive attempt failed; it remains \
                     journaled and will be retried"
                );
                Some(CONNECTOR_RETIREMENT_REDRIVE_DELAY)
            }
        };

        if let Some(delay) = requeue_delay {
            entry.next_due = std::time::Instant::now() + delay;
            let key = entry.record.selector.clone();
            let mut queue = coordinator.queue.lock().unwrap();
            queue.entry(key).or_insert(entry);
        }
    }
}

enum ConnectorRetirementAttempt {
    Complete,
    Retryable(anyhow::Error),
    Failed(anyhow::Error),
}

fn run_connector_retirement_attempt(
    state: &Arc<SharedState>,
    record: &ConnectorRetirementRecord,
) -> ConnectorRetirementAttempt {
    let retired = match state
        .index_writer
        .retire_code_selector(record.selector.clone())
    {
        Ok(retired) => retired,
        Err(error) if super::code_source::selector_retirement_retryable(&error) => {
            return ConnectorRetirementAttempt::Retryable(error);
        }
        Err(error) => return ConnectorRetirementAttempt::Failed(error),
    };
    let document_count = retired.document_count;
    // Dropping the token releases the writer lane's retirement hold, exactly
    // as the first attempt does in `retire_superseded_generation`. Sidecar
    // snapshots are never touched by this lane (content addressed, shared
    // across generations, reclaimed by a separate sweep), so there is no
    // cleanup hold to extend the way the code lane's redrive does.
    drop(retired);
    if let Err(error) = state.file_sources.store().complete_retirement(record) {
        return ConnectorRetirementAttempt::Failed(error);
    }
    tracing::info!(
        project_id = %record.project_id,
        connector_source_id = record.scope.connector_source_id().as_str(),
        superseded_generation = %record.superseded_generation_id,
        selector = %record.selector,
        documents = document_count,
        "connector retirement redrive completed a deferred retirement; the \
         stranded corpus is healed"
    );
    ConnectorRetirementAttempt::Complete
}

/// Boot recovery: re-enqueue every retirement still journaled from a prior
/// process. Runs after [`recover_connector_activations`] for the same reason
/// that call documents -- the read view must exist first -- though this sweep
/// does not itself touch the read view.
///
/// Failure for one row is logged and skipped, matching
/// `recover_connector_activations`'s per-scope failure isolation: a store
/// read error here must not take a working daemon boot down with it.
pub(crate) fn recover_connector_retirements(state: &Arc<SharedState>) {
    let store = state.file_sources.store();
    match store.retirement_records() {
        Ok(records) => {
            for record in records {
                tracing::info!(
                    project_id = %record.project_id,
                    selector = %record.selector,
                    superseded_generation = %record.superseded_generation_id,
                    "recovering a journaled connector retirement from a prior process"
                );
                enqueue_connector_retirement_work(state.clone(), record);
            }
        }
        Err(error) => {
            tracing::error!(
                %error,
                "connector retirement recovery skipped: the retirement journal is unreadable"
            );
        }
    }
}

/// Nudge every journaled deferral to retry promptly once the vector store
/// finishes warming, instead of waiting out its current backoff. Called from
/// the same place the code lane's `notify_cutback_readiness_available` is
/// (`spawn_vector_warmup_thread` in `background.rs`); purely an optimization,
/// since the coordinator's own backoff already retries without it.
pub(crate) fn notify_connector_retirement_readiness_available(state: &Arc<SharedState>) {
    let coordinator = state.file_sources.retirement_coordinator();
    let now = std::time::Instant::now();
    {
        let mut queue = coordinator.queue.lock().unwrap();
        for entry in queue.values_mut() {
            entry.next_due = now;
        }
    }
    coordinator.notify.notify_one();
}

/// Replace the workspace manifest so readers select this generation, and swap
/// the pinned code read view inside the same manifest coordinator.
///
/// Doing the swap INSIDE the coordinator is what makes the new selector and
/// the view that filters on it become visible together. A view published one
/// republish later would filter out the documents the manifest just
/// activated.
fn publish_derived_manifest(
    state: &Arc<SharedState>,
    project_id: &str,
    scope: &ConnectorScope,
    generation_id: &str,
    remote_watermark: &str,
    selector: &str,
    snapshot_id: &str,
) -> Result<()> {
    let edges_dir = super::edge_sidecar_dir(state);
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        &edges_dir,
        project_id,
        scope.connector_source_id().as_str(),
        remote_watermark,
        generation_id,
        selector,
        snapshot_id,
        || {
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), selector.to_string());
            index.replace_active_code_selectors(selectors.clone());
            state
                .edge_index_ready
                .store(false, std::sync::atomic::Ordering::Release);
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                searcher: index.searcher(),
                edge_index: Arc::new(crate::edge_index::EdgeIndex::default()),
                catalog_epoch: state.records_provider.records_snapshot().authority_epoch,
                git_overlays: super::state::read_git_overlays_for_view(
                    &state.project_authority,
                    &edges_dir,
                    &state.git_transport_cutover,
                    &state.code_sources,
                ),
            });
            Ok(())
        },
    )
}

/// The catalog project a connector scope publishes into.
fn project_for_scope(state: &Arc<SharedState>, scope: &ConnectorScope) -> Result<String> {
    state
        .code_sources
        .producer_auth()
        .connectors()
        .project_for(scope.connector_source_id())
        .map(|project_id| project_id.as_str().to_string())
        .ok_or_else(|| {
            anyhow!(
                "connector scope {} has no catalog project; onboard it before activating",
                scope.connector_source_id().as_str()
            )
        })
}

/// Reconcile every granted connector scope against the derived manifest at
/// boot.
///
/// Total by construction: [`bbox_file_source_store::classify_activation_tear`]
/// lands every combination of (recorded generation, manifest generation,
/// state) on exactly one arm, so no interleaving of the four activation
/// writes is unhandled and none of them is a crash loop.
///
/// Failure for one scope is logged and skipped rather than refusing boot: a
/// connector source that cannot reconcile is one project's documents missing,
/// and taking the whole daemon down for it would be the worse outcome.
pub(crate) fn recover_connector_activations(state: &Arc<SharedState>) {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    if !connectors.enabled() {
        return;
    }
    let store = state.file_sources.store();
    let edges_dir = super::edge_sidecar_dir(state);
    let manifest = match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "connector activation recovery skipped: the derived manifest is unreadable"
            );
            return;
        }
    };
    let scopes: Vec<ConnectorScope> = connectors
        .grants()
        .iter()
        .map(|grant| grant.scope.clone())
        .collect();
    for scope in scopes {
        let Some(project_id) = connectors.project_for(scope.connector_source_id()) else {
            // Pending onboarding: no project, so no manifest entry can name
            // one of its generations and there is nothing to reconcile.
            continue;
        };
        let manifest_generation = manifest
            .workspaces
            .get(project_id.as_str())
            .and_then(|entry| entry.code_source_generation.clone());
        if let Err(error) = recover_one_scope(state, &store, &scope, manifest_generation.as_deref())
        {
            tracing::warn!(
                connector_source_id = %scope.connector_source_id().as_str(),
                error = %error,
                "connector activation recovery failed for one scope"
            );
        }
    }
}

fn recover_one_scope(
    state: &Arc<SharedState>,
    store: &Arc<FileSourceStore>,
    scope: &ConnectorScope,
    manifest_generation: Option<&str>,
) -> Result<()> {
    let Some(tear) = store.classify_tear(scope, manifest_generation)? else {
        return Ok(());
    };
    let record = store
        .active_generation(scope)?
        .ok_or_else(|| anyhow!("a classified tear lost its activation record"))?;
    let generation_id = record.generation_id.as_str();
    match tear {
        ActivationTear::Converged => Ok(()),
        ActivationTear::RecoverForwardReplayStateFlip => {
            // Both sides name this generation: the activation completed and
            // only the flip was lost. Replaying it converges.
            tracing::info!(
                generation = %generation_id,
                "replaying a lost connector activation state flip"
            );
            store.mark_active(scope, generation_id)
        }
        ActivationTear::RecoverForwardRepublishManifest => {
            // Active proves the manifest write was ISSUED, so an absent or
            // stale entry is a lost derived write, not an in-flight
            // activation. This store is the authority; republish.
            tracing::info!(
                generation = %generation_id,
                "republishing a lost connector derived-manifest write"
            );
            let project_id = project_for_scope(state, scope)?;
            let generation = store
                .load_generation(scope, generation_id)?
                .ok_or_else(|| anyhow!("active connector generation {generation_id} is missing"))?;
            publish_derived_manifest(
                state,
                &project_id,
                scope,
                generation_id,
                &generation.descriptor.remote_watermark,
                &record.selector,
                &bbox_edge_sidecar::snapshot::collected_snapshot_id(
                    project_id.as_str(),
                    generation_id,
                ),
            )?;
            state.nudge_edge_index_rebuild();
            Ok(())
        }
        ActivationTear::RecoverBackwardToManifest => {
            // The activation never completed and readers are correctly
            // serving the predecessor, so the MANIFEST wins and nothing here
            // rewrites it. The generation itself is durable and complete, so
            // it returns to Ready and reaches Active only through an ordinary
            // forward activation, never through a repair that assumes it was
            // already live.
            tracing::info!(
                generation = %generation_id,
                "connector activation was torn before its manifest write; readers stay on the \
                 predecessor and the generation returns to Ready"
            );
            store.set_state(scope, generation_id, FileGenerationStateV1::Ready, None)?;
            let scope = scope.clone();
            let generation_id = generation_id.to_string();
            enqueue_activation(state, scope, generation_id);
            Ok(())
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    //! The phase-1 gate, end to end: publish through the route layer, activate
    //! through the writer op, and prove the documents are reachable.
    //!
    //! The fixture bytes are the SYNTHETIC ones a real connector would
    //! publish, rendered by `bbox-file-collector` rather than invented here.
    //! That matters for the PDF and the image specifically: a test that
    //! indexed a `.md` file named `report.pdf` would prove the plumbing and
    //! nothing about whether the chunker registry claims what a document store
    //! actually sends.

    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use bbox_corpus_core::project_catalog::{
        ConnectorKind, ConnectorScope, ConnectorSourceId, CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_file_source::{
        BeginFileUploadRequestV1, BeginFileUploadResponseV1, FileGenerationDescriptorV1,
        FileGenerationStateV1, FileGenerationStatusV1, FileManifestEntryV1, FileManifestPageV1,
        FinalizeFileUploadResponseV1, MissingFileBlobsPageV1, PublicationTelemetryV1,
    };
    use sha2::{Digest, Sha256};
    use tantivy::TantivyDocument;
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::TermQuery;
    use tantivy::schema::{Field, IndexRecordOption, Term};
    use tower::ServiceExt as _;

    use super::*;
    use crate::server::connector_grants::ConnectorGrantRuntime;
    use crate::server::producer_auth::ProducerAuthRuntime;
    use crate::server::state::SharedState;

    const SOURCE_ID: &str = "csrc_5f2c1d9a4b6e470e";
    const PROJECT_ID: &str = "p_000000000000000000000000000000a1";
    const PRODUCER: &str = "producer-a";

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new(SOURCE_ID, "fixture").unwrap()
    }

    /// One published document, as the wire carries it.
    struct Document {
        logical_path: &'static str,
        bytes: Vec<u8>,
        remote_id: &'static str,
    }

    impl Document {
        fn entry(&self) -> FileManifestEntryV1 {
            FileManifestEntryV1 {
                logical_path: self.logical_path.to_string(),
                content_sha256: hex::encode(Sha256::digest(&self.bytes)),
                size: self.bytes.len() as u64,
                remote_id: self.remote_id.to_string(),
                remote_version: "v1".to_string(),
                remote_url: Some(format!("https://fixture.invalid/d/{}", self.remote_id)),
            }
        }
    }

    /// Strictly sorted by logical path, which the manifest contract requires.
    fn documents() -> Vec<Document> {
        vec![
            Document {
                logical_path: "images/diagram.png",
                bytes: bbox_file_collector::fixture::render_png(),
                remote_id: "remote-image",
            },
            Document {
                logical_path: "notes/plan.md",
                bytes: b"# Plan\n\nThe quarterly widget targets are unchanged.\n".to_vec(),
                remote_id: "remote-plan",
            },
            Document {
                logical_path: "notes/report.pdf",
                bytes: bbox_file_collector::fixture::render_pdf(
                    "Quarterly report: widget throughput exceeded the forecast.",
                ),
                remote_id: "remote-report",
            },
            // A folder named `target` and a dotted name are ORDINARY content
            // in a document store. The code lane's walker policy would drop
            // both, and their silent disappearance would be
            // indistinguishable from a broken connector.
            Document {
                logical_path: "target/quarterly.md",
                bytes: b"# Quarterly\n\nthis folder is not build output\n".to_vec(),
                remote_id: "remote-target",
            },
        ]
    }

    fn descriptor(
        entries: &[FileManifestEntryV1],
        cursor_epoch: u64,
    ) -> FileGenerationDescriptorV1 {
        FileGenerationDescriptorV1 {
            schema_version: bbox_file_source::SCHEMA_VERSION,
            connector_policy_version: bbox_file_source::CONNECTOR_POLICY_VERSION.to_string(),
            scope: scope(),
            remote_watermark: "watermark-1".to_string(),
            cursor_epoch,
            manifest_sha256: bbox_file_source::manifest_sha256(entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.size).sum(),
        }
    }

    fn token_file(root: &std::path::Path, value: &str) -> std::path::PathBuf {
        let path = root.join("producer.token");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    /// Install one connector producer grant into BOTH the daemon's config and
    /// its live auth table, and return the bearer.
    ///
    /// Writing it into `state.config` is not decoration. The onboard route
    /// promotes a freshly minted scope by REBUILDING producer auth from
    /// `state.config`, and other paths rebuild it too. A fixture that installs
    /// only the auth table leaves the daemon self-inconsistent -- the table
    /// says the connector family is enabled while the config says it is
    /// disabled -- so the first rebuild silently replaces the table with a
    /// disabled one and the NEXT request dies in the middleware's
    /// `enabled()` gate with a 503 that looks nothing like the fixture bug it
    /// actually is. Keeping the two agreeing is what makes the rebuild a
    /// no-op instead of a demolition.
    fn install_connectors(
        state: &Arc<SharedState>,
        root: &std::path::Path,
        catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    ) -> String {
        let token_secret = "a".repeat(64);
        let config = crate::config::SourceConnectorsConfig {
            enabled: true,
            producers: vec![crate::config::ConnectorProducerConfig {
                producer_id: PRODUCER.into(),
                token_file: token_file(root, &token_secret),
                token_files: Vec::new(),
                scopes: vec![crate::config::ConnectorScopeGrant {
                    connector_source_id: ConnectorSourceId::parse(SOURCE_ID).unwrap(),
                    connector_kind: ConnectorKind::parse("fixture").unwrap(),
                    remote_authority: "fixture.invalid".to_string(),
                    profile: crate::config::ConnectorProfile::File,
                }],
            }],
        };
        state.config.write().source_connectors = config.clone();
        let connectors = ConnectorGrantRuntime::build(&config, Some(catalog)).unwrap();
        state.code_sources.install_auth_for_test(Arc::new(
            ProducerAuthRuntime::for_test_connectors(Arc::new(connectors)),
        ));
        token_secret
    }

    /// A catalog-backed daemon whose only producer authority is a connector
    /// grant on [`SOURCE_ID`], already onboarded to a catalog project.
    fn onboarded_state(root: &std::path::Path) -> (Arc<SharedState>, String) {
        let catalog_root = root.join("catalog");
        std::fs::create_dir_all(&catalog_root).unwrap();
        let catalog_projects_path = catalog_root.join("projects.json");
        bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            &catalog_projects_path,
        )
        .unwrap();
        let state = Arc::new(SharedState::for_test_catalog(root, &catalog_projects_path));

        // Onboarding, as the catalog records it: a project whose scope IS the
        // connector scope. Nothing here fabricates a published scope or a
        // repo id, which is the whole reason the connector lane exists.
        let store = state.project_authority.catalog_store().unwrap().clone();
        let project_id = ProjectId::parse(PROJECT_ID.to_string()).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog, _attachments| {
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id: project_id.clone(),
                        scope: ProjectScope::Connector(scope()),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "Ops shared folder".into(),
                        created_at: "2026-08-13T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                Ok(())
            })
            .unwrap();

        let pinned = store.snapshot().unwrap();
        let token_secret = install_connectors(&state, root, pinned.catalog());
        assert!(
            !state
                .code_sources
                .producer_auth()
                .connectors()
                .is_pending_onboard(&scope()),
            "the fixture scope must be onboarded before any publication route admits it"
        );
        (state, token_secret)
    }

    fn request(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(body)
            .unwrap()
    }

    async fn json_body<T: serde::de::DeserializeOwned>(response: axum::response::Response) -> T {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Drive one whole publication through the ROUTE LAYER: begin, manifest,
    /// close, blobs, finalize. Returns the server-derived generation id.
    async fn publish(
        state: &Arc<SharedState>,
        token: &str,
        documents: &[Document],
        cursor_epoch: u64,
    ) -> String {
        let app = crate::server::file_source::router(state.clone()).with_state(state.clone());
        let entries: Vec<FileManifestEntryV1> =
            documents.iter().map(|document| document.entry()).collect();
        let begin = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, cursor_epoch),
            telemetry: PublicationTelemetryV1::default(),
            degradation: None,
        };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                token,
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let begun: BeginFileUploadResponseV1 = json_body(response).await;

        let page = FileManifestPageV1 {
            entries: entries.clone(),
        };
        let response = app
            .clone()
            .oneshot(request(
                "PUT",
                &format!(
                    "/internal/file-source/v1/uploads/{}/manifest/0",
                    begun.upload_id
                ),
                token,
                Body::from(serde_json::to_vec(&page).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/file-source/v1/uploads/{}/manifest/complete",
                    begun.upload_id
                ),
                token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let owed: MissingFileBlobsPageV1 = json_body(response).await;

        for hash in &owed.hashes {
            let document = documents
                .iter()
                .find(|document| &document.entry().content_sha256 == hash)
                .expect("the server may only ask for blobs the manifest names");
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!(
                            "/internal/file-source/v1/uploads/{}/blobs/{hash}",
                            begun.upload_id
                        ))
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(header::CONTENT_LENGTH, document.bytes.len())
                        .body(Body::from(document.bytes.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT, "blob {hash}");
        }

        let response = app
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/file-source/v1/uploads/{}/finalize",
                    begun.upload_id
                ),
                token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let finalized: FinalizeFileUploadResponseV1 = json_body(response).await;
        finalized.generation_id
    }

    /// Poll the status ROUTE until the generation reaches a terminal state.
    ///
    /// Polling rather than awaiting a handle on purpose: this is the exact
    /// contract a producer has. Finalize means the bytes are durable, and the
    /// only way to learn the activation outcome is this route.
    async fn await_terminal(
        state: &Arc<SharedState>,
        token: &str,
        generation_id: &str,
    ) -> FileGenerationStatusV1 {
        for _ in 0..600 {
            let app = crate::server::file_source::router(state.clone()).with_state(state.clone());
            let response = app
                .oneshot(request(
                    "GET",
                    &format!("/internal/file-source/v1/generations/{generation_id}/status"),
                    token,
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let status: FileGenerationStatusV1 = json_body(response).await;
            if status.state.is_terminal_success() || status.state.is_terminal_failure() {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("generation {generation_id} never reached a terminal state");
    }

    fn first_text(doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|value| match value {
                tantivy::schema::OwnedValue::Str(text) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Every document the index holds under one selector, as stored fields.
    fn documents_under(state: &Arc<SharedState>, selector: &str) -> Vec<TantivyDocument> {
        let index = state.idx.read();
        let reader = index.index_handle().reader().unwrap();
        reader.reload().unwrap();
        let searcher = reader.searcher();
        let fields = index.field_handles();
        let query = TermQuery::new(
            Term::from_field_text(fields.code_source_selector, selector),
            IndexRecordOption::Basic,
        );
        let total = searcher.search(&query, &Count).unwrap();
        searcher
            .search(&query, &TopDocs::with_limit(total.max(1)))
            .unwrap()
            .into_iter()
            .map(|(_, address)| searcher.doc(address).unwrap())
            .collect()
    }

    /// A daemon where the scope is GRANTED but not yet cataloged, which is
    /// the only state the onboard route admits and every publication route
    /// refuses.
    fn pending_state(root: &std::path::Path) -> (Arc<SharedState>, String) {
        let catalog_root = root.join("catalog");
        std::fs::create_dir_all(&catalog_root).unwrap();
        let catalog_projects_path = catalog_root.join("projects.json");
        bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            &catalog_projects_path,
        )
        .unwrap();
        let state = Arc::new(SharedState::for_test_catalog(root, &catalog_projects_path));
        let store = state.project_authority.catalog_store().unwrap().clone();
        let pinned = store.snapshot().unwrap();
        let token_secret = install_connectors(&state, root, pinned.catalog());
        assert!(
            state
                .code_sources
                .producer_auth()
                .connectors()
                .is_pending_onboard(&scope()),
            "a granted scope with no catalog project is pending onboarding"
        );
        (state, token_secret)
    }

    #[tokio::test]
    async fn onboarding_mints_the_connector_project_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, token) = pending_state(&root);
        let app = crate::server::file_source::router(state.clone()).with_state(state.clone());

        // A pending scope opens NO publication lane. This is the refusal that
        // makes "add the scope to the daemon config first" possible without
        // opening a lane for a project that does not exist.
        let entries: Vec<FileManifestEntryV1> = documents()
            .iter()
            .map(|document| document.entry())
            .collect();
        let begin = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, 0),
            telemetry: PublicationTelemetryV1::default(),
            degradation: None,
        };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "publishing under a pending scope is a conflict, deliberately \
             distinct from a forbidden one: they are different fixes"
        );

        let onboard = bbox_file_source::FileCatalogOnboardRequestV1 {
            schema_version: bbox_file_source::CATALOG_ONBOARD_SCHEMA_VERSION,
            scope: scope(),
            probed_connector_kind: "fixture".into(),
            probed_remote_authority: "fixture.invalid".into(),
            probed_remote_root_id: Some("root-1".into()),
            probed_remote_display_name: Some("Ops shared folder".into()),
            display_name: "Ops shared folder".into(),
        };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/catalog/onboard",
                &token,
                Body::from(serde_json::to_vec(&onboard).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let receipt: bbox_file_source::FileCatalogOnboardResponseV1 = json_body(response).await;
        assert!(receipt.created_project, "the first onboard mints it");

        // The catalog now owns the scope-to-project mapping, which is the
        // only authority the activation lane ever consults.
        let store = state.project_authority.catalog_store().unwrap().clone();
        let pinned = store.snapshot().unwrap();
        let project = pinned
            .catalog()
            .projects
            .values()
            .find(|project| matches!(&project.scope, ProjectScope::Connector(owned) if owned == &scope()))
            .expect("onboarding records a connector-scoped project");
        assert_eq!(project.project_id.as_str(), receipt.project_id);

        // Find-or-create: a producer driving onboard before every cycle is
        // safe to retry rather than duplicating the project.
        let response = app
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/catalog/onboard",
                &token,
                Body::from(serde_json::to_vec(&onboard).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let replayed: bbox_file_source::FileCatalogOnboardResponseV1 = json_body(response).await;
        assert_eq!(replayed.project_id, receipt.project_id);
        assert!(
            !replayed.created_project,
            "only the call that MINTED the project reports creating it"
        );
        assert_eq!(
            store.snapshot().unwrap().catalog().projects.len(),
            1,
            "a retried onboard is not a second project"
        );
    }

    #[tokio::test]
    async fn a_connector_generation_publishes_activates_and_becomes_searchable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        // The image chunker stores its payload in a PROCESS-GLOBAL visual
        // store. Without this guard the test writes a payload into the
        // developer's real state directory, and the OnceLock pins that path
        // for every later test in the binary.
        let _visual = bbox_visual_store::install_test_global(Arc::new(
            bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
        ));
        let (state, token) = onboarded_state(&root);
        let documents = documents();

        let generation_id = publish(&state, &token, &documents, 0).await;
        let status = await_terminal(&state, &token, &generation_id).await;
        assert_eq!(
            status.state,
            FileGenerationStateV1::Active,
            "diagnostic: {:?}",
            status.diagnostic
        );
        assert_eq!(status.file_count, documents.len() as u64);

        // The activation record names the catalog project, and the selector
        // is an ordinary collected one: a connector source is a collected
        // source to everything downstream of byte acquisition.
        let record = state
            .file_sources
            .store()
            .active_generation(&scope())
            .unwrap()
            .unwrap();
        assert_eq!(record.generation_id, generation_id);
        assert!(record.selector.starts_with("collected:"));
        assert!(record.document_count > 0);

        let selector = crate::index::project_files::collected_materialization_selector(
            PROJECT_ID,
            &generation_id,
        );
        let indexed = documents_under(&state, &selector);
        assert_eq!(
            indexed.len() as u64,
            record.document_count,
            "the activation record's count must be the index's count"
        );

        let fields = state.idx.read().field_handles();
        let paths: BTreeSet<String> = indexed
            .iter()
            .map(|doc| first_text(doc, fields.relative_path))
            .collect();
        for document in &documents {
            assert!(
                paths.contains(document.logical_path),
                "{} must be indexed; paths were {paths:?}",
                document.logical_path
            );
        }
        assert!(
            paths.contains("target/quarterly.md"),
            "a remote folder named `target` is ordinary content, not build \
             output; dropping it would be indistinguishable from a broken \
             connector"
        );

        for doc in &indexed {
            assert_eq!(
                first_text(doc, fields.source_kind),
                bbox_code_source::SOURCE_KIND_COLLECTED,
                "connector content is indexed as an ordinary collected source"
            );
            assert_eq!(first_text(doc, fields.project_id), PROJECT_ID);
            assert_eq!(
                first_text(doc, fields.commit_sha),
                "",
                "a connector source has no commit, and the display-only \
                 remote watermark must never be laundered into one"
            );
        }

        // The PDF is CHUNKED, not merely stored: its page text reached the
        // content field. This is what the synthetic fixture buys over a
        // renamed text file.
        let pdf_text: String = indexed
            .iter()
            .filter(|doc| first_text(doc, fields.relative_path) == "notes/report.pdf")
            .map(|doc| first_text(doc, fields.content))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            pdf_text.contains("widget throughput"),
            "the PDF's page text must be searchable, got: {pdf_text}"
        );

        // The image is claimed by the visual lane rather than chunked as
        // text, so its reachability is the image chunk existing under the
        // selector at all.
        let image_kinds: Vec<String> = indexed
            .iter()
            .filter(|doc| first_text(doc, fields.relative_path) == "images/diagram.png")
            .map(|doc| first_text(doc, fields.chunk_kind))
            .collect();
        assert_eq!(
            image_kinds,
            vec!["image".to_string()],
            "a standalone image is one visual chunk under the connector's \
             selector"
        );
    }

    #[tokio::test]
    async fn a_second_generation_supersedes_the_first_on_the_read_plane() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let _visual = bbox_visual_store::install_test_global(Arc::new(
            bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
        ));
        let (state, token) = onboarded_state(&root);
        // Selector retirement deletes the superseded generation's vectors
        // alongside its documents, so it REFUSES with a retryable
        // "vector store is still warming up" while no global store is
        // installed. A for_test daemon never warms one on its own, so without
        // this the retirement defers forever and the assertion below can only
        // ever time out. The code lane's own restart test installs the
        // daemon's store the same way.
        let _vectors = bbox_vectors::install_test_global(state.vector_store.clone());

        let first_documents = documents();
        let first = publish(&state, &token, &first_documents, 0).await;
        assert_eq!(
            await_terminal(&state, &token, &first).await.state,
            FileGenerationStateV1::Active
        );
        let first_selector =
            crate::index::project_files::collected_materialization_selector(PROJECT_ID, &first);

        let mut second_documents = documents();
        second_documents[1].bytes =
            b"# Plan\n\nThe quarterly widget targets were revised upward.\n".to_vec();
        let second = publish(&state, &token, &second_documents, 0).await;
        assert_ne!(second, first, "changed bytes are a distinct generation");
        assert_eq!(
            await_terminal(&state, &token, &second).await.state,
            FileGenerationStateV1::Active
        );

        let second_selector =
            crate::index::project_files::collected_materialization_selector(PROJECT_ID, &second);
        assert!(
            !documents_under(&state, &second_selector).is_empty(),
            "the new generation is populated"
        );

        // Every generation mints its OWN selector, so activation does not
        // overwrite the predecessor's documents; they are deleted by an
        // explicit retirement that runs AFTER the state flip. The flip is what
        // a producer observes, so polling here is not papering over a race: it
        // is the honest statement that cleanup follows publication rather than
        // riding inside it.
        let mut retired = false;
        for _ in 0..600 {
            if documents_under(&state, &first_selector).is_empty() {
                retired = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            retired,
            "the superseded generation's documents must be retired; leaving \
             them behind accumulates one whole corpus per publication cycle"
        );

        // The predecessor is RETAINED, not pruned: it is where a tear between
        // the activation record and the derived manifest recovers backward to.
        let store = state.file_sources.store();
        let record = store.active_generation(&scope()).unwrap().unwrap();
        assert_eq!(record.generation_id, second);
        assert_eq!(
            record.superseded_generation_id.as_deref(),
            Some(first.as_str())
        );
        assert!(store.load_generation(&scope(), &first).unwrap().is_some());
        assert_eq!(
            store
                .load_generation(&scope(), &first)
                .unwrap()
                .unwrap()
                .state,
            FileGenerationStateV1::Superseded
        );
    }

    #[tokio::test]
    async fn a_replayed_publication_resumes_without_reuploading_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let _visual = bbox_visual_store::install_test_global(Arc::new(
            bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
        ));
        let (state, token) = onboarded_state(&root);
        let documents = documents();
        let app = crate::server::file_source::router(state.clone()).with_state(state.clone());
        let entries: Vec<FileManifestEntryV1> =
            documents.iter().map(|document| document.entry()).collect();
        let begin = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, 0),
            telemetry: PublicationTelemetryV1::default(),
            degradation: None,
        };

        // A first attempt gets as far as one blob, then the producer dies.
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        let begun: BeginFileUploadResponseV1 = json_body(response).await;
        let page = FileManifestPageV1 {
            entries: entries.clone(),
        };
        app.clone()
            .oneshot(request(
                "PUT",
                &format!(
                    "/internal/file-source/v1/uploads/{}/manifest/0",
                    begun.upload_id
                ),
                &token,
                Body::from(serde_json::to_vec(&page).unwrap()),
            ))
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/file-source/v1/uploads/{}/manifest/complete",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        let owed: MissingFileBlobsPageV1 = json_body(response).await;
        assert_eq!(owed.hashes.len(), documents.len());
        let landed = owed.hashes[0].clone();
        let document = documents
            .iter()
            .find(|document| document.entry().content_sha256 == landed)
            .unwrap();
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!(
                        "/internal/file-source/v1/uploads/{}/blobs/{landed}",
                        begun.upload_id
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_LENGTH, document.bytes.len())
                    .body(Body::from(document.bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Restart: the same descriptor derives the same upload, and the owed
        // set is RECOMPUTED from the content-addressed store rather than
        // replayed, so the blob that landed is not re-requested.
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        let resumed: BeginFileUploadResponseV1 = json_body(response).await;
        assert_eq!(
            resumed.upload_id, begun.upload_id,
            "an exact replay resumes"
        );
        assert_eq!(
            resumed.ordinal, begun.ordinal,
            "and a replay does not advance the ordinal"
        );
        let response = app
            .oneshot(request(
                "GET",
                &format!(
                    "/internal/file-source/v1/uploads/{}/missing",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        let still_owed: MissingFileBlobsPageV1 = json_body(response).await;
        assert_eq!(still_owed.hashes.len(), documents.len() - 1);
        assert!(
            !still_owed.hashes.contains(&landed),
            "a blob that landed before the restart is never re-requested"
        );
    }

    #[tokio::test]
    async fn a_torn_activation_is_classified_and_reconciled_at_boot() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let _visual = bbox_visual_store::install_test_global(Arc::new(
            bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
        ));
        let (state, token) = onboarded_state(&root);
        let documents = documents();
        let generation_id = publish(&state, &token, &documents, 0).await;
        assert_eq!(
            await_terminal(&state, &token, &generation_id).await.state,
            FileGenerationStateV1::Active
        );

        let store = state.file_sources.store();
        // Model the crash between the derived manifest and the state flip:
        // both sides name this generation, but the flip was lost.
        store
            .set_state(
                &scope(),
                &generation_id,
                FileGenerationStateV1::StagingIndex,
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .classify_tear(&scope(), Some(&generation_id))
                .unwrap()
                .unwrap(),
            bbox_file_source_store::ActivationTear::RecoverForwardReplayStateFlip
        );

        recover_connector_activations(&state);
        assert_eq!(
            store
                .load_generation(&scope(), &generation_id)
                .unwrap()
                .unwrap()
                .state,
            FileGenerationStateV1::Active,
            "a lost flip replays forward; this store is the authority"
        );
        assert_eq!(
            store
                .classify_tear(&scope(), Some(&generation_id))
                .unwrap()
                .unwrap(),
            bbox_file_source_store::ActivationTear::Converged
        );
    }

    #[tokio::test]
    async fn a_pending_scope_publishes_nothing_and_an_unknown_producer_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (state, token) = onboarded_state(&root);
        let app = crate::server::file_source::router(state.clone()).with_state(state.clone());
        let entries: Vec<FileManifestEntryV1> = documents()
            .iter()
            .map(|document| document.entry())
            .collect();
        let begin = BeginFileUploadRequestV1 {
            descriptor: descriptor(&entries, 0),
            telemetry: PublicationTelemetryV1::default(),
            degradation: None,
        };

        // No credential at all stops at the authorization boundary, before
        // any body is parsed.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/file-source/v1/uploads")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A wrong bearer is equally refused, and the real one still works.
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                &"b".repeat(64),
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(request(
                "POST",
                "/internal/file-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&begin).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    /// The retirement redrive (gap-7e44ee3b): a selector retirement deferred
    /// by the vector-store readiness gate is journaled at defer time and
    /// redriven to completion once the gate opens, rather than stranding the
    /// superseded generation's corpus forever.
    mod retirement_redrive {
        use std::time::Duration;

        use super::*;

        /// Poll a predicate on a bounded schedule, matching the polling style
        /// the rest of this module's fixtures use for "eventually true"
        /// background-thread outcomes.
        async fn await_true(mut predicate: impl FnMut() -> bool, what: &str) {
            for _ in 0..600 {
                if predicate() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("{what} never became true");
        }

        #[tokio::test]
        async fn a_deferred_retirement_is_journaled_and_redriven_once_the_vector_store_warms() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let _visual = bbox_visual_store::install_test_global(Arc::new(
                bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
            ));
            let (state, token) = onboarded_state(&root);
            // Deliberately NOT installing the vector store global yet: a
            // superseded selector with documents under it must defer on
            // `try_global()` returning `None`, exactly the "vector store is
            // still warming up during post-boot activation" shape the gap
            // names.

            let first_documents = documents();
            let first = publish(&state, &token, &first_documents, 0).await;
            assert_eq!(
                await_terminal(&state, &token, &first).await.state,
                FileGenerationStateV1::Active
            );
            let first_selector =
                crate::index::project_files::collected_materialization_selector(PROJECT_ID, &first);
            assert!(
                !documents_under(&state, &first_selector).is_empty(),
                "the first generation must have documents for its retirement \
                 to be anything other than the count==0 short-circuit"
            );

            let mut second_documents = documents();
            second_documents[1].bytes =
                b"# Plan\n\nThe quarterly widget targets were revised upward.\n".to_vec();
            let second = publish(&state, &token, &second_documents, 0).await;
            assert_eq!(
                await_terminal(&state, &token, &second).await.state,
                FileGenerationStateV1::Active
            );

            // The activation completed, but retirement of the first
            // generation's selector must have deferred: the store's
            // journal names it, and its documents are still there.
            let store = state.file_sources.store();
            await_true(
                || store.retirement_pending(&first_selector).unwrap(),
                "the deferred retirement was journaled",
            )
            .await;
            assert!(
                !documents_under(&state, &first_selector).is_empty(),
                "a deferred retirement must not have deleted anything"
            );

            // The vector store finishes warming, and the redrive completes
            // the deferral it recorded.
            let _vectors = bbox_vectors::install_test_global(state.vector_store.clone());
            notify_connector_retirement_readiness_available(&state);

            // Poll the JOURNAL, not the index, as the completion signal.
            // `run_connector_retirement_attempt` deletes the selector's
            // documents and only THEN clears the journal row, so waiting on
            // the row (the later of the two writes) is race-free; polling
            // `documents_under` first and asserting the journal afterward
            // with no wait raced the two writes directly and could observe
            // the index already clear while the coordinator thread had not
            // yet reached `complete_retirement`.
            await_true(
                || !store.retirement_pending(&first_selector).unwrap(),
                "the redrive completed the deferred retirement and cleared its journal row",
            )
            .await;
            assert!(
                documents_under(&state, &first_selector).is_empty(),
                "a completed retirement must have removed the selector's documents"
            );
        }

        #[tokio::test]
        async fn stacked_deferrals_from_several_activations_are_all_redriven_to_completion() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let _visual = bbox_visual_store::install_test_global(Arc::new(
                bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
            ));
            let (state, token) = onboarded_state(&root);
            // No vector store installed: every activation below supersedes a
            // predecessor with documents, so every one of them defers.

            let mut selectors = Vec::new();
            let first = publish(&state, &token, &documents(), 0).await;
            assert_eq!(
                await_terminal(&state, &token, &first).await.state,
                FileGenerationStateV1::Active
            );
            let mut previous_id = first;

            // Three more activations, each superseding the last, all landing
            // while the vector store is still warming -- the "several
            // activations during one long warmup" shape. Each revision's
            // bytes differ from every other so each publish derives its own
            // generation id.
            for revision in 1..=3u8 {
                let selector = crate::index::project_files::collected_materialization_selector(
                    PROJECT_ID,
                    &previous_id,
                );
                selectors.push(selector);

                let mut next_documents = documents();
                next_documents[1].bytes = format!(
                    "# Plan\n\nThe quarterly widget targets were revised {revision} times.\n"
                )
                .into_bytes();
                let next = publish(&state, &token, &next_documents, 0).await;
                assert_eq!(
                    await_terminal(&state, &token, &next).await.state,
                    FileGenerationStateV1::Active
                );
                previous_id = next;
            }

            let store = state.file_sources.store();
            for selector in &selectors {
                await_true(
                    || store.retirement_pending(selector).unwrap(),
                    "every superseded selector from a stacked-deferral run was journaled",
                )
                .await;
                assert!(!documents_under(&state, selector).is_empty());
            }
            assert_eq!(
                store.retirement_records().unwrap().len(),
                selectors.len(),
                "every deferral in the stack must have its own journal row"
            );

            let _vectors = bbox_vectors::install_test_global(state.vector_store.clone());
            notify_connector_retirement_readiness_available(&state);

            // Poll the JOURNAL as the completion signal, not the index: the
            // coordinator's single background thread drains due entries in a
            // tight loop after one nudge (it does not require one nudge per
            // row), but `run_connector_retirement_attempt` deletes a
            // selector's documents strictly before it clears that selector's
            // journal row. Waiting on `documents_under` per selector and then
            // asserting the whole journal empty with no further wait raced
            // the LAST selector's own two writes: its documents could already
            // read as gone while its journal row had not yet been removed,
            // which is exactly the flake the cluster caught (documents_under
            // for the last selector observed the tantivy commit slightly
            // ahead of complete_retirement's own separate fsync). The journal
            // row is written after the document deletion in every case, so
            // waiting on "every row cleared" is the race-free, and strictly
            // later, signal; checking `documents_under` afterward can never
            // fail once the journal has drained.
            await_true(
                || store.retirement_records().unwrap().is_empty(),
                "the redrive completed every stacked deferral, not just the most recent",
            )
            .await;
            for selector in &selectors {
                assert!(
                    documents_under(&state, selector).is_empty(),
                    "a completed retirement must have removed the selector's documents"
                );
            }
        }

        #[tokio::test]
        async fn redriving_an_already_retired_connector_selector_is_a_no_op() {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let _visual = bbox_visual_store::install_test_global(Arc::new(
                bbox_visual_store::VisualPayloadStore::open(root.join("visual")),
            ));
            let (state, token) = onboarded_state(&root);
            // Vectors installed up front: this generation's retirement
            // succeeds on the first attempt, so the case under test is a
            // REDRIVE of a selector that already has nothing left to do.
            let _vectors = bbox_vectors::install_test_global(state.vector_store.clone());

            let first_documents = documents();
            let first = publish(&state, &token, &first_documents, 0).await;
            assert_eq!(
                await_terminal(&state, &token, &first).await.state,
                FileGenerationStateV1::Active
            );
            let first_selector =
                crate::index::project_files::collected_materialization_selector(PROJECT_ID, &first);

            let mut second_documents = documents();
            second_documents[1].bytes =
                b"# Plan\n\nThe quarterly widget targets were revised upward.\n".to_vec();
            let second = publish(&state, &token, &second_documents, 0).await;
            assert_eq!(
                await_terminal(&state, &token, &second).await.state,
                FileGenerationStateV1::Active
            );

            await_true(
                || documents_under(&state, &first_selector).is_empty(),
                "retirement completed synchronously since the vector store was already warm",
            )
            .await;
            let store = state.file_sources.store();
            assert!(!store.retirement_pending(&first_selector).unwrap());

            // Redrive the same identity directly, as boot recovery or a
            // racing coordinator entry would. Both the writer's count==0
            // short-circuit and the store's missing-row no-op must make this
            // a clean success, not an error.
            let record = ConnectorRetirementRecord {
                version: 1,
                project_id: PROJECT_ID.to_string(),
                scope: scope(),
                superseded_generation_id: first.clone(),
                selector: first_selector.clone(),
                reason: "idempotency probe".to_string(),
            };
            for _ in 0..2 {
                let outcome = run_connector_retirement_attempt(&state, &record);
                assert!(
                    matches!(outcome, ConnectorRetirementAttempt::Complete),
                    "redriving an already-retired selector must succeed as a no-op"
                );
            }
            assert!(!store.retirement_pending(&first_selector).unwrap());
        }
    }
}
