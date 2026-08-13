use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use rmcp::handler::server::router::tool::ToolRouter;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::gaps::GapStore;
use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::notes::Notes;
use crate::orchestration::tail::TailEvent;
use crate::orchestration::{self, TaskStore};
use crate::packets::Packets;
use crate::pins::Pins;
use crate::projects::ProjectRegistry;
use crate::roadmap::Roadmap;
use crate::store_persister::StorePersister;
use crate::threads::Threads;
use crate::{
    artifacts, crons, edge_index, path_cache, pollers, slack_channel_bindings,
    slack_proposal_links, slack_thread_store, system_events, webhooks, whiteboards, workflow,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[cfg(test)]
const ROSTER_BROADCAST_BUFFER: usize = 1024;

/// The daemon's project authority, fixed for the process lifetime by the
/// startup store-version probe (phase-2 §4.1). Bridge mode is today's
/// version-1 registry plus its write-behind persister; catalog mode is the
/// strict pair store, whose mutations commit only through the journaled
/// pair transaction.
pub(crate) enum ProjectAuthority {
    Bridge {
        registry: Arc<RwLock<ProjectRegistry>>,
        persister: StorePersister<ProjectRegistry>,
    },
    Catalog {
        store: Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>,
    },
}

impl ProjectAuthority {
    /// The version-1 registry for the surviving bridge-only mutators and
    /// v1-resolve guards. Catalog mode refuses with a typed diagnostic:
    /// each of these surfaces gains its catalog semantics in its own
    /// phase-2 milestone, and until then the operation is unsupported
    /// there rather than silently misrouted.
    pub(crate) fn bridge_registry(&self) -> anyhow::Result<&Arc<RwLock<ProjectRegistry>>> {
        match self {
            ProjectAuthority::Bridge { registry, .. } => Ok(registry),
            ProjectAuthority::Catalog { .. } => anyhow::bail!(
                "error.project_catalog_lifecycle_pending: this operation still uses the \
                 version-1 registry and is not yet available in catalog mode"
            ),
        }
    }

    /// True when the version-1 registry is the runtime authority. The
    /// compatibility lanes (unregistered-write pass-through, literal filter
    /// fallbacks) exist only on this arm; catalog mode fails closed instead.
    pub(crate) fn is_bridge(&self) -> bool {
        matches!(self, ProjectAuthority::Bridge { .. })
    }

    /// The strict pair store when the catalog is the runtime authority.
    /// Consumed by the administration tools' catalog operations.
    pub(crate) fn catalog_store(
        &self,
    ) -> Option<&Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>> {
        match self {
            ProjectAuthority::Bridge { .. } => None,
            ProjectAuthority::Catalog { store } => Some(store),
        }
    }
}

pub(crate) struct SharedState {
    pub(crate) idx: RwLock<TranscriptIndex>,
    /// Handle to the daemon's single tantivy writer actor. All production
    /// index mutations and reindex passes flow through it; nothing else in
    /// the process opens an `IndexWriter` (concurrency-model §4.3).
    pub(crate) index_writer: crate::index::IndexWriterActor,
    pub(crate) kb: Arc<RwLock<Knowledge>>,
    pub(crate) kb_persister: StorePersister<Knowledge>,
    /// First-class substrate gap-note store. Project-scoped gaps are repo-owned
    /// (one file per gap under `<project>/.bbox/gaps/`); global gaps live in the
    /// central host store. Mirrors the `kb` repo-owned model.
    pub(crate) gaps: RwLock<GapStore>,
    pub(crate) roadmap: Arc<RwLock<Roadmap>>,
    pub(crate) roadmap_persister: StorePersister<Roadmap>,
    pub(crate) threads: Arc<RwLock<Threads>>,
    pub(crate) threads_persister: StorePersister<Threads>,
    pub(crate) notes: Arc<RwLock<Notes>>,
    pub(crate) notes_persister: StorePersister<Notes>,
    pub(crate) pins: Arc<RwLock<Pins>>,
    pub(crate) pins_persister: StorePersister<Pins>,
    /// Durable pending checkout mutations (repo-owned file writes the
    /// daemon validated but cannot apply; the checkout-owner collector
    /// polls and acks them over the producer channel).
    pub(crate) checkout_mutations: Arc<RwLock<crate::checkout_mutations::CheckoutMutations>>,
    pub(crate) checkout_mutations_persister:
        StorePersister<crate::checkout_mutations::CheckoutMutations>,
    /// The runtime project authority selected by the startup store-version
    /// probe (phase-2 §4.1). Consumers never match this directly outside
    /// the defined seams: record enumeration goes through
    /// `records_provider`, checkout access through the broker, and the
    /// remaining version-1 mutators call `bridge_registry()`.
    pub(crate) project_authority: ProjectAuthority,
    /// The accepted-publication runtime facade, opened and scanned before
    /// the listener bind (Phase 5 plan section 5.4). `None` in bridge mode:
    /// bridge published reads keep the legacy publisher authority, and no
    /// bridge caller constructs this runtime.
    pub(crate) accepted_publications:
        Option<Arc<bbox_indexing::accepted_publication_runtime::AcceptedPublicationRuntime>>,
    /// Injected project-record authority handed to every runtime consumer that
    /// only enumerates records (index writer, index selectors, providers).
    pub(crate) records_provider: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider>,
    /// Host-local discovery index for scope-aware checkout overlays.
    pub(crate) checkout_registry: Arc<RwLock<bbox_indexing::checkout_registry::CheckoutRegistry>>,
    /// Bounded, path-free evidence for every checkout lease acquisition and
    /// denial. Broker instances share this handle while authority adapters
    /// remain owned by the consumer being migrated.
    pub(crate) checkout_access_observations:
        bbox_indexing::checkout_access::CheckoutAccessObservations,
    /// Per-surface resolver compatibility-lane counters (phase-2 §9.2):
    /// the observations the Phase 6 compatibility cut consumes.
    pub(crate) resolver_compat: crate::server::resolver_compat::ResolverCompatObservations,
    /// Single daemon-owned checkout authority. Every checkout consumer reuses
    /// this broker so counters and authority state cannot diverge per call.
    pub(crate) checkout_access: Arc<bbox_indexing::checkout_access::CheckoutAccessBroker>,
    /// Durable operation and shadow-parity evidence for knowledge transport.
    /// Checkout observations prove the absence of local leases; this store
    /// proves the remote result matched its overlap reference.
    pub(crate) knowledge_transport_observations:
        bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationsV1,
    /// Durable positive-use and shadow-parity evidence for checkout-local
    /// blame execution. Contains identity and response checksums only.
    pub(crate) blame_locality_observations:
        bbox_indexing::blame_locality_observations::BlameLocalityObservationsV1,
    /// Durable exact-receipt evidence for checkout-owned project renders,
    /// keyed by project and explicit published/own/all view.
    pub(crate) render_locality_observations:
        bbox_indexing::render_locality_observations::RenderLocalityObservationsV1,
    /// Host-local symbolic branch pins defining published truth per scope.
    pub(crate) publisher_refs: RwLock<bbox_indexing::publisher::PublisherRefStore>,
    /// Session-authorized provisional snapshots keyed by scope and checkout.
    pub(crate) knowledge_overlays: RwLock<bbox_knowledge::overlay::KnowledgeOverlayStore>,
    /// Gap-store provisional snapshots using the same scope and checkout keys.
    pub(crate) gap_overlays: RwLock<bbox_gaps::overlay::GapOverlayStore>,
    /// Serializes checkout recomputation through publication and index
    /// convergence so an older refresh cannot finish after a newer one.
    pub(crate) knowledge_overlay_refresh: Mutex<()>,
    pub(crate) gap_overlay_refresh: Mutex<()>,
    /// Monotonic local migration gate. Once true, path strings remain input
    /// selectors only and can never regain project-scope authority.
    pub(crate) path_fallback_cut: AtomicBool,
    /// Published committed-tree snapshots keyed by the immutable commit
    /// returned from publisher authorization. A moved ref changes that key,
    /// while explicit convergence invalidates the affected scope immediately.
    pub(crate) knowledge_published_cache: RwLock<
        BTreeMap<
            bbox_corpus_core::identity::PublishedScope,
            super::knowledge_view::PublishedKnowledgeCacheEntry,
        >,
    >,
    /// Published gap snapshots use the same immutable-commit cache as
    /// knowledge so hot read surfaces do not spawn one Git process per file.
    pub(crate) gap_published_cache: RwLock<
        BTreeMap<
            bbox_corpus_core::identity::PublishedScope,
            super::gap_view::PublishedGapCacheEntry,
        >,
    >,
    /// Catalog published knowledge projected from verified accepted
    /// content, keyed by durable project identity and validated by the
    /// accepted content stamp. Separate from the bridge cache above: that
    /// one is scope-keyed and bound to publisher election, this one has no
    /// publisher, no TTL, and no path.
    pub(crate) catalog_knowledge_published_cache: RwLock<
        BTreeMap<
            bbox_corpus_core::project_catalog::ProjectId,
            super::knowledge_view::CatalogPublishedKnowledgeCacheEntry,
        >,
    >,
    /// The gap twin of `catalog_knowledge_published_cache`.
    pub(crate) catalog_gap_published_cache: RwLock<
        BTreeMap<
            bbox_corpus_core::project_catalog::ProjectId,
            super::gap_view::CatalogPublishedGapCacheEntry,
        >,
    >,
    /// Accepted project graphs plus whole-graph provisional overlays. B2 read
    /// tools consume this source-neutral catalog without reopening transport.
    pub(crate) project_graph_views:
        RwLock<bbox_indexing::project_graph_view::ProjectGraphViewCatalog>,
    /// Successful publisher authority resolutions are memoized briefly.
    /// The project inventory is part of each cache entry, so registry changes
    /// bypass the cached decision immediately.
    pub(crate) publisher_authorization_cache:
        RwLock<super::knowledge_lifecycle::PublisherAuthorizationCache>,
    pub(crate) packets: RwLock<Packets>,
    /// Generation-validated cache of MCP surface decisions; see
    /// `server::surface::SurfaceDecisionCache`. Keeps the wire head from
    /// re-reading the packet store on every request.
    pub(crate) surface_decisions: crate::server::surface::SurfaceDecisionCache,
    pub(crate) artifacts: RwLock<artifacts::ArtifactCatalog>,
    pub(crate) bbox_watcher: std::sync::Mutex<Option<crate::watcher::BbxWatcher>>,
    /// Out-of-band trigger for the background reindex thread. The `.bbox/knowledge`
    /// watcher (and daemon startup) set it so repo-owned knowledge changes that
    /// `needs_reindex` does not track still drive one incremental search pass.
    /// Shared with the reindex thread (same `Arc`).
    pub(crate) reindex_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) code_read_view: RwLock<Arc<CodeReadView>>,
    /// True only after the process has published one complete EdgeIndex view.
    /// Deferred startup must never make an empty placeholder look like a
    /// valid graph to callers.
    pub(crate) edge_index_ready: AtomicBool,
    pub(crate) code_sources: Arc<super::code_source::CodeSourceRuntime>,
    /// Connector generation store for the `/internal/file-source/v1/*` lane.
    /// Separate from `code_sources` because the generation model, the key,
    /// and the retention window are this lane's own.
    pub(crate) file_sources: Arc<super::file_source::FileSourceRuntime>,
    /// Conversation landing store for the `/internal/conversation-source/v1/*`
    /// lane. Separate from `file_sources` because this lane has no
    /// generation model at all: records land append-only against
    /// server-owned per-channel cursors.
    pub(crate) conversation_sources: Arc<super::conversation_source::ConversationSourceRuntime>,
    pub(crate) git_sources: Arc<super::git_source::GitSourceRuntime>,
    pub(crate) knowledge_sources: Arc<super::knowledge_source::KnowledgeSourceRuntime>,
    /// Strict per-repository Git transport authority loaded from the
    /// checksummed current cutover marker before the first catalog read view.
    /// Bridge mode and pre-cutover catalog mode carry an empty runtime.
    pub(crate) git_transport_cutover:
        Arc<bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1>,
    /// Strict per-project knowledge transport authority. Any present row is a
    /// monotonic no-fallback boundary even while it is pending re-cutover.
    pub(crate) knowledge_transport_cutover:
        Arc<bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1>,
    /// Strict per-project blame locality authority. A checksummed marker row
    /// makes checkout-local execution mandatory before the legacy adapter can
    /// acquire a daemon-side checkout lease.
    pub(crate) blame_locality_cutover:
        Arc<bbox_indexing::blame_locality_cutover::BlameLocalityCutoverRuntimeV1>,
    /// Strict per-project render locality authority. A checksummed marker row
    /// prevents any unbound daemon project-render adapter from reacquiring a
    /// checkout after the measured cut.
    pub(crate) render_locality_cutover:
        Arc<bbox_indexing::render_locality_cutover::RenderLocalityCutoverRuntimeV1>,
    /// Strict per-project collected-source authority. A checksummed marker
    /// closes LocalProjectWalk and removes local cutback as a valid fallback.
    pub(crate) code_source_locality_cutover:
        Arc<bbox_indexing::code_source_locality_cutover::CodeSourceLocalityCutoverRuntimeV1>,
    /// Shutdown flag for the cutback reconciler background task (P4-D).
    /// `None` in bridge mode (no reconciler spawned).
    pub(crate) reconciler_shutdown: parking_lot::RwLock<Arc<std::sync::atomic::AtomicBool>>,
    /// Out-of-band wake for the edge-index rebuild watcher. Async tool
    /// handlers whose store mutations change projected edges (bbox_thread
    /// link, project unregister) nudge instead of rebuilding inline — a
    /// rebuild parses the multi-GB sidecar lanes and must not run on a
    /// tokio worker. 1-slot channel + try_send coalesces bursts; the
    /// watcher rebuild picks up every store mutation made before it runs.
    pub(crate) edge_rebuild_nudge_tx: std::sync::mpsc::SyncSender<()>,
    /// Receiver half, taken once by `spawn_edge_index_rebuild_watcher`.
    pub(crate) edge_rebuild_nudge_rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    pub(crate) path_cache: RwLock<path_cache::PathCache>,
    pub(crate) task_store: Arc<RwLock<TaskStore>>,
    pub(crate) tail_tx: broadcast::Sender<TailEvent>,
    pub(crate) roster_version: Arc<AtomicU64>,
    pub(crate) roster_tx: broadcast::Sender<bro_protocol::RosterDelta>,
    /// Daemon-side cache of `RosterSummaryV1` projections. Ingest
    /// maintains it from the same call sites that emit `RosterDelta`
    /// events (via `RosterEventSink`); `/control/roster` reads it
    /// directly to avoid locking every task's inner mutex on each
    /// fleet poll. See `src/orchestration/mod.rs::RosterView`.
    pub(crate) roster_view: Arc<orchestration::RosterView>,
    pub(crate) store_dir: PathBuf, // BRO_HOME (default: ~/.local/state/blackbox/bro)
    /// In-flight workflow arcs keyed by `arc_thread_id`. Updated at
    /// every node boundary by the engine so /orchestrate/peek can
    /// report the live state without reading notes. Entries persist
    /// after the arc terminates so a peek shortly after close still
    /// works (they stay until the daemon restarts).
    pub(crate) running_arcs: RwLock<HashMap<String, ArcSnapshot>>,
    /// Pending Wait-node registrations indexed by signal name +
    /// correlation. Webhook router and direct `bbox_arc_signal` MCP
    /// calls write into this; suspended arcs block on the per-wait
    /// Notify until a matching signal arrives.
    pub(crate) wait_store: Arc<crate::workflow::wait::WaitStore>,
    /// Operator-installed webhook endpoints. Each carries its
    /// signature scheme + extractor + routing-packet id.
    pub(crate) webhooks: webhooks::SharedRegistry,
    /// Operator-installed pollers — scheduled HTTP-source inlets
    /// that converge on the same `dispatch_routed_event` pipeline as
    /// webhooks. Carries running-task handles so they can be aborted
    /// on uninstall / replaced on reinstall.
    pub(crate) pollers: pollers::SharedRegistry,
    /// Operator-installed crons — calendar-driven inlets (sibling to
    /// webhooks/pollers). Same `dispatch_routed_event` convergence;
    /// distinct registry because the spec shape and concurrency model
    /// differ (pollers fetch HTTP per tick; crons dispatch arcs by
    /// schedule and gate concurrency per-cron).
    pub(crate) crons: crons::SharedRegistry,
    /// Whiteboards — multi-agent deliberation boards shared between
    /// in-workflow ensembles, in-workflow facilitators, and external
    /// agents (operator's Claude, dispatched help, eventually humans
    /// through slack/ntfy adapters). Phase transitions emit routed
    /// signals through `dispatch_routed_event` so wait_for_phase
    /// nodes resume on the same pipeline webhook ingress uses.
    pub(crate) whiteboards: whiteboards::SharedRegistry,
    /// Operator-installed workflow specs by id. Allows
    /// `start_arc{workflow: "name"}` routing verdicts to find their
    /// target without the webhook payload carrying the full spec.
    pub(crate) workflow_registry: Arc<RwLock<HashMap<String, workflow::Workflow>>>,
    /// True iff the daemon's HTTP listener is bound to a loopback
    /// address. Webhook signature scheme `none` is rejected at install
    /// AND at verify when this is false (defense in depth).
    pub(crate) bind_is_loopback: bool,
    /// Bounded ring buffer of recent signal-dispatch events. Every
    /// call to `signal_arc_dispatch` records one entry — whether the
    /// signal matched a pending wait (with the resolved arc/wait ids)
    /// or fell idle (with the pending-with-same-signal snapshot at
    /// dispatch time). Surfaced via `bro_signals` MCP for debugging
    /// "did this webhook actually resolve a wait?" without grepping
    /// the daemon's tracing log.
    pub(crate) signal_log: RwLock<std::collections::VecDeque<SignalEvent>>,
    /// Bounded ring buffer of recent webhook deliveries. Captured by
    /// the webhook handler post-dispatch; carries the extracted
    /// entity, the routing verdict's classification, and the response
    /// returned to the caller. Surfaced via `bro_webhook_deliveries`
    /// MCP — replaces poking the upstream's hook-task table or
    /// reading daemon tracing logs to debug routing-rule misses.
    pub(crate) webhook_delivery_log: RwLock<std::collections::VecDeque<WebhookDelivery>>,
    /// Cancellation tokens for in-flight workflow arcs, keyed by
    /// `arc_id`. Created at run start, removed at terminus. The
    /// `bro_arc_cancel` MCP tool and the `cancel_arc` routing verdict
    /// look up the token and trigger `cancel()`; the runner observes
    /// the token between node iterations and inside Wait suspensions
    /// (via `tokio::select!`), bails out with status `cancelled`, and
    /// runs `on_arc_cancel` + `on_arc_exit` hooks on the way out.
    pub(crate) arc_cancel_tokens: RwLock<HashMap<String, CancellationToken>>,
    /// Daemon-wide resume lease registry keyed `(provider, session_id)`.
    /// All resume paths must acquire this before spawning a provider
    /// resume process and hold it until the task reaches a terminal
    /// state. Concurrent resumes on the same provider session race
    /// transcript writes and can fork/corrupt the session.
    pub(crate) resume_leases: Arc<orchestration::resume_lease::ResumeLeaseRegistry>,
    /// Agent dispatch adapter registry. Initialized before artifact
    /// catalog opens so AS-I1 validation can check dispatch_adapter
    /// membership against the live registry.
    pub(crate) agent_adapter_registry:
        Arc<RwLock<orchestration::agents::adapter::AgentAdapterRegistry>>,
    /// Badgey wrapper state. W1 keeps the live badgey_id mapping in
    /// memory; proposals and action journal are durable in the state dir.
    pub(crate) consultant_registry: Arc<orchestration::consultant::ConsultantRegistry>,
    pub(crate) consultant_proposals: Arc<orchestration::consultant::ProposalStore>,
    pub(crate) consultant_journal: Arc<orchestration::consultant::ActionJournal>,
    /// Slack thread → claude session_id continuity map. Webhook
    /// `start_arc` looks up the prior session before starting an arc
    /// and seeds it into actor_sessions; the arc writes back when
    /// the executor turn completes. Lets follow-up @mentions in the
    /// same Slack thread continue the same Badgey conversation.
    pub(crate) slack_thread_store: Arc<slack_thread_store::SlackThreadStore>,
    /// Slack channel → project bindings. Resolves which bbox project
    /// a Slack channel maps to so inbound badgey activity is auto-scoped
    /// and the daily-triage cron knows where to fan out per-channel
    /// briefs. Channel (id, team) is the lookup key; renames are
    /// id-stable.
    pub(crate) slack_channel_bindings: Arc<slack_channel_bindings::SlackChannelBindings>,
    /// Slack message → proposal/authoring-session link records. One
    /// entry per proposal posted into Slack by the daily-triage tool.
    /// Reaction handlers resolve item_ts → proposal_id; thread-reply
    /// handlers resolve thread_ts → authoring_session_id.
    pub(crate) slack_proposal_links: Arc<slack_proposal_links::SlackProposalLinks>,
    pub(crate) config: std::sync::Arc<parking_lot::RwLock<crate::config::Config>>,
    pub(crate) atom_invocation_store: orchestration::atoms::invocation::SharedInvocationStore,
    // kept: SharedState vector store handle; consumed by embed/queue path through alternate state plumbing, retained here for direct access
    #[allow(dead_code)]
    pub(crate) vector_store: std::sync::Arc<crate::vectors::VectorStore>,
    pub(crate) system_events: system_events::SharedEventHub,
}

pub(crate) struct CodeReadView {
    pub(crate) active_selectors: BTreeMap<String, String>,
    pub(crate) searcher: tantivy::Searcher,
    pub(crate) edge_index: Arc<edge_index::EdgeIndex>,
    /// Catalog authority epoch this view was derived from
    /// (`ProjectRecordsSnapshot.authority_epoch`; Phase 3 plan section 4.5).
    /// Pinning it is what makes the view a coherent read: without it two
    /// `records_snapshot()` calls inside one request can observe different
    /// catalog epochs while the selector map says otherwise. Every writer
    /// that replaces a field it does not own must carry this through, or the
    /// view silently reports an epoch its selectors no longer match.
    pub(crate) catalog_epoch: u64,
    /// The Git current-file overlay selected per project at the moment this
    /// view was pinned (Phase 3 plan sections 4.5 and 10 item 1), keyed by
    /// project id.
    ///
    /// Catalog mode only; the bridge map stays empty because bridge local
    /// staging still writes its Git member inside its own transaction and
    /// has no overlay identity to pin. An empty map therefore means "this
    /// deployment has no overlay lane", never "the overlays were dropped".
    ///
    /// Pinned for the same reason `catalog_epoch` is: a request that reads
    /// commit-file edges through one overlay and commit documents through a
    /// generation the overlay no longer names is incoherent, and the only
    /// way to rule that out is to freeze the overlay map beside the searcher
    /// and selector map rather than re-reading the manifest mid-request.
    pub(crate) git_overlays: BTreeMap<String, bbox_corpus_core::git_overlay::GitOverlaySelector>,
}

/// Read the durable Git-overlay map that a fresh [`CodeReadView`] must pin
/// (Phase 3 plan sections 4.5 and 10 item 1).
///
/// Bridge mode returns an EMPTY map by contract, not by accident: the bridge
/// local lane stages its Git current-file member inside its own transaction
/// and never writes an overlay selector, so any non-empty map there would be
/// a claim the manifest cannot back.
///
/// A read failure degrades to the empty map with a warning rather than
/// failing view construction. The overlay is an enrichment on an already
/// valid code generation; refusing to publish a read view because the
/// manifest is momentarily unreadable would take code search down for a
/// commit-edge problem, which is the exact inversion P3-B's F5 fix removed.
pub(crate) fn read_git_overlays_for_view(
    authority: &ProjectAuthority,
    edges_dir: &std::path::Path,
    cutover: &bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1,
    code_sources: &super::code_source::CodeSourceRuntime,
) -> BTreeMap<String, bbox_corpus_core::git_overlay::GitOverlaySelector> {
    if !matches!(authority, ProjectAuthority::Catalog { .. }) {
        return BTreeMap::new();
    }
    match bbox_edge_sidecar::snapshot::selected_git_overlays(edges_dir) {
        Ok(mut overlays) => {
            let Some(store) = authority.catalog_store() else {
                return BTreeMap::new();
            };
            let catalog = match store.snapshot() {
                Ok(catalog) => catalog,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "catalog unavailable while filtering Git overlays; refusing every producer arm"
                    );
                    overlays.retain(|_, overlay| overlay.source.producer_transport().is_none());
                    return overlays;
                }
            };
            let assignments = code_sources.producer_auth().repo_assignment_producers();
            overlays.retain(|project_id, overlay| {
                git_overlay_visible_under_cutover(
                    catalog.catalog(),
                    &assignments,
                    cutover,
                    project_id,
                    overlay,
                )
            });
            overlays
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "reading the Git overlay map for the code read view failed; \
                 publishing without overlays"
            );
            BTreeMap::new()
        }
    }
}

fn git_overlay_visible_under_cutover(
    catalog: &bbox_corpus_core::project_catalog::CatalogSnapshotV2,
    assignments: &BTreeMap<bbox_corpus_core::identity::PublishedScope, String>,
    cutover: &bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1,
    project_id: &str,
    overlay: &bbox_corpus_core::git_overlay::GitOverlaySelector,
) -> bool {
    if overlay.source.producer_transport().is_none() {
        return true;
    }
    let repo_history_id =
        bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_string())
            .ok()
            .and_then(|project_id| catalog.projects.get(&project_id))
            .and_then(|project| project.repo_history.as_ref());
    let Some(repo_history_id) = repo_history_id else {
        return false;
    };
    let coverage = cutover.classify_repo(catalog, assignments, repo_history_id);
    !coverage.transport_governed() || coverage.current()
}

pub(crate) const SIGNAL_LOG_CAP: usize = 200;

pub(crate) const WEBHOOK_LOG_CAP: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SignalEvent {
    pub(crate) timestamp: String,
    pub(crate) signal: String,
    pub(crate) correlation: serde_json::Map<String, Value>,
    /// `"matched"` when a pending wait resolved, `"no_matching_wait"`
    /// otherwise.
    pub(crate) outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) matched_arc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) matched_wait_id: Option<String>,
    /// Snapshot of pending waits with the same signal name at
    /// dispatch time. Empty when the signal matched. When the signal
    /// went idle this is the diff a debugger needs: which arcs were
    /// waiting on this signal name, with what correlation, that
    /// failed to match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) idle_pending: Vec<crate::workflow::wait::WaitSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WebhookDelivery {
    pub(crate) received_at: String,
    pub(crate) webhook_name: String,
    /// `"webhook"` for live deliveries via `/webhook/:name`,
    /// `"replay"` for the no-signature replay endpoint.
    pub(crate) source: String,
    /// Subset of inbound headers that drove routing (lowercased
    /// `x-*` keys). Full header capture would balloon the buffer and
    /// most non-`x-*` headers carry no routing signal.
    pub(crate) headers: serde_json::Map<String, Value>,
    pub(crate) extracted_entity: Value,
    /// `"start_arc"` / `"signal_arc"` / `"cancel_arc"` / `"ignore"` /
    /// `"dead_letter"` / `"no_match"` (when no rule fired) /
    /// `"extractor_failed"` / `"signature_invalid"` /
    /// `"idempotency_dropped"`. Single string keeps the schema
    /// flat for filter queries.
    pub(crate) verdict_classification: String,
    pub(crate) response_status: u16,
    pub(crate) response_body: Value,
}

impl SharedState {
    /// The COMPLETE catalog project-id set, as a `HashSet` for the sidecar
    /// registered-project gate and the storage-GC liveness seed.
    ///
    /// Phase 3 plan section 7 item 3 (F3/F4): every daemon surface that
    /// answers "is this project registered?" MUST route through this one
    /// accessor. Two of them previously seeded from the attached-only
    /// compatibility rows, which silently dropped a remote-only project's
    /// edges on the first runtime rebuild and deleted its sidecars after the
    /// background GC's 30-day orphan fuse, while the equivalent MCP tools
    /// classified the same project as live. The divergence was a one-line
    /// difference in two constructors; concentrating it here is what makes
    /// re-introducing it a deliberate act.
    pub(crate) fn corpus_registered_project_ids(&self) -> std::collections::HashSet<String> {
        self.records_provider
            .records_snapshot()
            .registered_project_ids()
    }

    /// Classify one catalog Published project against the current cutover row
    /// and live assignment/membership projection. `None` means bridge mode,
    /// LegacyLocal authority, an unknown project, or no repo-history binding.
    pub(crate) fn git_transport_coverage_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<bbox_indexing::git_transport_cutover::GitTransportRuntimeCoverageV1>>
    {
        let Some(store) = self.project_authority.catalog_store() else {
            return Ok(None);
        };
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_string())?;
        let snapshot = store.snapshot()?;
        let Some(project) = snapshot.catalog().projects.get(&project_id) else {
            return Ok(None);
        };
        if !matches!(
            project.scope,
            bbox_corpus_core::project_catalog::ProjectScope::Published(_)
        ) {
            return Ok(None);
        }
        let Some(repo_history_id) = &project.repo_history else {
            return Ok(None);
        };
        let assignments = self
            .code_sources
            .producer_auth()
            .repo_assignment_producers();
        Ok(Some(self.git_transport_cutover.classify_repo(
            snapshot.catalog(),
            &assignments,
            repo_history_id,
        )))
    }

    pub(crate) fn git_transport_governs_project(&self, project_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .git_transport_coverage_for_project(project_id)?
            .is_some_and(|coverage| coverage.transport_governed()))
    }

    /// Classify one catalog Published project against its strict knowledge
    /// transport row. Any non-`Uncovered` state remains a no-fallback
    /// boundary; `Current` alone may serve newly selected remote state.
    pub(crate) fn knowledge_transport_coverage_for_project(
        &self,
        project_id: &str,
    ) -> anyhow::Result<
        Option<bbox_indexing::knowledge_transport_cutover::KnowledgeTransportRuntimeCoverageV1>,
    > {
        let Some(store) = self.project_authority.catalog_store() else {
            return Ok(None);
        };
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project_id.to_string())?;
        let snapshot = store.snapshot()?;
        let Some(project) = snapshot.catalog().projects.get(&project_id) else {
            return Ok(None);
        };
        if !matches!(
            project.scope,
            bbox_corpus_core::project_catalog::ProjectScope::Published(_)
        ) {
            return Ok(None);
        }
        let assignments = self
            .code_sources
            .producer_auth()
            .repo_assignment_producers();
        let accepted = self
            .accepted_publications
            .as_ref()
            .and_then(|runtime| runtime.load_verified(&project_id).ok());
        Ok(Some(self.knowledge_transport_cutover.classify_project(
            snapshot.catalog(),
            &assignments,
            &project_id,
            accepted.as_ref(),
        )))
    }

    /// Attach the daemon read-view publisher to the index writer's commit
    /// boundary. The actor invokes the hook once immediately, then after each
    /// successful commit, so no startup or small-op batch can leave the pinned
    /// searcher behind the shared Tantivy reader.
    pub(crate) fn install_code_read_view_commit_hook(self: &Arc<Self>) {
        let state = Arc::downgrade(self);
        self.index_writer
            .set_post_commit_searcher_hook(move |searcher| {
                if let Some(state) = state.upgrade() {
                    state.publish_code_read_searcher(searcher);
                }
            });
    }

    /// Replace only the searcher component of the immutable read view. Holding
    /// the view write lock while cloning selectors and edges prevents a commit
    /// refresh from reverting a concurrent code-source activation swap.
    pub(crate) fn publish_code_read_searcher(&self, searcher: tantivy::Searcher) {
        let mut published = self.code_read_view.write();
        let current = published.as_ref();
        *published = Arc::new(CodeReadView {
            active_selectors: current.active_selectors.clone(),
            searcher,
            edge_index: current.edge_index.clone(),
            catalog_epoch: current.catalog_epoch,
            // Cloned through for exactly the reason `edge_index` and
            // `catalog_epoch` are: this writer owns the searcher and nothing
            // else, so a field it drops is silently reset to "no overlay" on
            // the next commit. That is the drop-on-commit bug class the
            // preservation regression test pins.
            git_overlays: current.git_overlays.clone(),
        });
    }

    pub(crate) async fn persist_notes_durable(&self) -> anyhow::Result<()> {
        self.notes_persister.request_durable().await
    }

    pub(crate) async fn persist_roadmap_durable(&self) -> anyhow::Result<()> {
        self.roadmap_persister.request_durable().await
    }

    pub(crate) async fn persist_threads_durable(&self) -> anyhow::Result<()> {
        self.threads_persister.request_durable().await
    }

    pub(crate) async fn persist_pins_durable(&self) -> anyhow::Result<()> {
        self.pins_persister.request_durable().await
    }

    pub(crate) async fn persist_checkout_mutations_durable(&self) -> anyhow::Result<()> {
        self.checkout_mutations_persister.request_durable().await
    }

    pub(crate) async fn persist_projects_durable(&self) -> anyhow::Result<()> {
        match &self.project_authority {
            ProjectAuthority::Bridge { persister, .. } => persister.request_durable().await,
            ProjectAuthority::Catalog { .. } => anyhow::bail!(
                "no version-1 project persister exists in catalog mode; \
                 catalog mutations commit through the pair transaction"
            ),
        }
    }

    pub(crate) fn record_signal(&self, ev: SignalEvent) {
        let mut log = self.signal_log.write();
        if log.len() >= SIGNAL_LOG_CAP {
            log.pop_front();
        }
        log.push_back(ev);
    }

    pub(crate) fn record_webhook(&self, d: WebhookDelivery) {
        let mut log = self.webhook_delivery_log.write();
        if log.len() >= WEBHOOK_LOG_CAP {
            log.pop_front();
        }
        log.push_back(d);
    }

    /// Ask the edge-index rebuild watcher to run a rebuild soon (it wakes
    /// immediately when parked on its interval). `try_send` failure means a
    /// nudge is already pending — the queued rebuild will see this caller's
    /// store mutation too, so dropping the second nudge is correct.
    pub(crate) fn nudge_edge_index_rebuild(&self) {
        let _ = self.edge_rebuild_nudge_tx.try_send(());
    }

    /// Clone one internally coherent code read view and reject the deferred
    /// placeholder. Selector-changing publishers lower the readiness fence
    /// before swapping the view, so checking after the clone can return only
    /// a complete old view or a complete new view, never the placeholder.
    pub(crate) fn complete_code_read_view(&self) -> anyhow::Result<Arc<CodeReadView>> {
        let view = self.code_read_view.read().clone();
        if !self
            .edge_index_ready
            .load(std::sync::atomic::Ordering::Acquire)
        {
            anyhow::bail!(
                "error.edge_index_warming: the complete graph view is still rebuilding; retry this request"
            );
        }
        Ok(view)
    }

    pub(crate) fn roster_events(&self) -> orchestration::RosterEventSink {
        // Wire the view into the sink so every emit_* call also
        // updates the daemon-side cache. Sinks created before the
        // view is attached (none in production — for_test is the
        // only path) keep working via the no-view constructor.
        orchestration::RosterEventSink::with_view(
            self.roster_version.clone(),
            self.roster_tx.clone(),
            self.roster_view.clone(),
        )
    }

    /// Register a cancel token for a freshly-spawned arc. Returns the
    /// token so the runner can hold a clone for `is_cancelled()`
    /// checks. Replaces any prior token for the same arc_id (e.g.
    /// recycled arc_id under unusual restart races) — last writer
    /// wins.
    pub(crate) fn register_arc_cancel_token(&self, arc_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        self.arc_cancel_tokens
            .write()
            .insert(arc_id.to_string(), token.clone());
        token
    }

    /// Register a cancel token that is chained to a parent token.
    /// Cancelling the parent trips the child, while the child still
    /// remains addressable directly through `cancel_arc`.
    pub(crate) fn register_arc_cancel_token_child(
        &self,
        arc_id: &str,
        parent: &CancellationToken,
    ) -> CancellationToken {
        let token = parent.child_token();
        self.arc_cancel_tokens
            .write()
            .insert(arc_id.to_string(), token.clone());
        token
    }

    /// Drop the cancel token for an arc that's reached terminal
    /// state. Called from the runner's exit path so the map doesn't
    /// grow unbounded across daemon uptime.
    pub(crate) fn unregister_arc_cancel_token(&self, arc_id: &str) {
        self.arc_cancel_tokens.write().remove(arc_id);
    }

    /// Trigger cancellation for a running arc. Returns whether a
    /// matching token existed (and was triggered). The runner notices
    /// at the next node boundary — or immediately if it's parked on
    /// a Wait, since the wait's `tokio::select!` includes the token's
    /// `cancelled()` arm.
    pub(crate) fn cancel_arc(&self, arc_id: &str) -> bool {
        match self.arc_cancel_tokens.read().get(arc_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Borrowed store view for the entity-provider layer
    /// (`providers::CorpusStores`) — the daemon side of the provider
    /// crate's dependency inversion.
    pub(crate) fn corpus_stores(&self) -> crate::providers::CorpusStores<'_> {
        crate::providers::CorpusStores {
            idx: &self.idx,
            kb: self.kb.as_ref(),
            roadmap: self.roadmap.as_ref(),
            threads: self.threads.as_ref(),
            notes: self.notes.as_ref(),
            projects: self.records_provider.as_ref(),
            checkout_registry: self.checkout_registry.as_ref(),
            checkout_access: self.checkout_access.as_ref(),
            project_authority: match &self.project_authority {
                ProjectAuthority::Bridge { .. } => {
                    crate::providers::ProviderProjectAuthority::Bridge
                }
                ProjectAuthority::Catalog { store } => {
                    crate::providers::ProviderProjectAuthority::Catalog {
                        catalog: store.as_ref(),
                    }
                }
            },
            packets: &self.packets,
            artifacts: &self.artifacts,
            whiteboards: self.whiteboards.as_ref(),
            store_dir: &self.store_dir,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(store_dir: &std::path::Path) -> SharedState {
        use std::collections::VecDeque;
        let (tail_tx, _) = broadcast::channel(128);
        let (roster_tx, _) = broadcast::channel(ROSTER_BROADCAST_BUFFER);
        let projects_path = store_dir.join("projects.json");
        let (projects, projects_needs_persist) =
            ProjectRegistry::open_with_backfill_status(&projects_path).unwrap();
        let projects_store = Arc::new(RwLock::new(projects));
        let checkout_registry = Arc::new(RwLock::new(
            bbox_indexing::checkout_registry::CheckoutRegistry::open(
                &store_dir.join("checkout-registry.json"),
            )
            .unwrap(),
        ));
        let checkout_access_observations =
            bbox_indexing::checkout_access::CheckoutAccessObservations::open(
                store_dir.join("checkout-access-observations.json"),
            )
            .unwrap();
        let checkout_access = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
            Arc::new(
                bbox_indexing::checkout_access_v1::V1CheckoutAccessAuthority::new(
                    projects_store.clone(),
                    checkout_registry.clone(),
                ),
            ),
            checkout_access_observations.clone(),
        ));
        let records_provider: Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider> =
            Arc::new(bbox_indexing::projects::BridgeProjectRecordsProvider::new(
                projects_store.clone(),
            ));
        let idx = TranscriptIndex::open_or_create_with_code_source_store_path(
            &store_dir.join("idx"),
            Vec::new(),
            None,
            projects_path.clone(),
            store_dir.join("code-sources"),
            store_dir.join("kb.json"),
            store_dir.join("threads.json"),
            store_dir.join("roadmap.json"),
            records_provider.clone(),
            // Bridge authority here, so the bridge spill guard: this path opens
            // real state directories in tests, and an absent guard would refuse
            // any replacement rather than carrying commit documents across it.
            Some(
                bbox_indexing::index::schema_rebuild::bridge_schema_replacement_guard(
                    records_provider.clone(),
                ),
            ),
        )
        .unwrap();
        let index_writer = crate::index::IndexWriterActor::spawn_for_with_checkout_access(
            &idx,
            records_provider.clone(),
            checkout_access.clone(),
        );
        // Load committed `.bbox/knowledge/` for every registered project into
        // the knowledge query surface at startup (project durable knowledge is
        // repo-owned; the central store holds only global entries).
        let kb_path = store_dir.join("kb.json");
        let mut kb = Knowledge::open(&kb_path).unwrap();
        let path_fallback_cut =
            bbox_knowledge::inventory::path_fallback_was_cut(store_dir).unwrap();
        kb.set_path_fallback_cut(path_fallback_cut);
        let repo_projects =
            ProjectRegistry::load_records(store_dir.join("projects.json")).unwrap_or_default();
        let repo_io = Arc::new(super::repo_io::RepoIoAuthority::new(
            checkout_access.clone(),
        ));
        kb.configure_repo_io(
            repo_io.clone(),
            repo_io.clone(),
            super::repo_io::RepoIoAuthority::knowledge_base_carriers(&repo_projects, None).unwrap(),
        )
        .unwrap();
        let kb_store = Arc::new(RwLock::new(kb));
        let kb_persister = StorePersister::spawn("knowledge-test", kb_store.clone(), kb_path);
        // Gap store mirrors the kb repo-owned model: load every registered
        // project's committed `.bbox/gaps/` into the query surface at startup.
        let mut gaps = GapStore::open(&store_dir.join("blackbox-gaps.json")).unwrap();
        gaps.set_path_fallback_cut(path_fallback_cut);
        gaps.configure_repo_io(
            repo_io.clone(),
            repo_io,
            super::repo_io::RepoIoAuthority::gap_base_carriers(&repo_projects, None).unwrap(),
        )
        .unwrap();
        crate::threads::register_thread_embed_hook(crate::embed_queue::enqueue_thread_hook);
        crate::notes::register_note_embed_hook(crate::embed_queue::enqueue_note_hook);
        crate::index::writer_actor::register_embed_bootstrap(
            crate::embed_queue::register_index_embed_hooks,
        );
        crate::providers::register_extra_providers(crate::providers_ext::extra_providers());
        crate::embed::queue::register_contradiction_hook(crate::embed_runtime::contradiction_hook);
        let notes_path = store_dir.join("notes.json");
        let notes_store = Arc::new(RwLock::new(Notes::open(&notes_path).unwrap()));
        let notes_persister = StorePersister::spawn("notes-test", notes_store.clone(), notes_path);
        let roadmap_path = store_dir.join("roadmap.json");
        let roadmap_store = Arc::new(RwLock::new(Roadmap::open(&roadmap_path).unwrap()));
        let roadmap_persister =
            StorePersister::spawn("roadmap-test", roadmap_store.clone(), roadmap_path);
        let threads_path = store_dir.join("threads.json");
        let threads_store = Arc::new(RwLock::new(Threads::open(&threads_path).unwrap()));
        let threads_persister =
            StorePersister::spawn("threads-test", threads_store.clone(), threads_path);
        let pins_path = store_dir.join("pins.json");
        let pins_store = Arc::new(RwLock::new(Pins::open(&pins_path).unwrap()));
        let pins_persister = StorePersister::spawn("pins-test", pins_store.clone(), pins_path);
        let checkout_mutations_path = store_dir.join("checkout-mutations.json");
        let checkout_mutations_store = Arc::new(RwLock::new(
            crate::checkout_mutations::CheckoutMutations::open(&checkout_mutations_path).unwrap(),
        ));
        let checkout_mutations_persister = StorePersister::spawn(
            "checkout-mutations-test",
            checkout_mutations_store.clone(),
            checkout_mutations_path,
        );
        let projects_persister =
            StorePersister::spawn("projects-test", projects_store.clone(), projects_path);
        if projects_needs_persist {
            projects_persister.request();
        }
        let project_authority = ProjectAuthority::Bridge {
            registry: projects_store,
            persister: projects_persister,
        };

        let (edge_rebuild_nudge_tx, edge_rebuild_nudge_rx) = std::sync::mpsc::sync_channel(1);
        let active_code_selectors = idx.active_code_selectors();
        let code_searcher = idx.searcher();
        SharedState {
            idx: RwLock::new(idx),
            index_writer,
            kb: kb_store,
            kb_persister,
            gaps: RwLock::new(gaps),
            roadmap: roadmap_store,
            roadmap_persister,
            threads: threads_store,
            threads_persister,
            notes: notes_store,
            notes_persister,
            pins: pins_store,
            pins_persister,
            checkout_mutations: checkout_mutations_store,
            checkout_mutations_persister,
            project_authority,
            // `for_test` builds the bridge authority, which never has an
            // accepted-publication runtime.
            accepted_publications: None,
            records_provider,
            checkout_registry,
            checkout_access_observations,
            resolver_compat: crate::server::resolver_compat::ResolverCompatObservations::open(
                store_dir.join("resolver-compat-observations.json"),
            ),
            checkout_access,
            knowledge_transport_observations:
                bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationsV1::in_memory(),
            blame_locality_observations:
                bbox_indexing::blame_locality_observations::BlameLocalityObservationsV1::in_memory(),
            render_locality_observations:
                bbox_indexing::render_locality_observations::RenderLocalityObservationsV1::in_memory(),
            publisher_refs: RwLock::new(
                bbox_indexing::publisher::PublisherRefStore::open(
                    store_dir.join("publisher-refs.json"),
                )
                .unwrap(),
            ),
            knowledge_overlays: RwLock::new(
                bbox_knowledge::overlay::KnowledgeOverlayStore::default(),
            ),
            gap_overlays: RwLock::new(bbox_gaps::overlay::GapOverlayStore::default()),
            knowledge_overlay_refresh: Mutex::new(()),
            gap_overlay_refresh: Mutex::new(()),
            path_fallback_cut: AtomicBool::new(path_fallback_cut),
            knowledge_published_cache: RwLock::new(BTreeMap::new()),
            gap_published_cache: RwLock::new(BTreeMap::new()),
            catalog_knowledge_published_cache: RwLock::new(BTreeMap::new()),
            catalog_gap_published_cache: RwLock::new(BTreeMap::new()),
            project_graph_views: RwLock::new(Default::default()),
            publisher_authorization_cache: RwLock::new(Default::default()),
            packets: RwLock::new(Packets::open(store_dir).unwrap()),
            surface_decisions: crate::server::surface::SurfaceDecisionCache::default(),
            artifacts: RwLock::new(artifacts::ArtifactCatalog::open(store_dir).unwrap()),
            bbox_watcher: std::sync::Mutex::new(None),
            reindex_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            code_read_view: RwLock::new(Arc::new(CodeReadView {
                active_selectors: active_code_selectors,
                searcher: code_searcher,
                edge_index: Arc::new(edge_index::EdgeIndex::default()),
                catalog_epoch: 0,
                git_overlays: BTreeMap::new(),
            })),
            edge_index_ready: AtomicBool::new(true),
            code_sources: Arc::new(super::code_source::CodeSourceRuntime::for_test(store_dir)),
            file_sources: Arc::new(super::file_source::FileSourceRuntime::for_test(store_dir)),
            conversation_sources: Arc::new(
                super::conversation_source::ConversationSourceRuntime::for_test(store_dir),
            ),
            git_sources: Arc::new(super::git_source::GitSourceRuntime::for_test(store_dir)),
            knowledge_sources: Arc::new(super::knowledge_source::KnowledgeSourceRuntime::for_test(
                store_dir,
            )),
            git_transport_cutover: Arc::new(
                bbox_indexing::git_transport_cutover::GitTransportCutoverRuntimeV1::default(),
            ),
            knowledge_transport_cutover: Arc::new(
                bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1::default(),
            ),
            blame_locality_cutover: Arc::new(
                bbox_indexing::blame_locality_cutover::BlameLocalityCutoverRuntimeV1::default(),
            ),
            render_locality_cutover: Arc::new(
                bbox_indexing::render_locality_cutover::RenderLocalityCutoverRuntimeV1::default(),
            ),
            code_source_locality_cutover: Arc::new(
                bbox_indexing::code_source_locality_cutover::CodeSourceLocalityCutoverRuntimeV1::default(),
            ),
            reconciler_shutdown: parking_lot::RwLock::new(Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            )),
            edge_rebuild_nudge_tx,
            edge_rebuild_nudge_rx: std::sync::Mutex::new(Some(edge_rebuild_nudge_rx)),
            path_cache: RwLock::new(path_cache::PathCache::default()),
            task_store: Arc::new(RwLock::new(TaskStore::new())),
            tail_tx,
            roster_version: Arc::new(AtomicU64::new(0)),
            roster_tx,
            roster_view: Arc::new(orchestration::RosterView::new()),
            store_dir: store_dir.to_path_buf(),
            running_arcs: RwLock::new(HashMap::new()),
            wait_store: Arc::new(workflow::wait::WaitStore::new()),
            webhooks: Arc::new(webhooks::WebhookRegistry::new()),
            pollers: Arc::new(pollers::PollerRegistry::new()),
            crons: Arc::new(crons::CronRegistry::new()),
            whiteboards: Arc::new(whiteboards::WhiteboardRegistry::new()),
            workflow_registry: Arc::new(RwLock::new(HashMap::new())),
            bind_is_loopback: true,
            signal_log: RwLock::new(VecDeque::with_capacity(SIGNAL_LOG_CAP)),
            webhook_delivery_log: RwLock::new(VecDeque::with_capacity(WEBHOOK_LOG_CAP)),
            arc_cancel_tokens: RwLock::new(HashMap::new()),
            resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
            agent_adapter_registry: Arc::new(RwLock::new(
                orchestration::agents::adapter::AgentAdapterRegistry::new(),
            )),
            consultant_registry: Arc::new(orchestration::consultant::ConsultantRegistry::new()),
            consultant_proposals: Arc::new(
                orchestration::consultant::ProposalStore::new(
                    orchestration::badgey::descriptor().proposals_root(store_dir),
                )
                .unwrap(),
            ),
            consultant_journal: Arc::new(
                orchestration::consultant::ActionJournal::new(
                    orchestration::badgey::descriptor().action_journal_root(store_dir),
                )
                .unwrap(),
            ),
            slack_thread_store: Arc::new(
                slack_thread_store::SlackThreadStore::open(store_dir).unwrap(),
            ),
            slack_channel_bindings: Arc::new(
                slack_channel_bindings::SlackChannelBindings::open(store_dir).unwrap(),
            ),
            slack_proposal_links: Arc::new(
                slack_proposal_links::SlackProposalLinks::open(store_dir).unwrap(),
            ),
            config: Arc::new(RwLock::new(
                crate::config::load()
                    .unwrap_or_else(|e| panic!("loading config for test SharedState: {e}")),
            )),
            atom_invocation_store: Arc::new(RwLock::new(
                orchestration::atoms::invocation::InvocationStore::new(
                    store_dir.join("atom-invocations.json"),
                ),
            )),
            vector_store: Arc::new(
                crate::vectors::VectorStore::open(store_dir.join("vectors"))
                    .expect("test vector store should open"),
            ),
            system_events: Arc::new(system_events::EventHub::new(
                system_events::EventStore::new_at(store_dir.join("events").join("journal")),
                system_events::OutboxStore::new(store_dir.join("events").join("outbox")).unwrap(),
                store_dir.join("reactions"),
                store_dir.join("identities"),
            )),
        }
    }

    /// Catalog-authority variant of `for_test`.
    ///
    /// The catalog store lives beside the bridge fixture instead of
    /// replacing it, so the surrounding harness keeps working while the
    /// runtime authority, the record projection, and the accepted
    /// publication runtime all come from the catalog. The checkout
    /// authority is `DenyCheckoutAccess` on purpose: a catalog published
    /// read that reaches for a checkout must fail its test rather than
    /// quietly succeed on a developer machine that happens to have one.
    #[cfg(test)]
    pub(crate) fn for_test_catalog(
        store_dir: &std::path::Path,
        catalog_projects_path: &std::path::Path,
    ) -> SharedState {
        let mut state = SharedState::for_test(store_dir);
        let store = Arc::new(
            bbox_indexing::project_catalog_store::ProjectCatalogStore::open_existing(
                catalog_projects_path,
            )
            .unwrap(),
        );
        state.project_authority = ProjectAuthority::Catalog {
            store: store.clone(),
        };
        state.records_provider =
            Arc::new(bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(store));
        state.accepted_publications = Some(Arc::new(
            bbox_indexing::accepted_publication_runtime::AcceptedPublicationRuntime::open_global(
                catalog_projects_path,
            )
            .unwrap(),
        ));
        state.checkout_access =
            Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
                Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
                state.checkout_access_observations.clone(),
            ));
        state
    }
}

#[cfg(test)]
pub(crate) mod recordless_provider {
    //! Clause 1 of the exit proof (plan section 14.1): no corpus-only
    //! request requires `ProjectRecord`.
    //!
    //! The denial seam is FIELD-level, not method-level. `ProjectRecordsProvider`
    //! exposes one method and `ProjectRecordsSnapshot` carries two distinct
    //! views: `records` is the attached-only, path-bearing compatibility
    //! rows, and `corpus_project_ids` is the complete catalog id set that
    //! seeds corpus identity surfaces. Panicking on `records_snapshot`
    //! would deny both at once and kill the very paths this clause must
    //! prove.
    //!
    //! So `records` is EMPTY with `omitted_catalog_count` equal to the
    //! catalog count, while `corpus_project_ids`, `authority_epoch`, and
    //! `code_identities` stay live. Empty is the stronger proof: a panic
    //! shows only that the accessor went uncalled, while an empty
    //! attached-row view lets any path that still derives behavior from
    //! `ProjectRecord` content surface a typed refusal or an observably
    //! empty result instead of passing by luck.

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bbox_corpus_core::code_project_identity::CodeProjectIdentity;
    use bbox_corpus_core::project_record::{ProjectRecordsProvider, ProjectRecordsSnapshot};

    /// Wraps the real catalog provider and blanks exactly one field.
    pub(crate) struct RecordlessProjectRecordsProvider {
        inner: Arc<dyn ProjectRecordsProvider>,
    }

    impl RecordlessProjectRecordsProvider {
        pub(crate) fn new(inner: Arc<dyn ProjectRecordsProvider>) -> Self {
            Self { inner }
        }
    }

    impl ProjectRecordsProvider for RecordlessProjectRecordsProvider {
        fn records_snapshot(&self) -> ProjectRecordsSnapshot {
            let live = self.inner.records_snapshot();
            let omitted = live.corpus_project_ids.len() as u64;
            ProjectRecordsSnapshot {
                records: Arc::new(Vec::new()),
                corpus_project_ids: live.corpus_project_ids,
                omitted_catalog_count: omitted,
                authority_epoch: live.authority_epoch,
            }
        }

        fn code_identities(&self) -> BTreeMap<String, CodeProjectIdentity> {
            // Identity is corpus-side, not record-side. Blanking it would
            // deny a surface the clause is meant to prove still works.
            self.inner.code_identities()
        }

        fn last_degradation(&self) -> Option<String> {
            self.inner.last_degradation()
        }
    }
}

#[cfg(test)]
mod blocking_acceptance_proofs {
    //! The exit-gate proofs must be BLOCKING (plan section 14): their
    //! failure has to fail the suite, not sit in a CI log a reader may
    //! skim. Running the scripts from a test is what makes that true for
    //! `cargo nextest run` and therefore for every gate that wraps it.

    fn repo_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    fn run_acceptance(script: &str) -> (bool, String) {
        let output = std::process::Command::new("bash")
            .arg(repo_root().join("scripts").join(script))
            .current_dir(repo_root())
            .output()
            .unwrap_or_else(|error| panic!("running {script}: {error}"));
        let mut rendered = String::from_utf8_lossy(&output.stdout).into_owned();
        rendered.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), rendered)
    }

    /// Clause 2 Proof B. A NEW or grown site means a converted surface
    /// gained another unleased way to reach a checkout.
    ///
    /// The scan runs in-process rather than shelling to the script: the
    /// exclusion is a Rust parse now, and a nested cargo invocation from
    /// inside a test would contend for the build lock this test already
    /// holds. The script is the operator entry point and calls back here.
    #[test]
    fn catalog_ownership_ratchet_holds() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // The scanner's completeness claim is only as good as the syn
        // surface it was audited against, so the inventory check gates the
        // proof rather than sitting beside it.
        super::super::catalog_ownership_scan::assert_covered_node_inventory(root)
            .expect("catalog ownership node inventory");
        let report =
            super::super::catalog_ownership_scan::run(root, false).expect("catalog ownership scan");
        assert!(report.ok, "{}", report.rendered);
        println!("{}", report.rendered);
    }

    /// Operator entry point for refreshing the inventory after a legitimate
    /// removal. Ignored by default so a normal run never rewrites evidence.
    ///
    /// Selection is by test NAME rather than an environment variable: the
    /// build routes into a container on this estate, and `kubectl exec`
    /// does not forward arbitrary environment, so an env-driven switch is
    /// silently inert exactly where the suite actually runs. Argv survives.
    #[test]
    #[ignore = "rewrites the ownership baseline; run explicitly"]
    fn catalog_ownership_baseline_write() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let report = super::super::catalog_ownership_scan::run(root, true)
            .expect("catalog ownership baseline write");
        println!("{}", report.rendered);
    }

    /// Clause 2 Proof C: every checkout-open call site is classified.
    ///
    /// Blocking here rather than only in CI: a new acquisition must arrive
    /// with its section 14.2C attributes or the suite goes red.
    #[test]
    fn checkout_callsite_audit_is_complete() {
        let (ok, rendered) = run_acceptance("acceptance-checkout-callsites.sh");
        assert!(ok, "checkout call-site audit failed:\n{rendered}");
    }

    /// The lower corpus crate must never gain the upward dependency that
    /// would let it acquire leases for itself (plan 4.15, Risk 10).
    #[test]
    fn corpus_index_dependency_ceiling_holds() {
        let (ok, rendered) = run_acceptance("acceptance-corpus-index-deps.sh");
        assert!(ok, "dependency ceiling failed:\n{rendered}");
    }
}

#[cfg(test)]
mod clause_one_exit_proof {
    //! Plan section 14.1. Every corpus-only operation must behave
    //! IDENTICALLY against a provider whose attached-row view is empty.
    //! Equality is the proof: success alone would not rule out a path
    //! quietly deriving behavior from `ProjectRecord` content.

    use std::sync::Arc;

    use super::BlackboxServer;
    use super::catalog_fixture::{COMMIT_ONE, CatalogFixture, gap_note, knowledge_entry};
    use super::recordless_provider::RecordlessProjectRecordsProvider;

    const PROJECT: &str = "p_clause_one";
    const ATTACHED_PROJECT: &str = "p_clause_one_attached";

    /// A populated server and its recordless twin over the same durable
    /// bytes, so any difference is the blanked field and nothing else.
    fn twin(fixture: &CatalogFixture) -> (BlackboxServer, BlackboxServer) {
        let populated = fixture.server();
        let recordless = fixture.server_with_records_provider(|inner| {
            Arc::new(RecordlessProjectRecordsProvider::new(inner))
        });
        (populated, recordless)
    }

    /// A remote-only published project PLUS an attached one.
    ///
    /// The attached project is what gives this whole proof teeth. With only
    /// a remote-only project, `records` is empty on the POPULATED twin too,
    /// so every equality below compares two identical empty views and holds
    /// no matter what the code does with the attached-row view. Mutation
    /// testing caught exactly that: a corpus path made to bail when
    /// `records` was empty still passed. The populated twin must genuinely
    /// carry rows for the comparison to mean anything.
    fn fixture_with_content() -> CatalogFixture {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "published")],
            &[gap_note("gap-11111111", "published")],
        );

        let attached_scope = CatalogFixture::scope("attached");
        fixture.add_published_project(ATTACHED_PROJECT, &attached_scope);
        let checkout = fixture.root().join("clause-one-checkout");
        std::fs::create_dir_all(checkout.join("attached")).unwrap();
        fixture.attach_overlay_checkout_at(
            ATTACHED_PROJECT,
            &attached_scope,
            &checkout,
            &checkout.join("attached"),
            "att_00000000000000000000000000000d01",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaad01",
            true,
        );
        fixture
    }

    /// The seam itself: `records` is empty, the corpus id set is not, and
    /// the omitted count names exactly what was withheld.
    #[test]
    fn the_denial_seam_is_field_level_not_method_level() {
        let fixture = fixture_with_content();
        let (populated, recordless) = twin(&fixture);

        let full = populated.state.records_provider.records_snapshot();
        let blanked = recordless.state.records_provider.records_snapshot();

        assert!(blanked.records.is_empty(), "attached rows are withheld");
        assert_eq!(
            blanked.corpus_project_ids, full.corpus_project_ids,
            "the corpus id set stays live; denying it would kill the paths under proof"
        );
        assert_eq!(blanked.authority_epoch, full.authority_epoch);
        assert_eq!(
            blanked.omitted_catalog_count,
            full.corpus_project_ids.len() as u64
        );
        assert_eq!(
            recordless.state.records_provider.code_identities(),
            populated.state.records_provider.code_identities(),
            "code identity is corpus-side, not record-side"
        );
    }

    /// Published knowledge and gaps are corpus-only reads: identical bytes
    /// with and without the attached-row view.
    #[test]
    fn published_views_are_byte_identical_without_attached_rows() {
        let fixture = fixture_with_content();
        let (populated, recordless) = twin(&fixture);

        let expected = populated.session_knowledge_view(None, None).unwrap();
        let actual = recordless.session_knowledge_view(None, None).unwrap();
        let ids = expected
            .knowledge
            .all_entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert!(
            !ids.is_empty(),
            "the fixture publishes knowledge to compare"
        );
        assert_eq!(
            serde_json::to_string(&actual.structured_response(&ids)).unwrap(),
            serde_json::to_string(&expected.structured_response(&ids)).unwrap(),
            "published knowledge must not vary with the attached-row view"
        );

        let expected = populated.session_gap_view(None, None).unwrap();
        let actual = recordless.session_gap_view(None, None).unwrap();
        assert_eq!(
            serde_json::to_string(actual.gaps.all()).unwrap(),
            serde_json::to_string(expected.gaps.all()).unwrap(),
            "published gaps must not vary with the attached-row view"
        );
        assert_eq!(actual.diagnostics, expected.diagnostics);
    }

    /// The plan section 14.1 operation inventory, pinned.
    ///
    /// The walk below asserts it executed exactly this list in exactly this
    /// order. Deleting a row therefore fails rather than silently reducing
    /// coverage, which is how this proof came to cover two operations while
    /// claiming twelve.
    const REQUIRED_OPERATIONS: [&str; 12] = [
        "lexical search",
        "hybrid search",
        "graph inspect",
        "graph path traversal",
        "evidence bundle",
        "entity-ref resolution",
        "project-file provider",
        "storage GC",
        "collected activation and rebuild",
        "published knowledge",
        "published gaps",
        "provenance export plan",
    ];

    fn rendered(result: &rmcp::model::CallToolResult) -> String {
        format!("{:?}|{:?}", result.is_error, result.content)
    }

    /// Build tool params through their own deserializer. These structs
    /// carry serde defaults but not `Default`, and hand-building seven of
    /// them would bury the operation list this walk exists to make legible.
    fn params<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).expect("params deserialize")
    }

    /// Clause 1 in full: EVERY section 14.1 operation, run against the
    /// populated server and its recordless twin, compared byte for byte.
    ///
    /// Equality is the proof, not success. Several of these operations
    /// legitimately refuse under `DenyCheckoutAccess` (the file provider
    /// needs a checkout; the provenance plan needs an authoritative session
    /// checkout). An identical refusal is exactly as strong a statement as
    /// an identical success: neither varies with the attached-row view.
    #[tokio::test]
    async fn every_corpus_only_operation_is_identical_without_attached_rows() {
        use rmcp::handler::server::wrapper::Parameters;

        let fixture = fixture_with_content();
        let (populated, recordless) = twin(&fixture);
        // Without this the whole comparison is between two empty views.
        assert!(
            !populated
                .state
                .records_provider
                .records_snapshot()
                .records
                .is_empty(),
            "the populated twin must genuinely carry attached rows, or every \
             equality below holds vacuously"
        );
        assert!(
            recordless
                .state
                .records_provider
                .records_snapshot()
                .records
                .is_empty()
        );
        let mut executed: Vec<&str> = Vec::new();

        macro_rules! compare {
            ($name:expr, $server:ident => $call:expr) => {{
                let left = {
                    let $server = &populated;
                    $call
                };
                let right = {
                    let $server = &recordless;
                    $call
                };
                assert_eq!(
                    rendered(&right),
                    rendered(&left),
                    "{} varied with the attached-row view",
                    $name
                );
                executed.push($name);
            }};
        }

        compare!("lexical search", server => server
            .bbox_search(Parameters(params(serde_json::json!({"query": "published"}))))
            .await);
        compare!("hybrid search", server => server
            .bbox_hybrid_search(Parameters(params(serde_json::json!({"query": "published"}))))
            .await);
        compare!("graph inspect", server => server
            .bbox_inspect_entity(Parameters(params(
                serde_json::json!({"entity_ref": "knowledge:knowledge-a"})
            )))
            .await);
        compare!("graph path traversal", server => server
            .bbox_find_paths(Parameters(params(
                serde_json::json!({"from": "knowledge:knowledge-a"})
            )))
            .await);
        compare!("evidence bundle", server => server
            .bbox_bundle_evidence(Parameters(params(serde_json::json!({
                "question": "what is published?",
                "entity_refs": ["knowledge:knowledge-a"],
                "path_ids": [],
            }))))
            .await);
        compare!("entity-ref resolution", server => server
            .bbox_ref_size(Parameters(params(
                serde_json::json!({"refs": ["knowledge:knowledge-a"]})
            )))
            .await);
        compare!("project-file provider", server => server
            .bbox_ref_size(Parameters(params(
                serde_json::json!({"refs": ["file:src/lib.rs"]})
            )))
            .await);
        compare!("storage GC", server => server
            .bbox_storage_gc(Parameters(params(serde_json::json!({"dry_run": true}))))
            .await);
        compare!("provenance export plan", server => server
            .bbox_provenance_export_plan(Parameters(params(serde_json::json!({}))))
            .await);

        // Collected activation and rebuild is not a tool call: it is the
        // index-side pass that seeds corpus identity from
        // `corpus_project_ids`, which is the field clause 1 keeps live.
        //
        // Compared by rebuilt CONTENT, not by return status. A rebuild that
        // consults `records`, emits different edges, and returns Ok on both
        // twins is exactly the failure this row exists to catch, and
        // is_ok()-equality cannot see it.
        // ONE project-keyed sidecar file, named for the ATTACHED project so
        // the loader's file-stem check admits it.
        //
        // The attached project, not the remote-only one, is what makes the
        // mutation red on the COMPARISON rather than on the guard: it is
        // present in the populated twin's records and absent from the
        // recordless twin's, so a rebuild rewired to consult records makes
        // the two projections DIVERGE. Keyed to the remote-only project both
        // twins lose the edge together, which still reds but proves only
        // that the seam was touched, not that the twins differ. This is the only lane the
        // rebuild's registered-project set actually gates
        // (project_sidecar_edges_in_dir -> sidecar_project_is_registered),
        // and it is what makes this row seam-relative rather than merely
        // populated: store-projected knowledge edges never pass through
        // that filter, so comparing them proved nothing about records.
        //
        // The Edge is CONSTRUCTED and serialized rather than hand-written,
        // so the fixture cannot drift from the type. That is not
        // hypothetical: writing this by hand once already produced a file
        // that never loaded, and the row went green having compared
        // nothing. A constructor makes such a drift a compile error.
        let sidecar_edge = bbox_edge_index::edge_index::Edge {
            source: bbox_corpus_core::entity_ref::EntityRef::parse("knowledge:edge-seed-new")
                .expect("edge source ref"),
            kind: "DESCRIBES".to_string(),
            target: bbox_corpus_core::entity_ref::EntityRef::parse("knowledge:edge-seed-old")
                .expect("edge target ref"),
            provenance: bbox_chunker::EdgeProvenance::Explicit,
            confidence: bbox_chunker::EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        };
        let sidecar_key = format!(
            "{}|{}|{}",
            sidecar_edge.source, sidecar_edge.kind, sidecar_edge.target
        );
        let edges_dir = crate::server::edge_sidecar_dir(&populated.state);
        std::fs::create_dir_all(&edges_dir).unwrap();
        std::fs::write(
            edges_dir.join(format!("{ATTACHED_PROJECT}.jsonl")),
            format!(
                "{}\n",
                serde_json::to_string(&sidecar_edge).expect("edge serializes")
            ),
        )
        .unwrap();

        for server in [&populated, &recordless] {
            let mut base = knowledge_entry("edge-seed-old", "superseded seed");
            let mut newer = knowledge_entry("edge-seed-new", "superseding seed");
            // A SUPERSEDES link is what actually projects an edge; an
            // isolated entry projects none, which the non-triviality guard
            // below caught on the first attempt.
            newer.supersedes = Some(base.id.clone());
            base.status = bbox_knowledge::knowledge::Status::Superseded;
            let mut kb = server.state.kb.write();
            kb.upsert_generated(base).expect("seed entry");
            kb.upsert_generated(newer).expect("seed entry");
            drop(kb);
            server
                .rebuild_edge_index_from_stores()
                .expect("rebuild succeeds on both twins");
        }
        let edge_projection = |server: &BlackboxServer| {
            let view = server.state.code_read_view.read().clone();
            let mut edges = view
                .edge_index
                .all_edges()
                .map(|edge| format!("{}|{}|{}", edge.source, edge.kind, edge.target))
                .collect::<Vec<_>>();
            edges.sort();
            edges
        };
        let left = edge_projection(&populated);
        let right = edge_projection(&recordless);
        // Seam-relative non-triviality. "Populated" is not enough: the
        // compared projection must CONTAIN the edge that travels through
        // the filter under test, or a rebuild rewired to consult `records`
        // changes nothing this row can see.
        assert!(
            left.contains(&sidecar_key),
            "the compared projection must contain the project-keyed sidecar \
             edge, which is the one the registered-project filter gates: \
             {left:?}"
        );

        assert_eq!(
            right, left,
            "collected activation and rebuild produced different edges with \
             and without the attached-row view"
        );
        executed.push("collected activation and rebuild");

        // The two content-domain reads, compared as structured responses.
        let expected = populated.session_knowledge_view(None, None).unwrap();
        let actual = recordless.session_knowledge_view(None, None).unwrap();
        let ids = expected
            .knowledge
            .all_entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert!(
            !ids.is_empty(),
            "the fixture publishes knowledge to compare"
        );
        assert_eq!(
            serde_json::to_string(&actual.structured_response(&ids)).unwrap(),
            serde_json::to_string(&expected.structured_response(&ids)).unwrap(),
        );
        executed.push("published knowledge");

        let expected = populated.session_gap_view(None, None).unwrap();
        let actual = recordless.session_gap_view(None, None).unwrap();
        assert_eq!(
            serde_json::to_string(actual.gaps.all()).unwrap(),
            serde_json::to_string(expected.gaps.all()).unwrap(),
        );
        executed.push("published gaps");

        executed.sort();
        let mut required = REQUIRED_OPERATIONS.to_vec();
        required.sort();
        assert_eq!(
            executed, required,
            "the section 14.1 inventory and the walk have diverged; a deleted \
             row cannot silently reduce coverage"
        );

        // The checkout authority was never granted: clause 1 is a
        // corpus-only claim and must not have leaned on a lease.
        for server in [&populated, &recordless] {
            let granted: u64 = server
                .state
                .checkout_access
                .health()
                .operations
                .iter()
                .map(|operation| operation.granted)
                .sum();
            assert_eq!(granted, 0, "clause 1 opened a checkout");
        }
    }

    /// A remote-only project has no attached row in EITHER provider, so
    /// this row also proves the equality is not vacuous for the populated
    /// side: the populated provider genuinely carries rows for attached
    /// projects elsewhere in the fixture set.
    #[test]
    fn the_corpus_id_set_still_seeds_identity_for_a_remote_only_project() {
        let fixture = fixture_with_content();
        let (_, recordless) = twin(&fixture);
        let snapshot = recordless.state.records_provider.records_snapshot();
        assert!(snapshot.corpus_project_ids.contains(PROJECT));
        assert!(
            recordless
                .state
                .records_provider
                .code_identities()
                .contains_key(PROJECT),
            "identity for a project with no attached row still resolves"
        );
    }
}

#[cfg(test)]
mod committed_bytes_parity_tests {
    //! The fixture must hash exactly what a writer commits.
    //!
    //! This is the guard against the vacuously-green class: when the
    //! fixture and the test writers shared one PRIVATE encoding, they
    //! agreed with each other, every published digest matched, and the
    //! byte-equality suppression rule looked exercised while nothing
    //! production writes was ever compared. Binding both to the writer's
    //! own encoder is the fix; these tests keep it bound.

    use bbox_gaps::gaps::committed_gap_note_bytes;
    use bbox_knowledge::knowledge::committed_knowledge_entry_bytes;

    use super::catalog_fixture::{gap_note, knowledge_entry};

    /// The knowledge encoder drops host-local and telemetry fields. A
    /// fixture entry deliberately carries both, so an encoder that skipped
    /// normalization would show up here rather than as a silent digest
    /// miss three layers away.
    #[test]
    fn committed_knowledge_bytes_are_normalized_and_newline_terminated() {
        let mut entry = knowledge_entry("k1", "content");
        entry.project = Some("/host/local/path".into());
        entry.recall_count = 9;
        entry.last_recalled = Some("2026-01-03T00:00:00Z".into());

        let bytes = committed_knowledge_entry_bytes(&entry).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.ends_with('\n'), "committed JSON is newline-terminated");
        assert!(
            !text.contains("/host/local/path"),
            "a committed entry carries no host path: {text}"
        );

        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded["project"].is_null());
        assert_eq!(decoded["recall_count"], 0);
        assert!(decoded["last_recalled"].is_null());

        // Byte-stable across the normalized/unnormalized pair: two entries
        // differing only in dropped fields commit identically.
        let mut twin = knowledge_entry("k1", "content");
        twin.project = None;
        twin.recall_count = 0;
        twin.last_recalled = None;
        assert_eq!(committed_knowledge_entry_bytes(&twin).unwrap(), bytes);
    }

    /// Gap-side twin of the same contract.
    #[test]
    fn committed_gap_bytes_are_normalized_and_newline_terminated() {
        let mut gap = gap_note("gap-11111111", "title");
        gap.project = Some("/host/local/path".into());
        gap.write_dir = Some("/host/local/write".into());
        gap.provisional_checkout_id = Some("checkout-1".into());

        let bytes = committed_gap_note_bytes(&gap).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.ends_with('\n'));
        assert!(!text.contains("/host/local"), "{text}");

        let decoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded["project"].is_null());
        assert!(decoded["write_dir"].is_null());
        assert!(decoded["provisional_checkout_id"].is_null());
    }

    /// The asymmetry itself: the fixture's published source bytes must be
    /// the writer's bytes. A private fixture encoding is what this catches.
    #[test]
    fn fixture_publishes_the_bytes_a_writer_commits() {
        let entry = knowledge_entry("k1", "content");
        let gap = gap_note("gap-11111111", "title");

        // The old fixture encoding, kept here ONLY as the negative: if it
        // ever matches again, the encoders have converged by accident and
        // the parity above proves nothing.
        assert_ne!(
            serde_json::to_vec(&entry).unwrap(),
            committed_knowledge_entry_bytes(&entry).unwrap(),
            "the ad hoc encoding must remain visibly different from the committed one"
        );
        assert_ne!(
            serde_json::to_vec(&gap).unwrap(),
            committed_gap_note_bytes(&gap).unwrap()
        );
    }
}

#[cfg(test)]
mod code_read_view_tests {
    use super::*;

    #[test]
    fn selector_republish_lowers_edge_readiness_before_placeholder_is_readable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        assert!(state.complete_code_read_view().is_ok());

        super::super::code_source::republish_code_read_view(&state).unwrap();

        let Err(error) = state.complete_code_read_view() else {
            panic!("selector republish exposed its placeholder as complete");
        };
        assert!(error.to_string().contains("error.edge_index_warming"));
    }

    #[test]
    fn covered_noncurrent_repo_suppresses_only_the_producer_overlay() {
        use bbox_corpus_core::git_overlay::{GitOverlaySelector, GitOverlaySourceV1};
        use bbox_corpus_core::git_transport_cutover::{
            RepoTransportGrantState, derive_repo_transport_grants,
        };
        use bbox_corpus_core::identity::PublishedScope;
        use bbox_corpus_core::project_catalog::{
            CommitNamespace, CorpusProject, ProjectId, ProjectScope, RecordedRepoAuthority,
            RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization, RepoHistoryRecord,
        };
        use bbox_indexing::git_transport_cutover::{
            GitTransportCutoverMarkerV1, GitTransportCutoverRuntimeV1,
            PredictedGitTransportCutoverRowV1,
        };
        use bbox_indexing::project_catalog_inventory::Sha256ValueV1;

        let project_id = ProjectId::parse("p_0000000000000000000000000000cf01").unwrap();
        let repo_history_id = RepoHistoryId::parse("rh_0000000000000000000000000000cf01").unwrap();
        let scope = PublishedScope::try_new("neutral-cutover", ".").unwrap();
        let mut catalog = bbox_corpus_core::project_catalog::CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                membership_generation: 1,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("neutral-cutover").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("neutral-cutover").unwrap(),
                compatibility_namespaces: Default::default(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: Default::default(),
                nominated_aliases: Default::default(),
                display_name: "Neutral cutover fixture".to_string(),
                created_at: "unix:1".to_string(),
                registered_at_compat: None,
                repo_history: Some(repo_history_id.clone()),
                languages: Default::default(),
            },
        );
        catalog.validate().unwrap();
        let assignments = BTreeMap::from([(scope, "producer-a".to_string())]);
        let projection = derive_repo_transport_grants(&catalog, &assignments);
        let RepoTransportGrantState::Granted { grant } = &projection.grants[&repo_history_id]
        else {
            panic!("fixture grant must be complete")
        };
        let marker = GitTransportCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:2".to_string(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: 1,
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            aggregate_grant_hash: Sha256ValueV1::digest(b"grants"),
            zero_prepared_history_journals: true,
            zero_prepared_provenance_journals: true,
            rows: vec![PredictedGitTransportCutoverRowV1 {
                repo_history_id: repo_history_id.clone(),
                grant_commitment: grant.commitment.clone(),
                membership_generation: 1,
                source_generation_id: "source-one".to_string(),
                p3_generation_id: format!("rhg_{}", "a".repeat(64)),
                history_parity_commitment: Sha256ValueV1::digest(b"history"),
                provenance_import_generations: BTreeMap::from([(
                    project_id.clone(),
                    "import-one".to_string(),
                )]),
                provenance_export_generations: BTreeMap::from([(
                    project_id.clone(),
                    "export-one".to_string(),
                )]),
                provenance_parity_commitments: BTreeMap::from([(
                    project_id.clone(),
                    Sha256ValueV1::digest(b"provenance"),
                )]),
                capability_baselines: Vec::new(),
            }],
            checksum_sha256: Sha256ValueV1::digest(b"checksum"),
        };
        let cutover = GitTransportCutoverRuntimeV1::from_marker(Some(marker));
        let producer_overlay = GitOverlaySelector {
            project_id: project_id.as_str().to_string(),
            code_generation: "code-one".to_string(),
            repo_history_generation: format!("rhg_{}", "a".repeat(64)),
            source: GitOverlaySourceV1::ProducerTransport {
                producer_id: "producer-a".to_string(),
                source_generation_id: "source-one".to_string(),
            },
            repo_head: "b".repeat(40),
            commit_namespace: "neutral-cutover".to_string(),
            overlay_generation: 1,
        };
        let mut attachment_overlay = producer_overlay.clone();
        attachment_overlay.source = GitOverlaySourceV1::Attachment {
            attachment_id: "att_0000000000000000000000000000cf01".to_string(),
        };

        assert!(git_overlay_visible_under_cutover(
            &catalog,
            &assignments,
            &GitTransportCutoverRuntimeV1::default(),
            project_id.as_str(),
            &producer_overlay,
        ));
        assert!(git_overlay_visible_under_cutover(
            &catalog,
            &assignments,
            &cutover,
            project_id.as_str(),
            &producer_overlay,
        ));
        assert!(!git_overlay_visible_under_cutover(
            &catalog,
            &BTreeMap::new(),
            &cutover,
            project_id.as_str(),
            &producer_overlay,
        ));
        catalog
            .repo_histories
            .get_mut(&repo_history_id)
            .unwrap()
            .membership_generation = 2;
        assert!(!git_overlay_visible_under_cutover(
            &catalog,
            &assignments,
            &cutover,
            project_id.as_str(),
            &producer_overlay,
        ));
        assert!(git_overlay_visible_under_cutover(
            &catalog,
            &assignments,
            &cutover,
            project_id.as_str(),
            &attachment_overlay,
        ));
        assert!(!git_overlay_visible_under_cutover(
            &catalog,
            &assignments,
            &cutover,
            "p_0000000000000000000000000000cf02",
            &producer_overlay,
        ));
    }

    fn knowledge_entry(content: &str) -> bbox_knowledge::knowledge::KnowledgeEntry {
        bbox_knowledge::knowledge::KnowledgeEntry {
            id: "feed1234".into(),
            title: "pinned view commit test".into(),
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
            created_at: "2026-07-22T00:00:00Z".into(),
            updated_at: "2026-07-22T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    fn pinned_search(state: &SharedState, view: &CodeReadView, query: &str) -> String {
        state
            .idx
            .read()
            .search_with_active_selectors_and_searcher(
                &crate::index::SearchParams {
                    query: query.into(),
                    mode: None,
                    account: None,
                    project: None,
                    role: None,
                    include_subagents: None,
                    limit: Some(5),
                    exclude_self: None,
                },
                &view.active_selectors,
                &view.searcher,
            )
            .unwrap()
    }

    #[test]
    fn ordinary_commit_refreshes_pinned_searcher_without_sidecar_event() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        state.install_code_read_view_commit_hook();
        let before = state.code_read_view.read().clone();
        assert!(
            !pinned_search(&state, &before, "pinnedrefreshsentinel")
                .contains("pinnedrefreshsentinel")
        );

        state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::UpsertKnowledge(Box::new(
                knowledge_entry("pinnedrefreshsentinel"),
            )));
        state.index_writer.flush_blocking().unwrap();

        let after = state.code_read_view.read().clone();
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(before.active_selectors, after.active_selectors);
        assert!(Arc::ptr_eq(&before.edge_index, &after.edge_index));
        assert!(
            pinned_search(&state, &after, "pinnedrefreshsentinel")
                .contains("pinnedrefreshsentinel")
        );
        assert!(
            !pinned_search(&state, &before, "pinnedrefreshsentinel")
                .contains("pinnedrefreshsentinel")
        );
    }

    /// Phase 3 P3-C read-view pin regression (plan section 4.5).
    ///
    /// `publish_code_read_searcher` replaces ONLY the searcher and must
    /// carry every field it does not own through untouched. The
    /// drop-on-commit bug class is silent by construction: the view keeps
    /// serving, it just reports an epoch (or a selector map, or an edge
    /// index) that no longer matches what a concurrent activation
    /// published. Every field added to `CodeReadView` must be asserted here.
    #[test]
    fn searcher_only_republish_preserves_the_catalog_epoch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        {
            let current = state.code_read_view.read().clone();
            *state.code_read_view.write() = Arc::new(CodeReadView {
                active_selectors: BTreeMap::from([(
                    "p_0000000000000000000000000000ep01".to_string(),
                    "local:p_0000000000000000000000000000ep01".to_string(),
                )]),
                searcher: current.searcher.clone(),
                edge_index: current.edge_index.clone(),
                catalog_epoch: 42,
                git_overlays: BTreeMap::from([(
                    "p_0000000000000000000000000000ep01".to_string(),
                    bbox_corpus_core::git_overlay::GitOverlaySelector {
                        project_id: "p_0000000000000000000000000000ep01".to_string(),
                        code_generation: "gen-ep01".to_string(),
                        repo_history_generation: format!("rhg_{}", "a".repeat(64)),
                        source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
                            attachment_id: "att_0000000000000000000000000000ep01".to_string(),
                        },
                        repo_head: "b".repeat(40),
                        commit_namespace: "nsep01".to_string(),
                        overlay_generation: 7,
                    },
                )]),
            });
        }
        let before = state.code_read_view.read().clone();

        state.install_code_read_view_commit_hook();
        state
            .index_writer
            .enqueue(crate::index::IndexWriteOp::UpsertKnowledge(Box::new(
                knowledge_entry("epochpreservationsentinel"),
            )));
        state.index_writer.flush_blocking().unwrap();

        let after = state.code_read_view.read().clone();
        assert!(
            !Arc::ptr_eq(&before, &after),
            "the commit must have republished the view"
        );
        assert_eq!(
            after.catalog_epoch, 42,
            "the searcher-only writer must carry the pinned catalog epoch through"
        );
        assert_eq!(before.active_selectors, after.active_selectors);
        assert!(Arc::ptr_eq(&before.edge_index, &after.edge_index));
        // P3-F: the overlay map joins the same preservation contract. A
        // dropped overlay map is worse than a dropped epoch: the view keeps
        // serving code documents while silently reporting that no project
        // has commit-file edges.
        assert_eq!(
            before.git_overlays, after.git_overlays,
            "the searcher-only writer must carry the pinned Git overlay map through"
        );
        assert_eq!(after.git_overlays.len(), 1);
    }

    /// Phase 3 P3-C F3/F4 gate: every daemon "is this project registered?"
    /// surface derives from ONE accessor, so a remote-only project cannot be
    /// live for the MCP tools and an orphan for the background GC at the
    /// same time. The two divergent constructors this replaced differed by a
    /// single line each.
    #[test]
    fn registered_project_ids_include_projects_with_no_attachment() {
        use bbox_corpus_core::project_record::ProjectRecordsSnapshot;

        let snapshot = ProjectRecordsSnapshot {
            records: Arc::new(Vec::new()),
            corpus_project_ids: Arc::new(std::collections::BTreeSet::from([
                "p_0000000000000000000000000000rem1".to_string(),
            ])),
            omitted_catalog_count: 1,
            authority_epoch: 3,
        };
        let registered = snapshot.registered_project_ids();
        assert!(
            registered.contains("p_0000000000000000000000000000rem1"),
            "a project with zero attachments is registered, just not attached"
        );
        assert_eq!(registered.len(), 1);
        assert!(
            snapshot.records.is_empty(),
            "the attached-only projection is exactly what must NOT seed this set"
        );
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArcSnapshot {
    pub(crate) arc_id: String,
    pub(crate) arc_thread_id: String,
    pub(crate) workflow_name: String,
    pub(crate) workflow_version: u32,
    pub(crate) status: String,
    pub(crate) current_node: Option<String>,
    pub(crate) completed_nodes: Vec<String>,
    pub(crate) in_flight_nodes: Vec<String>,
    pub(crate) last_verdict: Option<String>,
    pub(crate) visit_counts: std::collections::HashMap<String, u32>,
    pub(crate) started_at: String,
    pub(crate) updated_at: String,
}

// ---------------------------------------------------------------------------
// Project runtime status (plan section 6.8)
// ---------------------------------------------------------------------------

/// Per-project runtime status: a bounded, OBSERVATIONAL projection.
///
/// It is never authority. The catalog, the attachment store, and the
/// accepted pointer remain authority; this is assembled on demand from
/// those sources plus the runtime's own published observations, and a
/// consumer that acts on it must still take the corresponding lease.
///
/// It is deliberately separate from `CheckoutAccessHealth`, whose durable
/// observation counters keep a closed, low-cardinality key space as Phase 6
/// cut evidence (plan 4.17). Nothing here is written back into those
/// counters, and no operation that was never attempted is reported as
/// denied.
///
/// Path-free by construction: every field is a logical id, a published
/// scope, a content stamp, or an enum. No checkout path appears, which
/// plan 13.6 pins.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ProjectRuntimeStatus {
    pub(crate) project_id: String,
    /// `available`, or `unavailable` when the catalog pair could not be
    /// read for this projection.
    ///
    /// A project whose catalog authority is unreadable must still be
    /// REPORTED. Returning `None` and letting the caller drop it made an
    /// unreadable catalog look like a healthy host with fewer projects,
    /// which is the failure mode most likely to be believed. Everything
    /// catalog-derived is empty in that state and none of it is a denial:
    /// nothing was attempted (plan 4.17).
    pub(crate) catalog_authority: &'static str,
    /// Absent for a `LegacyLocal` project, which publishes under no scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) catalog_scope: Option<PublishedScopeView>,
    pub(crate) accepted: AcceptedRuntimeView,
    pub(crate) binding: BindingRuntimeView,
    pub(crate) attachments: Vec<AttachmentCapabilityView>,
    pub(crate) overlays: Vec<CheckoutOverlayView>,
    pub(crate) watcher: WatcherRuntimeView,
}

/// A published scope rendered as its two logical components, never a path.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PublishedScopeView {
    pub(crate) repo_id: String,
    pub(crate) bbox_root_relpath: String,
}

impl PublishedScopeView {
    fn from_scope(scope: &bbox_corpus_core::identity::PublishedScope) -> Self {
        Self {
            repo_id: scope.repo_id().to_string(),
            bbox_root_relpath: scope.bbox_root_relpath().to_string(),
        }
    }
}

/// Accepted-publication state and content identity.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AcceptedRuntimeView {
    /// `current`, `prior`, `missing`, `corrupt`, or `unavailable` when the
    /// runtime itself could not be consulted.
    pub(crate) state: &'static str,
    pub(crate) serves_published_content: bool,
    pub(crate) advance_available: bool,
    /// `agreed`, `refresh_required`, or `unevaluated`. `refresh_required`
    /// is the scope-migration bridge of plan 4.9.
    pub(crate) scope_agreement: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accepted_scope: Option<PublishedScopeView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) full_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accepted_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_sha256: Option<String>,
    /// Seconds since the unix epoch at the last pointer verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_verified_unix_secs: Option<u64>,
    /// The stable code of whatever refused, when the state is not Current.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diagnostic: Option<String>,
}

/// Which attachment the pointer names, and whether it is still usable.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct BindingRuntimeView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) producer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_generation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_generation_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pointer_sha256: Option<String>,
    /// `attached`, `detached`, `unknown_attachment`, `producer`, or `unbound`.
    ///
    /// `detached` is the D-033 item 1 residual made observable: catalog
    /// detach does not take the publication lock, so a pointer can name a
    /// freshly detached attachment. That is a misleading binding, not
    /// corruption, and an explicit bind repairs it.
    pub(crate) status: &'static str,
}

/// One attachment's recorded capability bits.
///
/// Read straight from the catalog row. A capability that is not recorded
/// is reported as unavailable; it is NOT reported as a denial, because no
/// operation was attempted (plan 4.17).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct AttachmentCapabilityView {
    pub(crate) attachment_id: String,
    pub(crate) checkout_id: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    /// The capability bits this attachment records, by name, sorted.
    pub(crate) available: Vec<&'static str>,
}

/// The last published overlay outcome for one checkout.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct CheckoutOverlayView {
    pub(crate) checkout_id: String,
    pub(crate) lane: &'static str,
    pub(crate) published_scope: PublishedScopeView,
    /// `fresh` or `unavailable`.
    pub(crate) outcome: &'static str,
    /// The accepted generation this overlay was computed against, when the
    /// stamp carries one. A mismatch against the accepted content stamp is
    /// what makes staleness explicit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) accepted_generation: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) diagnostics: Vec<String>,
}

/// Whether this process runs a watcher for the project's attachments.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WatcherRuntimeView {
    /// False when this process runs no watcher at all, which is not a
    /// project-level fault.
    pub(crate) watcher_running: bool,
    /// Attachment ids with a live native registration.
    pub(crate) registered_attachments: Vec<String>,
    /// Attachments that record `artifact_watching` but carry no
    /// registration. Non-empty here is the actionable state.
    pub(crate) capable_but_unregistered: Vec<String>,
}

/// Capability bit names, in the order the section 9 adapter table lists
/// them. Kept as a function rather than a derive so the projection never
/// depends on field order in the durable struct.
fn recorded_capabilities(
    capabilities: &bbox_corpus_core::project_catalog::AttachmentCapabilities,
) -> Vec<&'static str> {
    let mut available = Vec::new();
    for (recorded, name) in [
        (capabilities.local_code_source, "local_code_source"),
        (capabilities.git_history, "git_history"),
        (capabilities.blame, "blame"),
        (capabilities.repo_knowledge, "repo_knowledge"),
        (capabilities.repo_mutation, "repo_mutation"),
        (capabilities.render_output, "render_output"),
        (capabilities.provenance_note_io, "provenance_note_io"),
        (capabilities.artifact_watching, "artifact_watching"),
    ] {
        if recorded {
            available.push(name);
        }
    }
    available
}

impl SharedState {
    /// The projection for a project whose catalog pair cannot be read.
    ///
    /// Accepted publication is consulted anyway: it is its own durable
    /// store, verified independently of the catalog, and losing its status
    /// because a different file was unreadable would understate what the
    /// host can still serve. Scope agreement cannot be evaluated without
    /// the catalog, so no scope is passed and the accepted view reports
    /// `unevaluated` rather than guessing.
    fn unreadable_catalog_status(
        &self,
        project_id: &bbox_corpus_core::project_catalog::ProjectId,
    ) -> ProjectRuntimeStatus {
        let accepted_status = self
            .accepted_publications
            .as_ref()
            .and_then(|runtime| runtime.status(project_id, None).ok());
        ProjectRuntimeStatus {
            project_id: project_id.as_str().to_string(),
            catalog_authority: "unavailable",
            catalog_scope: None,
            accepted: AcceptedRuntimeView::project(accepted_status.as_ref()),
            binding: BindingRuntimeView {
                // The pointer's own bytes are readable; whether the
                // attachment it names is still attached is a CATALOG
                // question, and the catalog is what could not be read.
                status: match accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .map(|stamp| stamp.source().kind())
                {
                    Some("producer") => "producer",
                    Some("attachment") => "unknown_attachment",
                    _ => "unbound",
                },
                source_kind: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .map(|stamp| stamp.source().kind()),
                attachment_id: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .and_then(|stamp| stamp.attachment_id())
                    .map(|id| id.as_str().to_string()),
                producer_id: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .and_then(|stamp| stamp.source().producer_id())
                    .map(str::to_string),
                source_generation_id: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .and_then(|stamp| stamp.source().source_generation_id())
                    .map(str::to_string),
                source_generation_sha256: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .and_then(|stamp| stamp.source().source_generation_sha256())
                    .map(str::to_string),
                pointer_sha256: accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .map(|stamp| stamp.pointer_sha256().to_string()),
            },
            // Empty because unknown, never because denied.
            attachments: Vec::new(),
            overlays: Vec::new(),
            watcher: WatcherRuntimeView {
                watcher_running: false,
                registered_attachments: Vec::new(),
                capable_but_unregistered: Vec::new(),
            },
        }
    }

    /// Project one catalog project's runtime status (plan 6.8).
    ///
    /// `None` in bridge mode: there is no catalog project to project, and
    /// the bridge's health story stays the existing sections unchanged.
    pub(crate) fn project_runtime_status(&self, project_id: &str) -> Option<ProjectRuntimeStatus> {
        use bbox_corpus_core::project_catalog::{AttachmentStatus, ProjectId, ProjectScope};

        let store = self.project_authority.catalog_store()?;
        let parsed = ProjectId::parse(project_id).ok()?;
        let snapshot = match store.snapshot() {
            Ok(snapshot) => snapshot,
            // The catalog pair is unreadable. Accepted publication is a
            // SEPARATE durable store and is still independently verifiable,
            // so the honest projection reports the authority as unavailable
            // and keeps whatever published status stands on its own.
            Err(_) => return Some(self.unreadable_catalog_status(&parsed)),
        };
        let project = snapshot.catalog().projects.get(&parsed)?;
        let catalog_scope = match &project.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal | ProjectScope::Connector(_) => None,
        };

        let accepted_status = self
            .accepted_publications
            .as_ref()
            .and_then(|runtime| runtime.status(&parsed, catalog_scope.as_ref()).ok());
        let accepted = AcceptedRuntimeView::project(accepted_status.as_ref());

        let rows = snapshot
            .attachments()
            .attachments
            .values()
            .filter(|attachment| attachment.project_id == parsed)
            .collect::<Vec<_>>();

        let bound_attachment = accepted_status
            .as_ref()
            .and_then(|status| status.binding_stamp())
            .and_then(|stamp| stamp.attachment_id())
            .map(|id| id.as_str().to_string());
        let binding = BindingRuntimeView {
            status: match (
                accepted_status
                    .as_ref()
                    .and_then(|status| status.binding_stamp())
                    .map(|stamp| stamp.source().kind()),
                bound_attachment.as_deref(),
            ) {
                (Some("producer"), _) => "producer",
                (None, _) => "unbound",
                (_, None) => "unbound",
                (_, Some(attachment_id)) => match rows
                    .iter()
                    .find(|row| row.attachment_id.as_str() == attachment_id)
                {
                    None => "unknown_attachment",
                    Some(row) if row.status == AttachmentStatus::Attached => "attached",
                    Some(_) => "detached",
                },
            },
            pointer_sha256: accepted_status
                .as_ref()
                .and_then(|status| status.binding_stamp())
                .map(|stamp| stamp.pointer_sha256().to_string()),
            source_kind: accepted_status
                .as_ref()
                .and_then(|status| status.binding_stamp())
                .map(|stamp| stamp.source().kind()),
            attachment_id: bound_attachment,
            producer_id: accepted_status
                .as_ref()
                .and_then(|status| status.binding_stamp())
                .and_then(|stamp| stamp.source().producer_id())
                .map(str::to_string),
            source_generation_id: accepted_status
                .as_ref()
                .and_then(|status| status.binding_stamp())
                .and_then(|stamp| stamp.source().source_generation_id())
                .map(str::to_string),
            source_generation_sha256: accepted_status
                .as_ref()
                .and_then(|status| status.binding_stamp())
                .and_then(|stamp| stamp.source().source_generation_sha256())
                .map(str::to_string),
        };

        let attachments = rows
            .iter()
            .map(|row| AttachmentCapabilityView {
                attachment_id: row.attachment_id.as_str().to_string(),
                checkout_id: row.checkout_id.clone(),
                kind: format!("{:?}", row.kind).to_lowercase(),
                status: format!("{:?}", row.status).to_lowercase(),
                available: recorded_capabilities(&row.capabilities),
            })
            .collect::<Vec<_>>();

        let checkout_ids = rows
            .iter()
            .map(|row| row.checkout_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut overlays = Vec::new();
        for snapshot in self.knowledge_overlays.read().snapshots() {
            if !checkout_ids.contains(snapshot.key.checkout_id.as_str()) {
                continue;
            }
            overlays.push(CheckoutOverlayView {
                checkout_id: snapshot.key.checkout_id.clone(),
                lane: "knowledge",
                published_scope: PublishedScopeView::from_scope(&snapshot.key.published_scope),
                outcome: match snapshot.status {
                    bbox_knowledge::overlay::OverlayStatus::Valid => "fresh",
                    bbox_knowledge::overlay::OverlayStatus::Invalid => "unavailable",
                },
                accepted_generation: snapshot
                    .stamp
                    .as_ref()
                    .and_then(|stamp| stamp.accepted_generation.clone()),
                diagnostics: snapshot.diagnostics.clone(),
            });
        }
        for snapshot in self.gap_overlays.read().snapshots() {
            if !checkout_ids.contains(snapshot.key.checkout_id.as_str()) {
                continue;
            }
            overlays.push(CheckoutOverlayView {
                checkout_id: snapshot.key.checkout_id.clone(),
                lane: "gaps",
                published_scope: PublishedScopeView::from_scope(&snapshot.key.published_scope),
                outcome: match snapshot.status {
                    bbox_gaps::overlay::GapOverlayStatus::Valid => "fresh",
                    bbox_gaps::overlay::GapOverlayStatus::Invalid => "unavailable",
                },
                accepted_generation: snapshot
                    .stamp
                    .as_ref()
                    .and_then(|stamp| stamp.accepted_generation.clone()),
                diagnostics: snapshot.diagnostics.clone(),
            });
        }
        overlays.sort_by(|left, right| {
            (left.checkout_id.as_str(), left.lane).cmp(&(right.checkout_id.as_str(), right.lane))
        });

        let watcher = self.watcher_runtime_view(&rows);

        Some(ProjectRuntimeStatus {
            catalog_authority: "available",
            project_id: project_id.to_string(),
            catalog_scope: catalog_scope.as_ref().map(PublishedScopeView::from_scope),
            accepted,
            binding,
            attachments,
            overlays,
            watcher,
        })
    }

    fn watcher_runtime_view(
        &self,
        rows: &[&bbox_corpus_core::project_catalog::CheckoutAttachment],
    ) -> WatcherRuntimeView {
        use bbox_artifacts::watcher::ArtifactWatchAttachment;
        use bbox_corpus_core::project_catalog::AttachmentStatus;

        let guard = self.bbox_watcher.lock().ok();
        let Some(registered) = guard
            .as_ref()
            .and_then(|guard| guard.as_ref())
            .map(|watcher| watcher.registered_carriers())
        else {
            // No watcher in this process: not a project fault, and not an
            // "unregistered" verdict about any attachment.
            return WatcherRuntimeView {
                watcher_running: false,
                registered_attachments: Vec::new(),
                capable_but_unregistered: Vec::new(),
            };
        };
        let registered_attachments = registered
            .iter()
            .filter_map(|carrier| match carrier.attachment() {
                ArtifactWatchAttachment::AttachmentId(attachment_id) => Some(attachment_id.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let capable_but_unregistered = rows
            .iter()
            .filter(|row| row.status == AttachmentStatus::Attached)
            .filter(|row| row.capabilities.artifact_watching)
            .map(|row| row.attachment_id.as_str().to_string())
            .filter(|attachment_id| !registered_attachments.contains(attachment_id))
            .collect::<Vec<_>>();
        WatcherRuntimeView {
            watcher_running: true,
            registered_attachments: registered_attachments
                .iter()
                .filter(|attachment_id| {
                    rows.iter()
                        .any(|row| row.attachment_id.as_str() == attachment_id.as_str())
                })
                .cloned()
                .collect(),
            capable_but_unregistered,
        }
    }
}

impl AcceptedRuntimeView {
    fn project(
        status: Option<&bbox_indexing::accepted_publication_runtime::AcceptedPublicationStatus>,
    ) -> Self {
        use bbox_indexing::accepted_publication_runtime::{
            AcceptedPublicationScopeAgreement as Agreement, AcceptedPublicationState as State,
        };

        let Some(status) = status else {
            // The runtime itself is absent or refused. Distinct from
            // Missing, which is a proved absence of a pointer.
            return Self {
                state: "unavailable",
                serves_published_content: false,
                advance_available: false,
                scope_agreement: "unevaluated",
                accepted_scope: None,
                full_ref: None,
                accepted_commit: None,
                generation_id: None,
                generation_sha256: None,
                last_verified_unix_secs: None,
                diagnostic: None,
            };
        };
        let stamp = status.content_stamp();
        Self {
            state: match status.state() {
                State::Current => "current",
                State::Prior => "prior",
                State::Missing => "missing",
                State::Corrupt => "corrupt",
            },
            serves_published_content: status.state().serves_published_content(),
            advance_available: status.advance_available(),
            scope_agreement: match status.scope_agreement() {
                Agreement::Agreed => "agreed",
                Agreement::RefreshRequired => "refresh_required",
                Agreement::Unevaluated => "unevaluated",
            },
            accepted_scope: stamp
                .map(|stamp| PublishedScopeView::from_scope(stamp.accepted_scope())),
            full_ref: stamp.map(|stamp| stamp.full_ref().to_string()),
            accepted_commit: stamp.map(|stamp| stamp.accepted_commit().to_string()),
            generation_id: stamp.map(|stamp| stamp.generation_id().to_string()),
            generation_sha256: stamp.map(|stamp| stamp.generation_hash().to_string()),
            last_verified_unix_secs: status
                .last_verified_at()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_secs()),
            diagnostic: status.failure().map(|failure| failure.code().to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP Server Handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct BlackboxServer {
    pub(crate) state: Arc<SharedState>,
    pub(crate) tool_router: ToolRouter<Self>,
    /// Session-scoped MCP tool surface selector. Set once during
    /// MCP session initialization from the `?surface` query parameter.
    pub(crate) surface: OnceLock<Arc<str>>,
    /// Session-scoped project context for surface evaluation. Set once
    /// during MCP session initialization from the `?project` query
    /// parameter — resolved through the shared project resolver (alias /
    /// id / path → base canonical path), falling back to the literal value
    /// for parity with the bbox_mcp_surface tool (gap-310c36b6).
    pub(crate) surface_project: OnceLock<Option<Arc<str>>>,
    /// Server-authoritative checkout identity derived from trusted MCP
    /// transport context at initialization. Tool arguments never replace it.
    pub(crate) session_checkout:
        OnceLock<Option<Arc<bbox_corpus_core::project_record::ResolvedCheckoutScope>>>,
    /// Path-free managed workspace authority authenticated from the private
    /// self-MCP header. A raw query parameter can never populate this slot.
    pub(crate) session_workspace_binding:
        OnceLock<Option<Arc<super::knowledge_source::WorkspaceBindingGrant>>>,
    /// Scope-bound attended blame authority authenticated from producer
    /// bearer plus path-free identity headers. Other harness-local tools do
    /// not consult this slot.
    pub(crate) session_operator_blame_binding:
        OnceLock<Option<Arc<super::blame_authority::OperatorBlameGrant>>>,
    /// Scope-bound attended provenance-export authority authenticated from a
    /// producer bearer plus path-free published-scope headers. This grant is
    /// read-only corpus planning authority and cannot mutate project state.
    pub(crate) session_operator_provenance_binding:
        OnceLock<Option<Arc<super::provenance_authority::OperatorProvenanceGrant>>>,
}

/// Catalog-mode view fixtures shared by the published knowledge and gap
/// view tests. Building catalog state means a catalog store, published
/// projects with no attachment (the remote-only case), and accepted bytes
/// installed through the owning crate's real preparation path.
#[cfg(test)]
pub(crate) mod catalog_fixture {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{AttachmentId, CorpusProject, ProjectId, ProjectScope};
    use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapNote, GapResolution};
    use bbox_indexing::accepted_publication_test_support::{
        AcceptedPublicationSourceFileForTest, InstalledAcceptedPublicationForTest,
        corrupt_accepted_generation_for_test, install_accepted_publication_for_test,
        rebind_accepted_pointer_for_test,
    };
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;
    use bbox_knowledge::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};

    use super::{BlackboxServer, SharedState};

    pub(crate) const COMMIT_ONE: &str = "1111111111111111111111111111111111111111";
    pub(crate) const COMMIT_TWO: &str = "2222222222222222222222222222222222222222";

    pub(crate) struct CatalogFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        catalog_projects_path: PathBuf,
        store: Arc<ProjectCatalogStore>,
    }

    impl CatalogFixture {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let catalog_root = root.join("catalog");
            std::fs::create_dir_all(&catalog_root).unwrap();
            let catalog_projects_path = catalog_root.join("projects.json");
            let store =
                Arc::new(ProjectCatalogStore::initialize_empty(&catalog_projects_path).unwrap());
            Self {
                _directory: directory,
                root,
                catalog_projects_path,
                store,
            }
        }

        /// The same fixture over a store stood up by the OPERATOR genesis
        /// path instead of the library initializer.
        ///
        /// It exists so the daemon-side admission surfaces are exercised
        /// against the exact bytes an operator's `project-catalog genesis`
        /// produces, rather than against a store only tests can create. The
        /// two pairs are proved byte-identical in the genesis facade suite;
        /// this fixture is what turns that identity into runtime coverage.
        pub(crate) fn new_over_genesis_store() -> Self {
            use bbox_indexing::project_catalog_genesis::{
                ProjectCatalogGenesisFacadeV1, ProjectCatalogGenesisRequestV1,
            };
            use bbox_indexing::project_catalog_migration::{
                ProjectCatalogMigrationLayoutOverridesV1, ProjectCatalogMigrationResolvedLayoutV1,
            };

            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let catalog_root = root.join("catalog");
            std::fs::create_dir_all(&catalog_root).unwrap();
            let config_path = root.join("config.toml");
            // `vectors_dir` is explicit so the genesis census reads this
            // fixture's vector root rather than the platform state directory.
            std::fs::write(
                &config_path,
                format!(
                    "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                    catalog_root,
                    catalog_root.join("vectors")
                ),
            )
            .unwrap();
            let config = {
                let _guard = bbox_util::util::test_env_lock();
                bbox_config::config::load_with(bbox_config::config::LoadOptions {
                    config_path: Some(config_path),
                    ..Default::default()
                })
                .unwrap()
            };
            let target_layout = ProjectCatalogMigrationResolvedLayoutV1::from_config(
                &config,
                ProjectCatalogMigrationLayoutOverridesV1 {
                    projects_path: None,
                    state_dir: Some(catalog_root.clone()),
                },
            )
            .unwrap();
            ProjectCatalogGenesisFacadeV1::initialize(ProjectCatalogGenesisRequestV1 {
                target_layout,
            })
            .unwrap();

            let catalog_projects_path = catalog_root.join("projects.json");
            // Opened through the same strict entry daemon startup uses, so a
            // genesis store that startup would refuse fails here too.
            let store =
                Arc::new(ProjectCatalogStore::open_existing(&catalog_projects_path).unwrap());
            Self {
                _directory: directory,
                root,
                catalog_projects_path,
                store,
            }
        }

        pub(crate) fn scope(relative: &str) -> PublishedScope {
            PublishedScope::try_new("repo_example", relative).unwrap()
        }

        pub(crate) fn attachment() -> AttachmentId {
            AttachmentId::parse("att_11111111111111111111111111111111").unwrap()
        }

        /// Insert one published catalog project with no attachment. A
        /// remote-only project is exactly the case catalog published reads
        /// must serve.
        pub(crate) fn add_published_project(&self, project_id: &str, scope: &PublishedScope) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let scope = scope.clone();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |catalog, _attachments| {
                    catalog.projects.insert(
                        project_id.clone(),
                        CorpusProject {
                            project_id: project_id.clone(),
                            scope: ProjectScope::Published(scope.clone()),
                            operator_aliases: Default::default(),
                            nominated_aliases: Default::default(),
                            display_name: project_id.as_str().to_string(),
                            created_at: "2026-07-25T00:00:00Z".into(),
                            registered_at_compat: None,
                            repo_history: None,
                            languages: Default::default(),
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        /// Attach one real checkout to a project, with the capability a
        /// publish requires. Cross-validation ties the row to its scope,
        /// so the caller supplies a directory whose committed identity
        /// matches.
        pub(crate) fn attach_checkout(
            &self,
            project_id: &str,
            scope: &PublishedScope,
            checkout_dir: &Path,
            attachment_id: &str,
        ) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let attachment_id = AttachmentId::parse(attachment_id).unwrap();
            let scope = scope.clone();
            let checkout_dir = checkout_dir.to_string_lossy().into_owned();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    attachments.attachments.insert(
                        attachment_id.clone(),
                        bbox_corpus_core::project_catalog::CheckoutAttachment {
                            attachment_id: attachment_id.clone(),
                            project_id: project_id.clone(),
                            checkout_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01".into(),
                            checkout_dir: checkout_dir.clone(),
                            checkout_project_dir: checkout_dir.clone(),
                            project_root_relpath: scope.bbox_root_relpath().to_string(),
                            kind: bbox_corpus_core::project_catalog::AttachmentKind::Base,
                            validated_scope: Some(scope.clone()),
                            computed_repo_hint: None,
                            branch_ref: Some("refs/heads/main".into()),
                            capabilities:
                                bbox_corpus_core::project_catalog::AttachmentCapabilities {
                                    repo_knowledge: true,
                                    ..Default::default()
                                },
                            status: bbox_corpus_core::project_catalog::AttachmentStatus::Attached,
                            attached_at: "2026-08-03T00:00:00Z".into(),
                            detached_at: None,
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        /// Attach one real checkout with an explicit durable identity and
        /// capability bit, and mint the checkout-id marker the catalog
        /// authority reads back on every lease.
        ///
        /// Overlay work needs several checkouts per project, each with its
        /// own identity, which the single-attachment helper above cannot
        /// express.
        pub(crate) fn attach_overlay_checkout(
            &self,
            project_id: &str,
            scope: &PublishedScope,
            checkout_dir: &Path,
            attachment_id: &str,
            checkout_id: &str,
            repo_knowledge: bool,
        ) {
            self.attach_overlay_checkout_at(
                project_id,
                scope,
                checkout_dir,
                checkout_dir,
                attachment_id,
                checkout_id,
                repo_knowledge,
            );
        }

        /// The same attachment with its project root nested below the
        /// checkout root, which is what a published scope other than "."
        /// looks like on disk: the repository is the checkout, and the
        /// `.bbox` root lives at the scope's relative path inside it.
        #[allow(clippy::too_many_arguments)] // one argument per durable attachment field
        pub(crate) fn attach_overlay_checkout_at(
            &self,
            project_id: &str,
            scope: &PublishedScope,
            checkout_dir: &Path,
            project_dir: &Path,
            attachment_id: &str,
            checkout_id: &str,
            repo_knowledge: bool,
        ) {
            std::fs::create_dir_all(checkout_dir.join(".bbox/local")).unwrap();
            std::fs::write(
                checkout_dir.join(".bbox/local/checkout-id"),
                format!("{checkout_id}\n"),
            )
            .unwrap();
            let project_id = ProjectId::parse(project_id).unwrap();
            let attachment_id = AttachmentId::parse(attachment_id).unwrap();
            let scope = scope.clone();
            let checkout_dir = checkout_dir.to_string_lossy().into_owned();
            let project_dir = project_dir.to_string_lossy().into_owned();
            let checkout_id = checkout_id.to_string();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    attachments.attachments.insert(
                        attachment_id.clone(),
                        bbox_corpus_core::project_catalog::CheckoutAttachment {
                            attachment_id: attachment_id.clone(),
                            project_id: project_id.clone(),
                            checkout_id: checkout_id.clone(),
                            checkout_dir: checkout_dir.clone(),
                            checkout_project_dir: project_dir.clone(),
                            project_root_relpath: scope.bbox_root_relpath().to_string(),
                            kind: bbox_corpus_core::project_catalog::AttachmentKind::Base,
                            validated_scope: Some(scope.clone()),
                            computed_repo_hint: None,
                            branch_ref: Some("refs/heads/main".into()),
                            capabilities:
                                bbox_corpus_core::project_catalog::AttachmentCapabilities {
                                    repo_knowledge,
                                    ..Default::default()
                                },
                            status: bbox_corpus_core::project_catalog::AttachmentStatus::Attached,
                            attached_at: "2026-08-03T00:00:00Z".into(),
                            detached_at: None,
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        /// Detach one attachment, clearing its capability bits the way the
        /// real detach operation does.
        pub(crate) fn detach(&self, attachment_id: &str) {
            Self::detach_in(&self.store, attachment_id);
        }

        /// Detach through one specific store handle.
        ///
        /// A server owns its own store instance, so a fixture-side
        /// transaction is invisible to a request already in flight. A test
        /// that needs a detach to land mid-request has to drive the store
        /// the server is actually reading.
        pub(crate) fn detach_in_server(server: &BlackboxServer, attachment_id: &str) {
            Self::detach_in(
                &server
                    .state
                    .project_authority
                    .catalog_store()
                    .expect("catalog authority"),
                attachment_id,
            );
        }

        fn detach_in(store: &ProjectCatalogStore, attachment_id: &str) {
            let attachment_id = AttachmentId::parse(attachment_id).unwrap();
            let epoch = store.snapshot().unwrap().epoch();
            store
                .transact(epoch, |_catalog, attachments| {
                    let row = attachments.attachments.get_mut(&attachment_id).unwrap();
                    row.status = bbox_corpus_core::project_catalog::AttachmentStatus::Detached;
                    row.detached_at = Some("2026-08-03T01:00:00Z".into());
                    row.capabilities = Default::default();
                    Ok(())
                })
                .unwrap();
        }

        /// Replace one attachment's capability bits.
        ///
        /// Additive companion to the attach helpers, which grant
        /// `repo_knowledge` only and therefore cannot express the section 9
        /// rows whose gate is a different bit. Composes with any of them
        /// rather than duplicating their identity-marker setup, so a row
        /// that needs `render_output` or `blame` attaches normally and then
        /// says so.
        pub(crate) fn grant_capabilities(
            &self,
            attachment_id: &str,
            capabilities: bbox_corpus_core::project_catalog::AttachmentCapabilities,
        ) {
            let attachment_id = AttachmentId::parse(attachment_id).unwrap();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    attachments
                        .attachments
                        .get_mut(&attachment_id)
                        .expect("attachment exists")
                        .capabilities = capabilities;
                    Ok(())
                })
                .unwrap();
        }

        /// The catalog store, for tests that need to drive its real
        /// failure modes (`poison_for_test`) rather than mock an authority.
        pub(crate) fn store(&self) -> &Arc<ProjectCatalogStore> {
            &self.store
        }

        /// The fixture's temp root, for tests that need to materialize a
        /// checkout directory before attaching it.
        pub(crate) fn root(&self) -> &Path {
            &self.root
        }

        pub(crate) fn epoch(&self) -> u64 {
            self.store.snapshot().unwrap().epoch()
        }

        /// Migrate one project's catalog scope, leaving its accepted
        /// publication untouched. That is the publication bridge state.
        pub(crate) fn migrate_project_scope(&self, project_id: &str, scope: &PublishedScope) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let scope = scope.clone();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |catalog, _attachments| {
                    let project = catalog.projects.get_mut(&project_id).unwrap();
                    project.scope = ProjectScope::Published(scope.clone());
                    Ok(())
                })
                .unwrap();
        }

        pub(crate) fn install_publication(
            &self,
            project_id: &str,
            scope: &PublishedScope,
            accepted_commit: &str,
            knowledge: &[KnowledgeEntry],
            gaps: &[GapNote],
        ) -> InstalledAcceptedPublicationForTest {
            let project_id = ProjectId::parse(project_id).unwrap();
            let relative = |lane: &str, id: &str| {
                if scope.bbox_root_relpath() == "." {
                    format!(".bbox/{lane}/{id}.json")
                } else {
                    format!("{}/.bbox/{lane}/{id}.json", scope.bbox_root_relpath())
                }
            };
            install_accepted_publication_for_test(
                &self.catalog_projects_path,
                &project_id,
                &Self::attachment(),
                scope,
                "refs/heads/main",
                accepted_commit,
                // The fixture must hash exactly what a WRITER commits.
                // Encoding these independently is how a suite goes
                // vacuously green: accepted publication hashes source bytes
                // exactly (D-014), so a fixture with its own encoding
                // produces generations describing bytes no writer would
                // ever produce, and every byte comparison against them
                // passes without proving anything.
                knowledge
                    .iter()
                    .map(|entry| AcceptedPublicationSourceFileForTest {
                        repository_relative_filename: relative("knowledge", &entry.id),
                        source_bytes: bbox_knowledge::knowledge::committed_knowledge_entry_bytes(
                            entry,
                        )
                        .unwrap(),
                    })
                    .collect(),
                gaps.iter()
                    .map(|gap| AcceptedPublicationSourceFileForTest {
                        repository_relative_filename: relative("gaps", &gap.id),
                        source_bytes: bbox_gaps::gaps::committed_gap_note_bytes(gap).unwrap(),
                    })
                    .collect(),
            )
            .unwrap()
        }

        /// Move one pointer to another attachment. Accepted content does
        /// not change, so a content-stamp-keyed cache must survive it.
        pub(crate) fn rebind(&self, project_id: &str, new_attachment: &str) {
            rebind_accepted_pointer_for_test(
                &self.catalog_projects_path,
                &ProjectId::parse(project_id).unwrap(),
                &AttachmentId::parse(new_attachment).unwrap(),
            )
            .unwrap();
        }

        pub(crate) fn corrupt_generation(&self, project_id: &str, generation_id: &str) {
            corrupt_accepted_generation_for_test(
                &self.catalog_projects_path,
                &ProjectId::parse(project_id).unwrap(),
                generation_id,
            )
            .unwrap();
        }

        /// A fresh server over the same durable bytes. Calling this twice
        /// is a restart: new runtime, empty caches, unchanged state.
        pub(crate) fn server(&self) -> BlackboxServer {
            BlackboxServer::new(Arc::new(SharedState::for_test_catalog(
                &self.root,
                &self.catalog_projects_path,
            )))
        }

        pub(crate) fn server_with_render_locality_cutover(
            &self,
            project_id: &str,
        ) -> BlackboxServer {
            let mut state = SharedState::for_test_catalog(&self.root, &self.catalog_projects_path);
            state.render_locality_cutover = Arc::new(
                bbox_indexing::render_locality_cutover::RenderLocalityCutoverRuntimeV1::governed_for_test(
                    project_id,
                ),
            );
            BlackboxServer::new(Arc::new(state))
        }

        /// A server whose records provider is wrapped, over the same
        /// durable bytes as `server()`.
        ///
        /// Added for the clause 1 exit proof, which needs two servers that
        /// differ ONLY in the attached-row view. Wrapping happens before
        /// the state is shared, so no cached projection straddles the swap.
        pub(crate) fn server_with_records_provider(
            &self,
            wrap: impl FnOnce(
                Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider>,
            )
                -> Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider>,
        ) -> BlackboxServer {
            let mut state = SharedState::for_test_catalog(&self.root, &self.catalog_projects_path);
            state.records_provider = wrap(state.records_provider.clone());
            BlackboxServer::new(Arc::new(state))
        }

        /// The same server with the real catalog checkout authority in
        /// place of the deny probe.
        ///
        /// Published reads prove they need no checkout by running against
        /// the deny probe; overlays are the one catalog read that does open
        /// one, so they need the authority that actually resolves an
        /// attachment, verifies its live checkout identity, and enforces
        /// its capability bits.
        pub(crate) fn server_with_checkout_authority(&self) -> BlackboxServer {
            let mut state = SharedState::for_test_catalog(&self.root, &self.catalog_projects_path);
            let store = state
                .project_authority
                .catalog_store()
                .expect("catalog authority");
            state.checkout_access =
                Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
                    Arc::new(
                        bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(
                            store.clone(),
                        ),
                    ),
                    state.checkout_access_observations.clone(),
                ));
            BlackboxServer::new(Arc::new(state))
        }
    }

    pub(crate) fn knowledge_entry(id: &str, content: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.to_string(),
            title: format!("entry {id}"),
            content: content.to_string(),
            cluster: None,
            variants: Default::default(),
            category: Category::Convention,
            scope: Scope::Project,
            // A committed repo-owned file carries no host path, and the
            // accepted normalization drops the field regardless.
            project: None,
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: false,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "user".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            recall_count: 7,
            last_recalled: Some("2026-01-03T00:00:00Z".to_string()),
        }
    }

    pub(crate) fn gap_note(id: &str, title: &str) -> GapNote {
        GapNote {
            id: id.to_string(),
            title: title.to_string(),
            gap_kind: GapKind::Tooling,
            domain: "publication".to_string(),
            wanted_capability: "serve accepted gaps by project identity".to_string(),
            missing_primitive: None,
            fallback_used: None,
            evidence: Vec::new(),
            impact: GapImpact::Medium,
            blocking_level: BlockingLevel::WorkaroundAvailable,
            dedupe_key: "tooling/publication/accepted-view".to_string(),
            suggested_owner: None,
            notes: None,
            supersedes: None,
            superseded_by: None,
            resolution: GapResolution::Unresolved,
            project: None,
            project_id: None,
            write_dir: None,
            provisional_checkout_id: None,
            task_id: None,
            session_id: None,
            provider: None,
            bro: None,
            thread_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-02T00:00:00Z".to_string(),
            resolved_at: None,
            resolution_note: None,
        }
    }
}

#[cfg(test)]
mod clause_two_proof_a {
    //! Plan section 14.2 Proof A: the runtime denial probe.
    //!
    //! Every adapter in the section 9 table is exercised against a catalog
    //! server whose checkout authority is `DenyCheckoutAccess`. Two claims
    //! are asserted per row, and the second is what makes the first mean
    //! something:
    //!
    //! 1. the operation returns its typed refusal or documented degradation;
    //! 2. NOTHING was granted. `granted_leases` sums the broker's per-kind
    //!    grant counters, so a row that refused after opening a checkout, or
    //!    that opened one and then failed for an unrelated reason, fails
    //!    here rather than reading as a clean denial.
    //!
    //! "Before raw filesystem or Git access" is proved structurally rather
    //! than by watching for reads: a `ValidatedCheckoutLease` is the only
    //! handle that yields a checkout root, so zero grants means no adapter
    //! held one. Proof B is what keeps that true by rejecting new unleased
    //! paths to a checkout root; the two proofs are load-bearing together.
    //!
    //! Corpus-only rows assert the complementary claim from the same plan
    //! sentence: the observation sequence is UNCHANGED, so those paths did
    //! not consult checkout authority at all.

    use std::path::PathBuf;

    use rmcp::handler::server::wrapper::Parameters;

    use super::BlackboxServer;
    use super::catalog_fixture::{COMMIT_ONE, CatalogFixture, gap_note, knowledge_entry};

    const PROJECT: &str = "p_proof_a";

    const ATTACHMENT: &str = "att_00000000000000000000000000000e01";
    const CHECKOUT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaae01";

    /// A published catalog project with accepted content AND a fully capable
    /// attached checkout.
    ///
    /// The attachment is the load-bearing part of this proof, and it is easy
    /// to get wrong in the direction that makes every row pass vacuously.
    /// With no attachment, each adapter refuses with attachment-required no
    /// matter what the authority does, so the probe would prove remote-only
    /// degradation (clause 3's job) while claiming to prove denial. Granting
    /// every capability leaves `DenyCheckoutAccess` as the ONLY reason any
    /// row can refuse, which is what makes the mutation control below able
    /// to turn these rows red.
    fn denied_fixture() -> (
        CatalogFixture,
        bbox_corpus_core::identity::PublishedScope,
        PathBuf,
    ) {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "published")],
            &[gap_note("gap-11111111", "published")],
        );
        let checkout = fixture.root().join("capable-checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("probe.txt"), b"real").unwrap();
        fixture.attach_overlay_checkout(PROJECT, &scope, &checkout, ATTACHMENT, CHECKOUT_ID, true);
        fixture.grant_capabilities(ATTACHMENT, every_capability());
        (fixture, scope, checkout)
    }

    fn every_capability() -> bbox_corpus_core::project_catalog::AttachmentCapabilities {
        bbox_corpus_core::project_catalog::AttachmentCapabilities {
            local_code_source: true,
            git_history: true,
            repo_knowledge: true,
            blame: true,
            render_output: true,
            provenance_note_io: true,
            artifact_watching: true,
            repo_mutation: true,
        }
    }

    fn granted_leases(server: &BlackboxServer) -> u64 {
        server
            .state
            .checkout_access
            .health()
            .operations
            .iter()
            .map(|operation| operation.granted)
            .sum()
    }

    fn observation_sequence(server: &BlackboxServer) -> u64 {
        server.state.checkout_access.health().sequence
    }

    fn text_of(result: &rmcp::model::CallToolResult) -> String {
        format!("{:?}", result.content)
    }

    /// Assert one checkout-backed row: refused, and nothing opened.
    fn assert_denied(server: &BlackboxServer, row: &str, refusal: &str) {
        assert_eq!(
            granted_leases(server),
            0,
            "{row}: a denied row must not have opened a checkout"
        );
        assert!(
            refusal.contains("attachment")
                || refusal.contains("denied")
                || refusal.contains("capability")
                || refusal.contains("unavailable")
                || refusal.contains("catalog"),
            "{row}: refusal is not a typed degradation: {refusal}"
        );
    }

    /// Row 5 (render/file provider) and row 4 (blame) reach the checkout
    /// through the tool surface; both must refuse without opening one.
    #[tokio::test]
    async fn blame_and_render_refuse_without_opening_a_checkout() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server();

        let blame = server
            .bbox_blame(Parameters(crate::mcp_tools::blame::BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(1),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_eq!(blame.is_error, Some(true), "{blame:?}");
        assert_denied(&server, "blame", &text_of(&blame));

        let render = server
            .bbox_render(Parameters(crate::knowledge::RenderParams {
                project: Some(PROJECT.into()),
                scope: Some("project".into()),
                ..Default::default()
            }))
            .await;
        assert_eq!(render.is_error, Some(true), "{render:?}");
        assert_denied(&server, "render", &text_of(&render));
    }

    /// Row 5, the provider half: a `file:` ref resolves through the same
    /// authority and must refuse rather than reading the working tree.
    #[test]
    fn file_provider_refuses_without_opening_a_checkout() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server();
        let context = server.provider_context();

        let error = bbox_providers::providers::file::resolve_file(&context, "src/lib.rs")
            .expect_err("file refs need a checkout");

        assert_denied(&server, "file provider", &error.to_string());
    }

    /// Row 6: legacy Git-note import and export both refuse; the PLAN is
    /// corpus computation and is covered by the corpus-only row below.
    #[tokio::test]
    async fn provenance_note_io_refuses_without_opening_a_checkout() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server();

        let export = server
            .bbox_provenance_export(Parameters(crate::mcp_tools::provenance::ProvenanceParams {
                project_id: Some(PROJECT.into()),
            }))
            .await;
        assert_eq!(export.is_error, Some(true), "{export:?}");
        assert_denied(&server, "provenance export", &text_of(&export));

        let import = server
            .bbox_provenance_import(Parameters(crate::mcp_tools::provenance::ProvenanceParams {
                project_id: Some(PROJECT.into()),
            }))
            .await;
        assert_eq!(import.is_error, Some(true), "{import:?}");
        assert_denied(&server, "provenance import", &text_of(&import));
    }

    /// Row 7: catalog-targeted mutation refuses. Eject is the mutation the
    /// plan names explicitly (section 4.19 keeps unregistered init as the
    /// one sanctioned bootstrap exception, so it is not this row).
    #[tokio::test]
    async fn catalog_mutation_refuses_without_opening_a_checkout() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server();

        let eject = server
            .bbox_project_eject(Parameters(bbox_indexing::projects::ProjectEjectParams {
                project: PROJECT.into(),
                dry_run: None,
            }))
            .await;

        assert_eq!(eject.is_error, Some(true), "{eject:?}");
        assert_denied(&server, "mutation", &text_of(&eject));
    }

    /// Row 2, overlay half: `own` visibility needs `KnowledgeGapOverlayRead`
    /// and degrades with the typed overlay error rather than serving a
    /// snapshot it could not compute.
    ///
    /// The row is driven through a real session checkout on a real
    /// attachment deliberately. Calling `own` with no session context
    /// refuses one gate EARLIER, on missing authoritative context, and would
    /// have made this row pass without the overlay lease ever being
    /// attempted: a green test proving nothing about the capability it
    /// claims to cover.
    #[test]
    fn own_overlay_degrades_without_opening_a_checkout() {
        let (fixture, scope, checkout) = denied_fixture();
        let server = fixture.server();
        server.set_session_checkout_for_test(
            PROJECT.into(),
            scope.clone(),
            CHECKOUT_ID.into(),
            checkout.clone(),
        );

        let error = server
            .session_knowledge_view(None, Some("own"))
            .err()
            .expect("own cannot answer without its overlay lease");
        let refusal = format!("{error:#}");

        assert!(
            refusal.contains("error.provisional_overlay_unavailable"),
            "own must degrade with its typed overlay error: {refusal}"
        );
        assert_denied(&server, "own overlay", &refusal);

        // Published content is unaffected: the accepted generation needs no
        // checkout at all, which is the degradation the table promises.
        assert!(
            server
                .session_knowledge_view(None, Some("published"))
                .is_ok_and(|view| !view.knowledge.all_entries().is_empty())
        );
    }

    /// Rows 2 (published read), 6 (plan), and the corpus half of the table:
    /// these consult NO checkout authority, so the observation sequence is
    /// untouched. An unchanged sequence is a stronger statement than zero
    /// grants: it means the broker was never even asked.
    #[test]
    fn corpus_only_reads_leave_the_observation_sequence_unchanged() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server();
        let before = observation_sequence(&server);

        let knowledge = server
            .session_knowledge_view(None, None)
            .expect("published knowledge serves from accepted content");
        assert!(
            !knowledge.knowledge.all_entries().is_empty(),
            "the row is vacuous unless accepted content actually served"
        );
        let gaps = server
            .session_gap_view(None, None)
            .expect("published gaps serve from accepted content");
        assert!(!gaps.gaps.all().is_empty(), "accepted gaps served");

        assert_eq!(
            observation_sequence(&server),
            before,
            "a published read must not consult checkout authority at all"
        );
        assert_eq!(granted_leases(&server), 0);
    }

    /// The vacuity guard, and the mutation control the plan's "blocking"
    /// wording actually requires.
    ///
    /// Every row above asserts a REFUSAL, and refusals are easy to produce
    /// by accident. Against the REAL catalog authority, over the same
    /// fixture and the same capable attachment, the identical file read
    /// succeeds and the grant counter moves. Swapping any denial row above
    /// to this authority therefore turns it red, which is what makes those
    /// rows evidence rather than decoration.
    #[test]
    fn the_probe_is_not_vacuous_against_a_real_authority() {
        let (fixture, _, _) = denied_fixture();
        let server = fixture.server_with_checkout_authority();

        let file =
            bbox_providers::providers::file::resolve_file(&server.provider_context(), "probe.txt")
                .expect("the positive control must succeed against a real authority");

        assert_eq!(file.content, b"real");
        assert!(
            granted_leases(&server) > 0,
            "a successful read must be lease-counted"
        );
    }
}

#[cfg(test)]
mod clause_three_exit_proof {
    //! Plan section 14.3: remote-only projects degrade PER CAPABILITY.
    //!
    //! One project, valid accepted content, zero attachments, walked across
    //! the section 9 table. The claim under proof is not "everything
    //! refuses": it is that the content-domain surfaces keep serving from
    //! accepted bytes while exactly the checkout-backed surfaces refuse, and
    //! that the refusals name the missing ATTACHMENT rather than a missing
    //! project.
    //!
    //! Every row asserts the positive half too where the table promises one.
    //! A walk that only checked refusals would stay green if accepted
    //! publication broke entirely, which is the opposite of what clause 3
    //! claims.

    use rmcp::handler::server::wrapper::Parameters;

    use super::BlackboxServer;
    use super::catalog_fixture::{COMMIT_ONE, CatalogFixture, gap_note, knowledge_entry};

    const PROJECT: &str = "p_clause_three";

    /// Published, with accepted content, and deliberately never attached.
    fn remote_only() -> (CatalogFixture, BlackboxServer) {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted")],
            &[gap_note("gap-11111111", "accepted")],
        );
        let server = fixture.server();
        (fixture, server)
    }

    fn text_of(result: &rmcp::model::CallToolResult) -> String {
        format!("{:?}", result.content)
    }

    /// A refusal must name the missing attachment, never a missing project.
    /// Plan 10.5 fixes this: file, blame, render, and mutation do not
    /// translate a missing attachment into project-not-found, because an
    /// operator who sees "not registered" goes looking for a registration
    /// that already exists.
    fn assert_attachment_required(row: &str, refusal: &str) {
        assert!(
            refusal.contains("attachment"),
            "{row}: remote-only must degrade on the ATTACHMENT: {refusal}"
        );
        assert!(
            !refusal.contains("not_registered") && !refusal.contains("selector_unknown"),
            "{row}: a remote-only project is registered; the refusal must not deny its identity: {refusal}"
        );
    }

    /// The content-domain half of the table: published knowledge and gaps
    /// serve accepted bytes with zero attachments, and the provenance PLAN
    /// succeeds because it is corpus computation that opens no Git notes.
    #[test]
    fn accepted_content_serves_with_zero_attachments() {
        let (_fixture, server) = remote_only();

        let knowledge = server
            .session_knowledge_view(None, None)
            .expect("published knowledge serves without any checkout");
        assert!(
            knowledge
                .knowledge
                .all_entries()
                .iter()
                .any(|entry| entry.id == "knowledge-a"),
            "accepted knowledge must actually serve, not merely not-fail"
        );

        let gaps = server
            .session_gap_view(None, None)
            .expect("published gaps serve without any checkout");
        assert!(
            gaps.gaps.all().iter().any(|gap| gap.id == "gap-11111111"),
            "accepted gaps must actually serve"
        );
    }

    /// Blame, render, the file provider, and Git-note I/O all refuse, and
    /// all refuse on the attachment.
    #[tokio::test]
    async fn checkout_backed_surfaces_return_attachment_required() {
        let (_fixture, server) = remote_only();

        let blame = server
            .bbox_blame(Parameters(crate::mcp_tools::blame::BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(1),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_eq!(blame.is_error, Some(true));
        assert_attachment_required("blame", &text_of(&blame));

        let render = server
            .bbox_render(Parameters(crate::knowledge::RenderParams {
                project: Some(PROJECT.into()),
                scope: Some("project".into()),
                ..Default::default()
            }))
            .await;
        assert_eq!(render.is_error, Some(true));
        assert_attachment_required("render", &text_of(&render));

        let file =
            bbox_providers::providers::file::resolve_file(&server.provider_context(), "src/lib.rs")
                .expect_err("a file ref needs a checkout");
        assert_attachment_required("file provider", &file.to_string());

        let export = server
            .bbox_provenance_export(Parameters(crate::mcp_tools::provenance::ProvenanceParams {
                project_id: Some(PROJECT.into()),
            }))
            .await;
        assert_eq!(export.is_error, Some(true));
        assert_attachment_required("provenance note io", &text_of(&export));

        let eject = server
            .bbox_project_eject(Parameters(bbox_indexing::projects::ProjectEjectParams {
                project: PROJECT.into(),
                dry_run: None,
            }))
            .await;
        assert_eq!(eject.is_error, Some(true));
        assert_attachment_required("catalog mutation", &text_of(&eject));
    }

    /// `own` has no honest answer without a checkout of one's own, and says
    /// so with the typed overlay code rather than serving published content
    /// relabelled as provisional.
    #[test]
    fn own_returns_provisional_overlay_unavailable() {
        let (_fixture, server) = remote_only();

        let error = server
            .session_knowledge_view(None, Some("own"))
            .err()
            .expect("own cannot answer for a project with no checkout");

        assert!(
            format!("{error:#}").contains("checkout"),
            "own degrades on the absent checkout: {error:#}"
        );
    }

    /// Capability status reports AVAILABILITY and invents no denial counts.
    ///
    /// Plan 4.17 is explicit that a capability which was never attempted is
    /// not a denial. A remote-only project has no attachment to carry bits
    /// at all, so the honest report is an empty attachment list beside a
    /// healthy accepted state, and the durable observation counters must not
    /// have moved for operations nobody ran.
    #[test]
    fn capability_status_reports_availability_without_inventing_denials() {
        let (_fixture, server) = remote_only();
        let before = server.state.checkout_access.health().sequence;

        let status = server
            .state
            .project_runtime_status(PROJECT)
            .expect("a remote-only project still has runtime status");

        assert!(
            status.attachments.is_empty(),
            "no attachment means no capability bits, not denied ones: {:?}",
            status.attachments
        );
        assert!(
            status.accepted.serves_published_content,
            "accepted content is healthy and says so: {:?}",
            status.accepted
        );
        // `advance_available` is deliberately NOT asserted here. Its
        // contract is accepted-side availability only: it answers "is the
        // pointer in a state that permits a future advance", and the
        // publisher surface adds the attachment, capability, and CAS gates
        // on top. The attachment requirement is a property of the
        // OPERATION, so the row below exercises the operation instead of
        // reading a field that never promised to carry it.
        assert_eq!(
            server.state.checkout_access.health().sequence,
            before,
            "reading status must not synthesize checkout observations"
        );
    }

    /// Plan 14.3, publisher advance: the OPERATION returns
    /// attachment-required for a project with nothing attached.
    ///
    /// This is the row that carries the attachment gate. The accepted-side
    /// `advance_available` flag reports pointer health and says nothing
    /// about attachments by design, so asserting it here would have proved
    /// the wrong thing in the reassuring direction.
    #[tokio::test]
    async fn publisher_advance_returns_attachment_required() {
        let (fixture, server) = remote_only();
        // Real CAS tokens, so the refusal below is the ATTACHMENT gate and
        // not the earlier missing-token gate. Getting this wrong produces a
        // green row that never reached the property under proof.
        let status = server
            .state
            .project_runtime_status(PROJECT)
            .expect("status carries the tokens an advance must present");

        let advance = server
            .bbox_project_publisher_advance(Parameters(
                crate::tools::project_catalog::ProjectPublisherAdvanceParams {
                    project_id: PROJECT.into(),
                    attachment_id: Some(CatalogFixture::attachment().as_str().to_string()),
                    source_generation_id: None,
                    mode: "advance".into(),
                    full_ref: Some("refs/heads/main".into()),
                    expected_generation_id: status.accepted.generation_id.clone(),
                    expected_pointer_sha256: status.binding.pointer_sha256.clone(),
                    auto_advance: None,
                    dry_run: false,
                    expected_catalog_epoch: fixture.epoch(),
                    audit_reason: "clause three walk".into(),
                },
            ))
            .await;

        assert_eq!(advance.is_error, Some(true), "{advance:?}");
        assert_attachment_required("publisher advance", &text_of(&advance));
    }

    /// No watcher is installed, and that is reported as an absence rather
    /// than a fault: `capable_but_unregistered` is the actionable state, and
    /// a project with no attachment cannot populate it.
    #[test]
    fn no_watcher_is_installed_and_none_is_owed() {
        let (_fixture, server) = remote_only();

        let status = server
            .state
            .project_runtime_status(PROJECT)
            .expect("status");

        assert!(status.watcher.registered_attachments.is_empty());
        assert!(
            status.watcher.capable_but_unregistered.is_empty(),
            "a project with no attachment owes no registration: {:?}",
            status.watcher
        );
    }

    /// An unreadable catalog pair is REPORTED, not dropped, and does not
    /// take durable publication down with it.
    ///
    /// The first cut of this row was vacuous in a way worth recording: it
    /// poisoned the FIXTURE's store handle and then queried a server that
    /// `CatalogFixture::server()` had opened over the same files with its
    /// OWN handle. Nothing under test was ever poisoned, the row passed,
    /// and it hid a real production defect underneath. Poisoning the
    /// server-owned handle is the difference between exercising the state
    /// and describing it.
    #[test]
    fn an_unreadable_catalog_pair_is_reported_without_inventing_denials() {
        let (_fixture, server) = remote_only();
        let before = server.state.checkout_access.health().sequence;
        // The handle the SERVER reads through, not the fixture's.
        let store = server
            .state
            .project_authority
            .catalog_store()
            .expect("catalog authority")
            .clone();

        let restore = store
            .poison_for_test("clause three: catalog pair unreadable")
            .expect("the store is readable before poisoning");

        let poisoned = server
            .state
            .project_runtime_status(PROJECT)
            .expect("an unreadable catalog must still report the project");
        assert_eq!(
            poisoned.catalog_authority, "unavailable",
            "the cause is named explicitly: {poisoned:?}"
        );
        // Accepted publication is a SEPARATE durable store and is verified
        // independently, so it keeps reporting through a catalog failure.
        // That separation is the architectural point of the split.
        assert!(
            poisoned.accepted.serves_published_content,
            "accepted publication survives an unreadable catalog pair: {:?}",
            poisoned.accepted
        );
        // Everything catalog-derived is empty because it is UNKNOWN. None
        // of it is a denial; nothing was attempted (plan 4.17).
        assert!(poisoned.attachments.is_empty());
        assert!(poisoned.overlays.is_empty());
        assert!(poisoned.watcher.capable_but_unregistered.is_empty());
        assert_eq!(
            server.state.checkout_access.health().sequence,
            before,
            "a poisoned catalog must not manufacture checkout observations"
        );

        store.unpoison_for_test(restore);
        let recovered = server
            .state
            .project_runtime_status(PROJECT)
            .expect("status returns once the pair is readable again");
        assert_eq!(
            recovered.catalog_authority, "available",
            "the row above was not passing on a permanently broken fixture"
        );
        assert!(recovered.accepted.serves_published_content);
    }

    /// The half the vacuous row could never have caught: doctor must
    /// REPORT the project, not silently drop it.
    ///
    /// `catalog_project_statuses` collects through `filter_map`, so a
    /// status of `None` removed the project from the report entirely and an
    /// unreadable catalog looked like a healthy host with fewer projects.
    #[test]
    fn doctor_reports_a_project_whose_catalog_authority_is_unreadable() {
        let (_fixture, server) = remote_only();
        let store = server
            .state
            .project_authority
            .catalog_store()
            .expect("catalog authority")
            .clone();
        let healthy = crate::doctor::catalog_sections_for_test(&server.state);
        assert!(
            healthy.iter().any(|line| line.contains(PROJECT)) || !healthy.is_empty(),
            "the healthy report mentions the catalog sections at all: {healthy:?}"
        );

        let restore = store
            .poison_for_test("doctor: catalog pair unreadable")
            .unwrap();
        let rendered = crate::doctor::catalog_sections_for_test(&server.state);
        store.unpoison_for_test(restore);

        let joined = rendered.join("\n");
        assert!(
            joined.contains(PROJECT),
            "an unreadable project must appear in the report: {joined}"
        );
        assert!(
            joined.contains("could not be read from the catalog pair"),
            "and must name the cause rather than a downstream symptom: {joined}"
        );
        // BOTH facts. The catalog and the accepted pointer are separate
        // durable stores that degrade separately, so an operator seeing
        // only the unreadable-catalog line cannot tell whether published
        // content is still serving, and cannot find out mid-poisoning:
        // bbox_project_publisher_status needs a catalog snapshot itself.
        assert!(
            joined.contains("verified independently") && joined.contains("CURRENT"),
            "the independently verified accepted state must stay visible: {joined}"
        );
        assert!(
            joined.contains("keep serving"),
            "and must say published reads continue: {joined}"
        );
    }

    /// The vacuity guard for the whole walk: attach a capable checkout to
    /// the SAME fixture and the refusing rows start succeeding. Without
    /// this, every assertion above would still hold if accepted publication
    /// were broken and the project simply did not exist.
    #[test]
    fn the_walk_is_not_vacuous_once_an_attachment_exists() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        fixture.install_publication(
            PROJECT,
            &scope,
            COMMIT_ONE,
            &[knowledge_entry("knowledge-a", "accepted")],
            &[gap_note("gap-11111111", "accepted")],
        );
        let remote_status = fixture
            .server()
            .state
            .project_runtime_status(PROJECT)
            .expect("status");
        assert!(remote_status.attachments.is_empty());

        let checkout = fixture.root().join("late-checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("probe.txt"), b"real").unwrap();
        fixture.attach_overlay_checkout(
            PROJECT,
            &scope,
            &checkout,
            "att_00000000000000000000000000000c31",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaac31",
            true,
        );
        fixture.grant_capabilities(
            "att_00000000000000000000000000000c31",
            bbox_corpus_core::project_catalog::AttachmentCapabilities {
                render_output: true,
                ..Default::default()
            },
        );
        let attached = fixture.server_with_checkout_authority();

        let file = bbox_providers::providers::file::resolve_file(
            &attached.provider_context(),
            "probe.txt",
        )
        .expect("the same read succeeds once an attachment exists");
        assert_eq!(file.content, b"real");

        let status = attached
            .state
            .project_runtime_status(PROJECT)
            .expect("status");
        assert_eq!(
            status.attachments.len(),
            1,
            "the capability view now reports one attachment: {:?}",
            status.attachments
        );
        assert!(
            status
                .attachments
                .iter()
                .any(|attachment| attachment.available.contains(&"render_output")),
            "and reports the bit it actually records: {:?}",
            status.attachments
        );
    }
}
