use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;
use rmcp::handler::server::router::tool::ToolRouter;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::notes::Notes;
use crate::orchestration::tail::TailEvent;
use crate::orchestration::{self, TaskStore};
use crate::packets::Packets;
use crate::pins::Pins;
use crate::projects::ProjectRegistry;
use crate::roadmap::Roadmap;
use crate::threads::Threads;
use crate::{
    artifacts, council, crons, edge_index, lsp, path_cache, pollers, slack_channel_bindings,
    slack_proposal_links, slack_thread_store, system_events, webhooks, whiteboards, workflow,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub(crate) struct SharedState {
    pub(crate) idx: RwLock<TranscriptIndex>,
    pub(crate) kb: RwLock<Knowledge>,
    pub(crate) roadmap: RwLock<Roadmap>,
    pub(crate) threads: RwLock<Threads>,
    pub(crate) notes: RwLock<Notes>,
    pub(crate) pins: RwLock<Pins>,
    pub(crate) projects: RwLock<ProjectRegistry>,
    pub(crate) packets: RwLock<Packets>,
    pub(crate) artifacts: RwLock<artifacts::ArtifactCatalog>,
    pub(crate) bbox_watcher: std::sync::Mutex<Option<crate::watcher::BbxWatcher>>,
    #[allow(dead_code)]
    pub(crate) edge_index: RwLock<edge_index::EdgeIndex>,
    pub(crate) path_cache: RwLock<path_cache::PathCache>,
    pub(crate) task_store: Arc<RwLock<TaskStore>>,
    pub(crate) tail_tx: broadcast::Sender<TailEvent>,
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
    /// Multi-peer chat councils — TUI-driven deliberation surface.
    /// One drain worker per (council × bro) serializes resumes for
    /// that bro; daemon-wide collisions on the same provider session
    /// are prevented via `resume_leases`.
    pub(crate) councils: council::SharedRegistry,
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
    pub(crate) badgey_registry: Arc<orchestration::badgey::BadgeyRegistry>,
    pub(crate) badgey_proposals: Arc<orchestration::badgey::ProposalStore>,
    pub(crate) badgey_journal: Arc<orchestration::badgey::ActionJournal>,
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
    /// Lazy-spawned per-project LSP sessions (JDTLS, rust-analyzer).
    /// Refactor tools call `with_session` instead of starting a fresh
    /// child every call; the manager amortizes initialize cost and
    /// idle-evicts sessions on a background tick.
    pub(crate) lsp_sessions: lsp::LspSessionManager,
    pub(crate) config: std::sync::Arc<parking_lot::RwLock<crate::config::Config>>,
    pub(crate) atom_invocation_store: orchestration::atoms::invocation::SharedInvocationStore,
    // kept: SharedState vector store handle; consumed by embed/queue path through alternate state plumbing, retained here for direct access
    #[allow(dead_code)]
    pub(crate) vector_store: std::sync::Arc<crate::vectors::VectorStore>,
    pub(crate) system_events: system_events::SharedEventHub,
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

    #[cfg(test)]
    pub(crate) fn for_test(store_dir: &std::path::Path) -> SharedState {
        use std::collections::VecDeque;
        let (tail_tx, _) = broadcast::channel(128);
        let idx = TranscriptIndex::open_or_create(
            &store_dir.join("idx"),
            Vec::new(),
            None,
            store_dir.join("projects"),
            store_dir.join("kb.json"),
            store_dir.join("threads.json"),
            store_dir.join("roadmap.json"),
        )
        .unwrap();
        SharedState {
            idx: RwLock::new(idx),
            kb: RwLock::new(Knowledge::open(&store_dir.join("kb.json")).unwrap()),
            roadmap: RwLock::new(Roadmap::open(&store_dir.join("roadmap.json")).unwrap()),
            threads: RwLock::new(Threads::open(&store_dir.join("threads.json")).unwrap()),
            notes: RwLock::new(Notes::open(&store_dir.join("notes.json")).unwrap()),
            pins: RwLock::new(Pins::open(&store_dir.join("pins.json")).unwrap()),
            projects: RwLock::new(ProjectRegistry::open(store_dir.join("projects.json")).unwrap()),
            packets: RwLock::new(Packets::open(store_dir).unwrap()),
            artifacts: RwLock::new(artifacts::ArtifactCatalog::open(store_dir).unwrap()),
            bbox_watcher: std::sync::Mutex::new(None),
            edge_index: RwLock::new(edge_index::EdgeIndex::default()),
            path_cache: RwLock::new(path_cache::PathCache::default()),
            task_store: Arc::new(RwLock::new(TaskStore::new())),
            tail_tx,
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
            councils: Arc::new(council::CouncilRegistry::new()),
            resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
            agent_adapter_registry: Arc::new(RwLock::new(
                orchestration::agents::adapter::AgentAdapterRegistry::new(),
            )),
            badgey_registry: Arc::new(orchestration::badgey::BadgeyRegistry::new()),
            badgey_proposals: Arc::new(
                orchestration::badgey::ProposalStore::new(store_dir.to_path_buf()).unwrap(),
            ),
            badgey_journal: Arc::new(
                orchestration::badgey::ActionJournal::new(store_dir.to_path_buf()).unwrap(),
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
            lsp_sessions: lsp::LspSessionManager::new(),
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
}
