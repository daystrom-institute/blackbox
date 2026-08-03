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
    #[allow(dead_code)] // P5-A installs the runtime; P5-B reads it.
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
    pub(crate) code_sources: Arc<super::code_source::CodeSourceRuntime>,
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
    store_dir: &std::path::Path,
) -> BTreeMap<String, bbox_corpus_core::git_overlay::GitOverlaySelector> {
    if !matches!(authority, ProjectAuthority::Catalog { .. }) {
        return BTreeMap::new();
    }
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(store_dir);
    match bbox_edge_sidecar::snapshot::selected_git_overlays(&edges_dir) {
        Ok(overlays) => overlays,
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
            super::repo_io::RepoIoAuthority::knowledge_base_carriers(&repo_projects).unwrap(),
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
            super::repo_io::RepoIoAuthority::gap_base_carriers(&repo_projects).unwrap(),
        )
        .unwrap();
        crate::threads::register_thread_embed_hook(crate::embed_queue::enqueue_thread_hook);
        crate::notes::register_note_embed_hook(crate::embed_queue::enqueue_note_hook);
        crate::index::writer_actor::register_embed_bootstrap(
            crate::embed_queue::register_index_embed_hooks,
        );
        crate::providers::register_extra_providers(crate::providers_ext::extra_providers());
        crate::embed::queue::register_contradiction_hook(crate::embed_runtime::contradiction_hook);
        crate::mcp_tools::hybrid_search::register_coverage_status_hook(
            crate::embed_runtime::status_response_for_buckets,
        );
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
            code_sources: Arc::new(super::code_source::CodeSourceRuntime::for_test(store_dir)),
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
mod code_read_view_tests {
    use super::*;

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
                        attachment_id: "att_0000000000000000000000000000ep01".to_string(),
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
}

/// Catalog-mode view fixtures shared by the published knowledge and gap
/// view tests. Building catalog state means a catalog store, published
/// projects with no attachment (the remote-only case), and accepted bytes
/// installed through the owning crate's real preparation path.
#[cfg(test)]
pub(crate) mod catalog_fixture {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{AttachmentId, CorpusProject, ProjectId, ProjectScope};
    use bbox_gaps::gaps::{BlockingLevel, GapImpact, GapKind, GapNote, GapResolution};
    use bbox_indexing::accepted_publication_test_support::{
        AcceptedPublicationSourceFileForTest, InstalledAcceptedPublicationForTest,
        corrupt_accepted_generation_for_test, install_accepted_publication_for_test,
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
                knowledge
                    .iter()
                    .map(|entry| AcceptedPublicationSourceFileForTest {
                        repository_relative_filename: relative("knowledge", &entry.id),
                        source_bytes: serde_json::to_vec(entry).unwrap(),
                    })
                    .collect(),
                gaps.iter()
                    .map(|gap| AcceptedPublicationSourceFileForTest {
                        repository_relative_filename: relative("gaps", &gap.id),
                        source_bytes: serde_json::to_vec(gap).unwrap(),
                    })
                    .collect(),
            )
            .unwrap()
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
