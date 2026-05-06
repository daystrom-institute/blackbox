mod artifacts;
mod chunker;
mod council;
mod crons;
mod edge_index;
mod embed;
mod embed_queue;
mod entity_loader;
pub mod entity_ref;
#[cfg(test)]
#[path = "../eval/check.rs"]
mod eval_check;
mod git;
mod inbox;
mod index;
mod knowledge;
mod mcp_client;
mod mcp_tools;
mod notes;
mod orchestration;
mod packets;
mod parser;
mod path_cache;
mod pins;
mod pollers;
mod projects;
mod providers;
mod query;
mod render;
mod routing;
mod search;
mod system_memory;
mod threads;
mod tool_docs;
mod util;
mod vectors;
mod webhooks;
mod whiteboards;
mod workflow;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use axum::extract::{Query, State as AxumState};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use futures::{stream::Stream, StreamExt};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, IntoContents, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use index::TranscriptIndex;
use knowledge::Knowledge;
use notes::Notes;
use orchestration::providers::{ExecOpts, Provider};
use orchestration::tail::TailEvent;
use orchestration::{self as orch, TaskStore};
use packets::{Packets, ScannerConfig};
use pins::{AmbientPinQuery, PinParams, Pins};
use projects::{ProjectListResponse, ProjectRecord, ProjectRegisterParams, ProjectRegistry};
use providers::ProviderContext;
use threads::Threads;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct SharedState {
    idx: RwLock<TranscriptIndex>,
    kb: RwLock<Knowledge>,
    threads: RwLock<Threads>,
    notes: RwLock<Notes>,
    pins: RwLock<Pins>,
    projects: RwLock<ProjectRegistry>,
    packets: RwLock<Packets>,
    artifacts: RwLock<artifacts::ArtifactCatalog>,
    #[allow(dead_code)]
    edge_index: RwLock<edge_index::EdgeIndex>,
    path_cache: RwLock<path_cache::PathCache>,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf, // BRO_HOME (default: ~/.local/state/blackbox/bro)
    /// In-flight workflow arcs keyed by `arc_thread_id`. Updated at
    /// every node boundary by the engine so /orchestrate/peek can
    /// report the live state without reading notes. Entries persist
    /// after the arc terminates so a peek shortly after close still
    /// works (they stay until the daemon restarts).
    running_arcs: RwLock<HashMap<String, ArcSnapshot>>,
    /// Pending Wait-node registrations indexed by signal name +
    /// correlation. Webhook router and direct `bbox_arc_signal` MCP
    /// calls write into this; suspended arcs block on the per-wait
    /// Notify until a matching signal arrives.
    wait_store: Arc<crate::workflow::wait::WaitStore>,
    /// Operator-installed webhook endpoints. Each carries its
    /// signature scheme + extractor + routing-packet id.
    webhooks: webhooks::SharedRegistry,
    /// Operator-installed pollers — scheduled HTTP-source inlets
    /// that converge on the same `dispatch_routed_event` pipeline as
    /// webhooks. Carries running-task handles so they can be aborted
    /// on uninstall / replaced on reinstall.
    pollers: pollers::SharedRegistry,
    /// Operator-installed crons — calendar-driven inlets (sibling to
    /// webhooks/pollers). Same `dispatch_routed_event` convergence;
    /// distinct registry because the spec shape and concurrency model
    /// differ (pollers fetch HTTP per tick; crons dispatch arcs by
    /// schedule and gate concurrency per-cron).
    crons: crons::SharedRegistry,
    /// Whiteboards — multi-agent deliberation boards shared between
    /// in-workflow ensembles, in-workflow facilitators, and external
    /// agents (operator's Claude, dispatched help, eventually humans
    /// through slack/ntfy adapters). Phase transitions emit routed
    /// signals through `dispatch_routed_event` so wait_for_phase
    /// nodes resume on the same pipeline webhook ingress uses.
    whiteboards: whiteboards::SharedRegistry,
    /// Operator-installed workflow specs by id. Allows
    /// `start_arc{workflow: "name"}` routing verdicts to find their
    /// target without the webhook payload carrying the full spec.
    workflow_registry: Arc<RwLock<HashMap<String, workflow::Workflow>>>,
    /// True iff the daemon's HTTP listener is bound to a loopback
    /// address. Webhook signature scheme `none` is rejected at install
    /// AND at verify when this is false (defense in depth).
    bind_is_loopback: bool,
    /// Bounded ring buffer of recent signal-dispatch events. Every
    /// call to `signal_arc_dispatch` records one entry — whether the
    /// signal matched a pending wait (with the resolved arc/wait ids)
    /// or fell idle (with the pending-with-same-signal snapshot at
    /// dispatch time). Surfaced via `bro_signals` MCP for debugging
    /// "did this webhook actually resolve a wait?" without grepping
    /// the daemon's tracing log.
    signal_log: RwLock<std::collections::VecDeque<SignalEvent>>,
    /// Bounded ring buffer of recent webhook deliveries. Captured by
    /// the webhook handler post-dispatch; carries the extracted
    /// entity, the routing verdict's classification, and the response
    /// returned to the caller. Surfaced via `bro_webhook_deliveries`
    /// MCP — replaces poking the upstream's hook-task table or
    /// reading daemon tracing logs to debug routing-rule misses.
    webhook_delivery_log: RwLock<std::collections::VecDeque<WebhookDelivery>>,
    /// Cancellation tokens for in-flight workflow arcs, keyed by
    /// `arc_id`. Created at run start, removed at terminus. The
    /// `bro_arc_cancel` MCP tool and the `cancel_arc` routing verdict
    /// look up the token and trigger `cancel()`; the runner observes
    /// the token between node iterations and inside Wait suspensions
    /// (via `tokio::select!`), bails out with status `cancelled`, and
    /// runs `on_arc_cancel` + `on_arc_exit` hooks on the way out.
    arc_cancel_tokens: RwLock<HashMap<String, CancellationToken>>,
    /// Multi-peer chat councils — TUI-driven deliberation surface.
    /// One drain worker per (council × bro) serializes resumes for
    /// that bro; daemon-wide collisions on the same provider session
    /// are prevented via `resume_leases`.
    councils: council::SharedRegistry,
    /// Daemon-wide resume lease registry keyed `(provider, session_id)`.
    /// Currently used only by the council drain worker — other resume
    /// paths (`bro_broadcast`, ad-hoc `bro_resume`, advisor) remain a
    /// single-resume-at-a-time assumption that this lease can later
    /// mechanize. Acquire returns an owned guard held across spawn +
    /// wait; drop on completion.
    resume_leases: Arc<orchestration::resume_lease::ResumeLeaseRegistry>,
}

const SIGNAL_LOG_CAP: usize = 200;
const WEBHOOK_LOG_CAP: usize = 200;

#[derive(Debug, Clone, Serialize)]
struct SignalEvent {
    timestamp: String,
    signal: String,
    correlation: serde_json::Map<String, Value>,
    /// `"matched"` when a pending wait resolved, `"no_matching_wait"`
    /// otherwise.
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_arc_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_wait_id: Option<String>,
    /// Snapshot of pending waits with the same signal name at
    /// dispatch time. Empty when the signal matched. When the signal
    /// went idle this is the diff a debugger needs: which arcs were
    /// waiting on this signal name, with what correlation, that
    /// failed to match.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    idle_pending: Vec<crate::workflow::wait::WaitSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
struct WebhookDelivery {
    received_at: String,
    webhook_name: String,
    /// `"webhook"` for live deliveries via `/webhook/:name`,
    /// `"replay"` for the no-signature replay endpoint.
    source: String,
    /// Subset of inbound headers that drove routing (lowercased
    /// `x-*` keys). Full header capture would balloon the buffer and
    /// most non-`x-*` headers carry no routing signal.
    headers: serde_json::Map<String, Value>,
    extracted_entity: Value,
    /// `"start_arc"` / `"signal_arc"` / `"cancel_arc"` / `"ignore"` /
    /// `"dead_letter"` / `"no_match"` (when no rule fired) /
    /// `"extractor_failed"` / `"signature_invalid"` /
    /// `"idempotency_dropped"`. Single string keeps the schema
    /// flat for filter queries.
    verdict_classification: String,
    response_status: u16,
    response_body: Value,
}

impl SharedState {
    fn record_signal(&self, ev: SignalEvent) {
        let mut log = self.signal_log.write();
        if log.len() >= SIGNAL_LOG_CAP {
            log.pop_front();
        }
        log.push_back(ev);
    }

    fn record_webhook(&self, d: WebhookDelivery) {
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
    pub fn register_arc_cancel_token(&self, arc_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        self.arc_cancel_tokens
            .write()
            .insert(arc_id.to_string(), token.clone());
        token
    }

    /// Drop the cancel token for an arc that's reached terminal
    /// state. Called from the runner's exit path so the map doesn't
    /// grow unbounded across daemon uptime.
    pub fn unregister_arc_cancel_token(&self, arc_id: &str) {
        self.arc_cancel_tokens.write().remove(arc_id);
    }

    /// Trigger cancellation for a running arc. Returns whether a
    /// matching token existed (and was triggered). The runner notices
    /// at the next node boundary — or immediately if it's parked on
    /// a Wait, since the wait's `tokio::select!` includes the token's
    /// `cancelled()` arm.
    pub fn cancel_arc(&self, arc_id: &str) -> bool {
        match self.arc_cancel_tokens.read().get(arc_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ArcSnapshot {
    arc_thread_id: String,
    workflow_name: String,
    workflow_version: u32,
    status: String,
    current_node: Option<String>,
    completed_nodes: Vec<String>,
    in_flight_nodes: Vec<String>,
    last_verdict: Option<String>,
    visit_counts: std::collections::HashMap<String, u32>,
    started_at: String,
    updated_at: String,
}

// ---------------------------------------------------------------------------
// MCP Server Handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BlackboxServer {
    state: Arc<SharedState>,
    tool_router: ToolRouter<Self>,
}

impl BlackboxServer {
    const MCP_RESPONSE_CAP_BYTES: usize = 80 * 1024;

    fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: Self::bbox_tools() + Self::bro_tools(),
        }
    }

    fn sync_knowledge_entry_to_index(&self, entry_id: &str) -> anyhow::Result<()> {
        let Some(entry) = self.state.kb.read().entry(entry_id).cloned() else {
            return Ok(());
        };
        let entity_id = crate::index::knowledge_entity_id(entry_id);
        let chunk_hash = crate::index::knowledge_chunk_hash(&entry);
        self.state.idx.write().index_knowledge_entry(&entry)?;
        embed_queue::enqueue_knowledge(&entry, &entity_id, &chunk_hash);
        Ok(())
    }

    fn tombstone_knowledge_entry_in_index(&self, entry_id: &str) -> anyhow::Result<()> {
        self.state.idx.write().delete_knowledge_entry(entry_id)?;
        embed_queue::tombstone_knowledge(&crate::index::knowledge_entity_id(entry_id));
        Ok(())
    }

    fn describe_schema_counts(&self) -> BTreeMap<String, usize> {
        let mut counts =
            mcp_tools::inspect::entity_type_count(&self.state.edge_index.read().known_refs());
        counts.insert("knowledge".into(), self.state.kb.read().all_entries().len());
        counts.insert("thread".into(), self.state.threads.read().all().len());
        counts.insert("note".into(), self.state.notes.read().all().len());
        counts.insert("whiteboard".into(), self.state.whiteboards.list_ids().len());
        counts
    }

    fn ambient_pin_block(
        &self,
        project_dir: Option<&str>,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        thread_id: Option<&str>,
        work_item_id: Option<&str>,
    ) -> Option<String> {
        self.state.pins.read().render_for_ambient(&AmbientPinQuery {
            project: project_dir,
            bro: bro_name,
            session_id,
            thread_id,
            work_item_id,
        })
    }

    /// Dispatch an executor node's turn (new session or resume of an
    /// existing one). Returns the spawned `Task` so the caller can wait
    /// on it. Duplicates the core of `bro_exec` / `bro_resume` minus the
    /// MCP-result formatting — used by the workflow engine.
    pub async fn workflow_dispatch_executor(
        &self,
        brofile: &str,
        prompt: &str,
        project_dir: Option<&str>,
        existing_session_id: Option<&str>,
    ) -> Result<Arc<orch::Task>, String> {
        let store_dir = self.state.store_dir.clone();
        let is_resume = existing_session_id.is_some();

        // Always use exec-target resolution. The workflow engine owns
        // the project_dir; resume just swaps the provider args call.
        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters) =
            self.resolve_exec_target(Some(brofile), None, project_dir)?;

        if is_resume && !provider.supports_resume() {
            return Err(format!("provider {provider} does not support resume"));
        }

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = match existing_session_id {
            Some(s) => s.to_string(),
            None if matches!(provider, Provider::Claude) => uuid::Uuid::new_v4().to_string(),
            None => "pending".to_string(),
        };

        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(brofile.to_string()),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(brofile),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: None,
            allow_recursion: false,
            provider: Some(provider),
        };
        let final_prompt =
            orch::apply_brofile_lens(&orch::apply_ambient(prompt, &ambient_ctx), lens.as_deref());
        let mut args = if is_resume {
            provider.build_resume_args(&session_id, &final_prompt, exec_opts.as_ref())
        } else {
            provider.build_exec_args(
                &final_prompt,
                &session_id,
                cwd.as_deref(),
                exec_opts.as_ref(),
            )
        };

        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            &task_id,
            brofile_filters.as_ref(),
        );
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        self.record_task_to_bro(brofile, &task);
        Ok(task)
    }

    /// Dispatch every member of an ensemble team with the same prompt,
    /// returning one task per member. Each dispatch goes through
    /// `workflow_dispatch_executor`, so durable-session reuse + ambient
    /// context + dispatch filters work uniformly. Unresolved brofiles
    /// are skipped (logged in the returned error string), not fatal.
    pub async fn workflow_dispatch_ensemble(
        &self,
        team_name: &str,
        prompt: &str,
        project_dir: Option<&str>,
        existing_session_ids: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<(String, Arc<orch::Task>)>, String> {
        // Scope the team lock narrowly — we only need it to read the
        // team's current roster. Holding a parking_lot guard across
        // `.await` makes the resulting future `!Send`, which axum
        // handler bounds reject. Snapshot + drop.
        let (members, project_dir_from_team): (Vec<_>, _) = {
            let _lock = orchestration::team::lock_teams();
            let team = orchestration::team::load_team(team_name, &self.state.store_dir)
                .ok_or_else(|| format!("Unknown team: {team_name}"))?;
            let project_dir_from_team = team.project_dir.clone();
            let members = team
                .members
                .iter()
                .map(|m| (m.name.clone(), m.brofile.clone()))
                .collect();
            (members, project_dir_from_team)
        };
        let cwd = project_dir.map(String::from).or(project_dir_from_team);
        let mut launched = Vec::new();
        for (member_name, brofile) in &members {
            let existing = existing_session_ids.get(member_name).cloned();
            let task = self
                .workflow_dispatch_executor(brofile, prompt, cwd.as_deref(), existing.as_deref())
                .await
                .map_err(|e| format!("member {member_name}: {e}"))?;
            // Stamp the precise team::member label, overriding the
            // brofile fallback that workflow_dispatch_executor →
            // record_task_to_bro set. Two team members sharing a
            // brofile (the common keystone-reviewers shape) would
            // otherwise be indistinguishable in `bro tail`.
            task.inner.lock().bro_label = Some(format!("{team_name}::{member_name}"));
            launched.push((member_name.clone(), task));
        }
        Ok(launched)
    }

    /// Apply a workflow-level policy packet to an arc-state entity.
    /// Returns the matching rule's classification (verdict) or `None`.
    pub fn apply_workflow_policy(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading policy packet {packet_id}: {e:#}"))?;
        let prediction = apply_packet_with(&packet, entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Evaluate a workflow gate packet against a node's output in
    /// mode=first semantics. Returns the matching rule's classification
    /// as the verdict, or `None` when no rule fires. Entity shape is
    /// `{output: <output>, node: <node_id>}` — packet predicates can
    /// reference either field.
    pub fn apply_workflow_gate(
        &self,
        packet_id: &str,
        output: &str,
        node_id: &str,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let entity = serde_json::json!({
            "output": output,
            "node": node_id,
        });
        let prediction = apply_packet_with(&packet, &entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Evaluate a workflow gate packet in mode=all — every rule whose
    /// antecedent holds emits a finding, the aggregate verdict is the
    /// highest-priority classification in the packet's lattice among
    /// the findings. Returns the verdict + the findings list so the
    /// engine can surface the multi-finding shape in arc notes.
    pub fn apply_workflow_gate_all(
        &self,
        packet_id: &str,
        output: &str,
        node_id: &str,
    ) -> Result<packets::ApplyAllResult, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let entity = serde_json::json!({
            "output": output,
            "node": node_id,
        });
        Ok(packets::apply_all_with(&packet, &entity, &*packet_store))
    }

    /// Entity-shaped variant of `apply_workflow_gate` — the workflow
    /// engine constructs the full ArcContext flatten (vars + outputs +
    /// meta + last_signal + node_output + node_id) and passes it
    /// directly so packet rules can reference `vars.x`,
    /// `last_signal.name`, etc.
    pub fn apply_workflow_gate_entity(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
        _node_id: &str,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let prediction = apply_packet_with(&packet, entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Entity-shaped `apply_all` variant. Same shape as
    /// `apply_workflow_gate_entity` but mode=all semantics.
    pub fn apply_workflow_gate_all_entity(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
        _node_id: &str,
    ) -> Result<packets::ApplyAllResult, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        Ok(packets::apply_all_with(&packet, entity, &*packet_store))
    }

    /// Server-owned WaitStore for suspendable arcs.
    pub fn wait_store(&self) -> &Arc<crate::workflow::wait::WaitStore> {
        &self.state.wait_store
    }

    /// Register an arc cancel token. Called by the workflow runner at
    /// startup; returned token is stored on the runner and observed
    /// between node iterations and inside Wait suspensions.
    pub fn register_arc_cancel_token(&self, arc_id: &str) -> CancellationToken {
        self.state.register_arc_cancel_token(arc_id)
    }

    /// Drop the arc's cancel token. Called by the runner at terminus.
    pub fn unregister_arc_cancel_token(&self, arc_id: &str) {
        self.state.unregister_arc_cancel_token(arc_id);
    }

    /// Trigger cancellation for a running arc.
    pub fn cancel_arc(&self, arc_id: &str) -> bool {
        self.state.cancel_arc(arc_id)
    }

    fn rebuild_edge_index_from_stores(&self) {
        rebuild_edge_index_from_shared(&self.state);
    }

    /// Resolve a workflow by registry id (set via `bro_workflow_install`
    /// or restored from disk on startup). Returns a clone so the caller
    /// can mutate locally without affecting the registry.
    pub fn resolve_workflow_by_id(&self, id: &str) -> Option<workflow::Workflow> {
        self.state.workflow_registry.read().get(id).cloned()
    }

    /// Soft-nag classifier for `bbox_learn`: apply the latest
    /// `content-classification/arc-bound` packet (if one is compiled) to the
    /// entry's content and return a suggestion string when it classifies
    /// arc-bound. System-generated entries (ids prefixed `bb-`, e.g. the
    /// regenerated tool reference) are exempt — their content legitimately
    /// discusses arc-bound patterns in documentation examples. Silent on any
    /// error; this is steering, not enforcement.
    fn arc_bound_warning(&self, id: Option<&str>, content: &str) -> Option<String> {
        if id.is_some_and(|s| s.starts_with("bb-")) {
            return None;
        }
        let packet_store = self.state.packets.read();
        let packets = packet_store.list_all().ok()?;
        let packet = packets
            .into_iter()
            .find(|pk| pk.domain == "content-classification/arc-bound")?;
        let entity = serde_json::json!({ "content": content });
        let prediction = apply_packet_with(&packet, &entity, &*packet_store)?;
        if prediction.classification == "arc_bound" {
            Some(format!(
                "\n\nNote: this content was classified arc-bound by packet {pkt} (rule: {rule}). Active-arc guidance that will not still be correct a year from now usually belongs in `bbox_pin` (scope=work_item/thread/bro/session) rather than `bbox_learn`, where it renders into every unrelated future session's CLAUDE.md. The entry was saved; review and consider pinning instead.",
                pkt = packet.id,
                rule = prediction.rule_id
            ))
        } else {
            None
        }
    }

    fn ok_text(text: &str) -> CallToolResult {
        CallToolResult::success(Self::cap_response_text(text).into_contents())
    }

    fn ok_json(value: &Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        CallToolResult::success(Self::cap_response_text(&text).into_contents())
    }

    fn err_text(msg: &str) -> CallToolResult {
        let mut r = CallToolResult::success(Self::cap_response_text(msg).into_contents());
        r.is_error = Some(true);
        r
    }

    fn cap_response_text(text: &str) -> String {
        if text.len() <= Self::MCP_RESPONSE_CAP_BYTES {
            return text.to_string();
        }
        let suffix = "\n\n[... response truncated to 80KB by bbox response cap]";
        let target = Self::MCP_RESPONSE_CAP_BYTES.saturating_sub(suffix.len());
        let mut out = String::new();
        for ch in text.chars() {
            if out.len() + ch.len_utf8() > target {
                break;
            }
            out.push(ch);
        }
        out.push_str(suffix);
        out
    }

    /// Run a sync tool handler: time it, log at debug (ok) / warn (err),
    /// uniformly convert Result<String> into CallToolResult. Centralizes
    /// the match-ok-err boilerplate that used to repeat in every bbox_*
    /// handler and gives us per-call duration visibility in journald
    /// (filter: `journalctl --user -u blackbox | grep bbox_`).
    fn run<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<String>,
    {
        let start = std::time::Instant::now();
        match op() {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bbox tools (search, knowledge, threads)
// ---------------------------------------------------------------------------

use artifacts::{ArtifactInstallParams, ArtifactListParams, ArtifactSupersedeParams};
use embed::ReembedParams;
use inbox::InboxParams;
use index::{
    CiteParams, ContextParams, MessagesParams, ReindexParams, SearchParams, SessionParams,
    SessionsListParams, TopicsParams,
};
use knowledge::{
    AbsorbParams, BootstrapParams, DecideParams, ForgetParams, KnowledgeLinkParams,
    KnowledgeListParams, LearnParams, RememberParams, RenderParams, ResponseFormat, ReviewParams,
};
use mcp_tools::blame::BlameParams;
use mcp_tools::bundle_evidence::BundleEvidenceParams;
use mcp_tools::discover_seed::DiscoverSeedParams;
use mcp_tools::find_paths::FindPathsParams;
use mcp_tools::hybrid_search::HybridSearchParams;
use mcp_tools::inspect::InspectEntityParams;
use mcp_tools::provenance::ProvenanceParams;
use notes::{NoteListParams, NoteParams, NoteResolveParams};
use packets::{
    apply_with as apply_packet_with, packet_matches_query, packet_summary,
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams,
};
use threads::{ThreadListParams, ThreadParams};

#[tool_router(router = bbox_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_search",
        description = "Search across all indexed transcripts. Default `mode=smart` broadens adjacent terms for recall; `mode=fulltext` gives raw Tantivy/Lucene-style boolean syntax."
    )]
    fn bbox_search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        Self::run("bbox_search", || {
            let mut idx = self.state.idx.write();
            if idx.is_empty() {
                idx.build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            drop(idx);
            self.state.idx.read().search(&p)
        })
    }

    #[tool(
        name = "bbox_hybrid_search",
        description = "Hybrid BM25+vector search over typed entities. vector_weight=0.6 by default; set 0.0 for BM25-only behavior, 1.0 for vector-only."
    )]
    fn bbox_hybrid_search(&self, Parameters(p): Parameters<HybridSearchParams>) -> CallToolResult {
        Self::run("bbox_hybrid_search", || {
            let mut idx = self.state.idx.write();
            if idx.is_empty() {
                idx.build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            drop(idx);
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::hybrid_search::hybrid_search(
                &self.state.idx.read(),
                &self.state.kb.read(),
                &provider_ctx,
                &p,
            )
        })
    }

    #[tool(
        name = "bbox_discover_seed_entities",
        description = "Find seed entities with notable_edges; inspect before answering."
    )]
    fn bbox_discover_seed_entities(
        &self,
        Parameters(p): Parameters<DiscoverSeedParams>,
    ) -> CallToolResult {
        Self::run("bbox_discover_seed_entities", || {
            let mut idx = self.state.idx.write();
            if idx.is_empty() {
                idx.build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            drop(idx);
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::discover_seed::discover_seed_entities(
                &self.state.idx.read(),
                &self.state.kb.read(),
                &provider_ctx,
                &self.state.edge_index.read(),
                &p,
            )
        })
    }

    #[tool(
        name = "bbox_cite",
        description = "Trace a claim back to the turn that established it."
    )]
    fn bbox_cite(&self, Parameters(p): Parameters<CiteParams>) -> CallToolResult {
        Self::run("bbox_cite", || self.state.idx.read().cite(&p))
    }

    #[tool(
        name = "bbox_context",
        description = "Conversation context around a specific byte offset."
    )]
    fn bbox_context(&self, Parameters(p): Parameters<ContextParams>) -> CallToolResult {
        Self::run("bbox_context", || self.state.idx.read().context(&p))
    }

    #[tool(
        name = "bbox_session",
        description = "Summary metadata for a single session."
    )]
    fn bbox_session(&self, Parameters(p): Parameters<SessionParams>) -> CallToolResult {
        Self::run("bbox_session", || self.state.idx.read().session(&p))
    }

    #[tool(
        name = "bbox_messages",
        description = "Chronological messages from a session."
    )]
    fn bbox_messages(&self, Parameters(p): Parameters<MessagesParams>) -> CallToolResult {
        Self::run("bbox_messages", || self.state.idx.read().messages(&p))
    }

    #[tool(
        name = "bbox_reindex",
        description = "Build or incrementally update the search index."
    )]
    fn bbox_reindex(&self, Parameters(p): Parameters<ReindexParams>) -> CallToolResult {
        Self::run("bbox_reindex", || self.state.idx.write().reindex(&p))
    }

    #[tool(
        name = "bbox_reembed",
        description = "Request an embedding rebuild for a configured route."
    )]
    fn bbox_reembed(&self, Parameters(p): Parameters<ReembedParams>) -> CallToolResult {
        let state = self.state.clone();
        Self::run("bbox_reembed", || embed::reembed_start(&p, state))
    }

    #[tool(
        name = "bbox_embed_status",
        description = "Return per-route embedding queue health."
    )]
    fn bbox_embed_status(&self) -> CallToolResult {
        Self::run("bbox_embed_status", embed_queue::status_json)
    }

    #[tool(
        name = "bbox_topics",
        description = "Top terms in a session by frequency."
    )]
    fn bbox_topics(&self, Parameters(p): Parameters<TopicsParams>) -> CallToolResult {
        Self::run("bbox_topics", || self.state.idx.read().topics(&p))
    }

    #[tool(
        name = "bbox_sessions_list",
        description = "Browse sessions sorted by recency."
    )]
    fn bbox_sessions_list(&self, Parameters(p): Parameters<SessionsListParams>) -> CallToolResult {
        Self::run("bbox_sessions_list", || {
            self.state.idx.read().sessions_list(&p)
        })
    }

    #[tool(
        name = "bbox_stats",
        description = "Corpus statistics (doc count, index size, file counts)."
    )]
    fn bbox_stats(&self) -> CallToolResult {
        Self::run("bbox_stats", || self.state.idx.read().stats())
    }

    #[tool(
        name = "bbox_inspect_entity",
        description = "Inspect a vertex: returns properties AND targeted edges in one call. Prefer targeted inspection over broad exploration: 1) Set edge_types to the specific edges you want (e.g. 'SUPERSEDES,DERIVED_FROM'). 2) Set direction to 'out' or 'in' when you know which way to traverse. 3) Use 'both' only for initial orientation on an unfamiliar entity. 4) Set per_type_limit=0 for property-only inspection. property_mode controls detail: 'summary' (names/titles only), 'smart' (full text <=300 chars, truncated for longer - default), 'full' (no truncation)."
    )]
    fn bbox_inspect_entity(
        &self,
        Parameters(p): Parameters<InspectEntityParams>,
    ) -> CallToolResult {
        Self::run("bbox_inspect_entity", || {
            let entity_ref = match crate::entity_ref::EntityRef::parse(&p.entity_ref) {
                Ok(entity_ref) => entity_ref,
                Err(err) => {
                    return Ok(mcp_tools::inspect::bad_input(
                        &p.entity_ref,
                        err.to_string(),
                    ));
                }
            };
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::inspect::inspect_entity(
                &p,
                &provider_ctx,
                &entity_ref,
                &self.state.edge_index.read(),
            )
        })
    }

    #[tool(
        name = "bbox_describe_schema",
        description = "Catalog agentic-corpus entity types and edge families. Use before bbox_inspect_entity, bbox_find_paths, or evidence bundling when you need the graph vocabulary, filterable fields, population counts, or traversal tips."
    )]
    fn bbox_describe_schema(&self) -> CallToolResult {
        Self::run("bbox_describe_schema", || {
            mcp_tools::describe_schema::describe_schema(&self.describe_schema_counts())
        })
    }

    #[tool(
        name = "bbox_find_paths",
        description = "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent."
    )]
    fn bbox_find_paths(&self, Parameters(p): Parameters<FindPathsParams>) -> CallToolResult {
        Self::run("bbox_find_paths", || {
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::find_paths::find_paths(
                &p,
                &provider_ctx,
                &self.state.edge_index.read(),
                &mut self.state.path_cache.write(),
            )
        })
    }

    #[tool(
        name = "bbox_bundle_evidence",
        description = "Package selected entity refs and cached path IDs into a structured evidence bundle. Use after bbox_find_paths to close the loop before answering; stale path IDs degrade explicitly under degraded.stale_path_ids instead of failing the whole response."
    )]
    fn bbox_bundle_evidence(
        &self,
        Parameters(p): Parameters<BundleEvidenceParams>,
    ) -> CallToolResult {
        Self::run("bbox_bundle_evidence", || {
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::bundle_evidence::bundle_evidence(
                &p,
                &provider_ctx,
                &self.state.edge_index.read(),
                &mut self.state.path_cache.write(),
            )
        })
    }

    #[tool(
        name = "bbox_blame",
        description = "Walk back from a code line to the conversation that produced it. Two modes: 1. Anchor-matching: the line's git blame commit matches a bbox-tracked tool-call anchor, returning the full session/brofile/arc/trigger chain. 2. Git-only fallback: no bbox anchor matches, returning git blame author info only, marked as non-bbox. Use this when you want to understand WHY a line exists, not just WHO wrote it."
    )]
    fn bbox_blame(&self, Parameters(p): Parameters<BlameParams>) -> CallToolResult {
        Self::run("bbox_blame", || {
            let provider_ctx = ProviderContext::new(&self.state);
            let projects = self.state.projects.read().list();
            mcp_tools::blame::blame(
                &p,
                &provider_ctx,
                &self.state.edge_index.read(),
                &projects,
            )
        })
    }

    #[tool(
        name = "bbox_provenance_export",
        description = "Write bbox provenance git notes for commits with tracked tool-call anchors."
    )]
    fn bbox_provenance_export(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        Self::run("bbox_provenance_export", || {
            let projects = self.state.projects.read().list();
            mcp_tools::provenance::export_provenance(
                &p,
                &self.state.edge_index.read(),
                &projects,
            )
        })
    }

    #[tool(
        name = "bbox_provenance_import",
        description = "Read bbox provenance git notes and replay them into the local EdgeIndex sidecar."
    )]
    fn bbox_provenance_import(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        Self::run("bbox_provenance_import", || {
            let projects = self.state.projects.read().list();
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
            let edges_imported =
                mcp_tools::provenance::import_provenance_to_edges_dir(&p, &projects, &edges_dir)?;
            self.rebuild_edge_index_from_stores();
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "edges_imported": edges_imported,
                "notes_ref": crate::git::notes_ref("provenance"),
            }))?)
        })
    }

    #[tool(
        name = "bbox_project_register",
        description = "Register a project directory for agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. Triggers the project-bootstrap-arc which walks the project, chunks files, writes to the index, and emits structural edges. project_id is derived from the canonicalized realpath and is per-machine; not portable across hosts. repo_id is null for non-git projects; for git projects it derives from the first-commit SHA (with remote-URL fallback for shallow clones), so it survives clones. Use bbox_project_list to inspect registered projects."
    )]
    fn bbox_project_register(
        &self,
        Parameters(p): Parameters<ProjectRegisterParams>,
    ) -> CallToolResult {
        Self::run("bbox_project_register", || {
            let record = self.state.projects.write().register_path(&p.path)?;
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
            let provenance_params = ProvenanceParams {
                project_id: Some(record.project_id.clone()),
            };
            mcp_tools::provenance::import_provenance_to_edges_dir(
                &provenance_params,
                std::slice::from_ref(&record),
                &edges_dir,
            )?;
            trigger_project_bootstrap_arc(self.state.clone(), record.clone());
            self.state
                .idx
                .write()
                .reindex(&ReindexParams { full: Some(false) })?;
            // Rebuild EdgeIndex AFTER reindex so freshly-derived edges from the
            // new project's chunks (IN_FILE, CONTAINS_SYMBOL, NEXT_CHUNK, etc.)
            // are projected into the in-memory index. Doing this before reindex
            // (the prior order) left the new project's edges invisible until
            // the next unrelated rebuild trigger.
            self.rebuild_edge_index_from_stores();
            Ok(serde_json::to_string_pretty(&record)?)
        })
    }

    #[tool(
        name = "bbox_project_list",
        description = "List registered project roots with their project_id, repo_id (null for non-git), canonical_path, registered_at, and is_git_repo flag. Idempotent read; safe to call repeatedly. project_ids are stable across daemon restarts. Use this before bbox_project_register to check whether a path is already registered."
    )]
    fn bbox_project_list(&self) -> CallToolResult {
        Self::ok_json(
            &serde_json::to_value(ProjectListResponse {
                projects: self.state.projects.read().list(),
            })
            .unwrap_or_default(),
        )
    }

    #[tool(
        name = "bbox_artifact_install",
        description = "Install a workflow, packet, or brofile artifact from a local JSON file path or http(s) URL into the versioned artifact catalog."
    )]
    async fn bbox_artifact_install(
        &self,
        Parameters(p): Parameters<ArtifactInstallParams>,
    ) -> CallToolResult {
        match install_artifact_from_params(&self.state, p).await {
            Ok(meta) => Self::ok_json(&serde_json::to_value(meta).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("artifact install failed: {e:#}")),
        }
    }

    #[tool(
        name = "bbox_artifact_list",
        description = "List installed workflow, packet, and brofile artifacts with version, source, active status, and supersession metadata."
    )]
    fn bbox_artifact_list(&self, Parameters(p): Parameters<ArtifactListParams>) -> CallToolResult {
        Self::run("bbox_artifact_list", || {
            let rows = self.state.artifacts.read().list(&p)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({ "artifacts": rows }),
            )?)
        })
    }

    #[tool(
        name = "bbox_artifact_supersede",
        description = "Mark one installed artifact superseded by another artifact of the same kind."
    )]
    fn bbox_artifact_supersede(
        &self,
        Parameters(p): Parameters<ArtifactSupersedeParams>,
    ) -> CallToolResult {
        Self::run("bbox_artifact_supersede", || {
            let kind = p.kind;
            let name = p.name.clone();
            let meta = self
                .state
                .artifacts
                .write()
                .supersede(p.kind, &p.name, &p.superseded_by)?;
            deactivate_artifact(&self.state, kind, &name)?;
            Ok(serde_json::to_string_pretty(&meta)?)
        })
    }

    #[tool(
        name = "bbox_learn",
        description = "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\"). If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism — use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically."
    )]
    fn bbox_learn(&self, Parameters(p): Parameters<LearnParams>) -> CallToolResult {
        let format = match ResponseFormat::parse_optional(p.format.as_deref()) {
            Ok(format) => format,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let start = std::time::Instant::now();
        match (|| {
            let warning = self.arc_bound_warning(p.id.as_deref(), &p.content);
            let result = self.state.kb.write().learn_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            Ok::<_, anyhow::Error>((result, warning))
        })() {
            Ok((result, warning)) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                match format {
                    ResponseFormat::Text => {
                        let text = match warning {
                            Some(w) => format!("{}{}", result.message, w),
                            None => result.message,
                        };
                        tracing::info!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, bytes = text.len(), "ok");
                        Self::ok_text(&text)
                    }
                    ResponseFormat::Json => {
                        let mut payload = serde_json::json!({
                            "id": result.id,
                            "action": result.action,
                            "rendered": result.rendered,
                            "render_pending": result.render_pending,
                            "message": result.message,
                        });
                        if let Some(summary) = result.summary {
                            payload["summary"] = serde_json::json!(summary);
                        }
                        if let Some(w) = warning {
                            payload["warnings"] = serde_json::json!([w.trim().to_string()]);
                        }
                        let bytes = serde_json::to_string(&payload)
                            .map(|s| s.len())
                            .unwrap_or_default();
                        tracing::info!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, bytes, "ok");
                        Self::ok_json(&payload)
                    }
                }
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    fn bbox_remember(&self, Parameters(p): Parameters<RememberParams>) -> CallToolResult {
        Self::run("bbox_remember", || {
            let result = self.state.kb.write().remember_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            Ok(result.message)
        })
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    fn bbox_decide(&self, Parameters(p): Parameters<DecideParams>) -> CallToolResult {
        Self::run("bbox_decide", || {
            let result = self.state.kb.write().decide_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            if let Some(old_id) = result.superseded.as_deref() {
                self.tombstone_knowledge_entry_in_index(old_id)?;
            }
            Ok(result.message)
        })
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces (a) rule-packets matching the query by id / domain / rule ids / classification values, and (b) system memories (code-embedded runbooks) marked `[system]`. Pass `category=\"packet\"` to list every compiled packet regardless of query. For structured packet discovery + filtering, use bbox_packet_list."
    )]
    fn bbox_knowledge(&self, Parameters(p): Parameters<KnowledgeListParams>) -> CallToolResult {
        Self::run("bbox_knowledge", || {
            let mut combined = self.state.kb.write().list(&p)?;

            // Surface matching packets. Uses the same match semantics as
            // bbox_packet_list so the two tools agree on what "matches" means.
            let all_packets = self.state.packets.read().list_all()?;
            let matching_packets: Vec<_> =
                if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                    all_packets
                        .into_iter()
                        .filter(|pkt| packet_matches_query(pkt, q))
                        .collect()
                } else if p.category.as_deref() == Some("packet") {
                    all_packets
                } else {
                    Vec::new()
                };

            if !matching_packets.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Rule-packets ───────────────────────────────\n");
                let limit = p.limit.unwrap_or(50).min(500) as usize;
                for pkt in matching_packets.iter().take(limit) {
                    let histogram: Vec<String> = pkt
                        .rules
                        .iter()
                        .fold(BTreeMap::<String, usize>::new(), |mut acc, r| {
                            *acc.entry(r.classification.clone()).or_insert(0) += 1;
                            acc
                        })
                        .into_iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect();
                    combined.push_str(&format!(
                        "[{}] Packet | domain: {} | scope: {} | {} rules [{}] | created {}\n",
                        pkt.id,
                        pkt.domain,
                        pkt.scope,
                        pkt.rules.len(),
                        histogram.join(", "),
                        pkt.created_at,
                    ));
                }
                combined.push_str(
                    "  (use bbox_packet_list for filter/query/preview; bbox_apply to evaluate)\n",
                );
            }

            // Also surface matching code-embedded memories. See
            // src/system_memory/ — these are static runbooks baked into the
            // binary via include_str!, queryable but never rendered.
            let memories = system_memory::search(p.query.as_deref());
            if !memories.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                for m in memories {
                    combined.push_str(&system_memory::format_for_listing(m));
                    combined.push('\n');
                }
            }
            Ok(combined)
        })
    }

    #[tool(
        name = "bbox_knowledge_link",
        description = "Append a knowledge edge."
    )]
    fn bbox_knowledge_link(
        &self,
        Parameters(p): Parameters<KnowledgeLinkParams>,
    ) -> CallToolResult {
        Self::run("bbox_knowledge_link", || {
            let edge = self.state.kb.write().append_link(&p)?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "linked",
                "source": p.source,
                "target": p.target,
                "kind": edge.kind.edge_kind(),
                "confidence": edge.confidence,
            }))?)
        })
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    fn bbox_forget(&self, Parameters(p): Parameters<ForgetParams>) -> CallToolResult {
        Self::run("bbox_forget", || {
            let message = self.state.kb.write().forget(&p)?;
            self.tombstone_knowledge_entry_in_index(&p.id)?;
            Ok(message)
        })
    }

    #[tool(
        name = "bbox_render",
        description = "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md."
    )]
    fn bbox_render(&self, Parameters(p): Parameters<RenderParams>) -> CallToolResult {
        Self::run("bbox_render", || self.state.kb.read().render(&p))
    }

    #[tool(
        name = "bbox_absorb",
        description = "Compatibility no-op for the old rendered-file import path."
    )]
    fn bbox_absorb(&self, Parameters(p): Parameters<AbsorbParams>) -> CallToolResult {
        Self::run("bbox_absorb", || self.state.kb.write().absorb(&p))
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    fn bbox_lint(&self) -> CallToolResult {
        Self::run("bbox_lint", || self.state.kb.read().lint())
    }

    #[tool(
        name = "bbox_review",
        description = "Approve or reject entries awaiting review."
    )]
    fn bbox_review(&self, Parameters(p): Parameters<ReviewParams>) -> CallToolResult {
        Self::run("bbox_review", || self.state.kb.write().review(&p))
    }

    #[tool(
        name = "bbox_bootstrap",
        description = "Onboard a new repo into the blackbox knowledge system."
    )]
    fn bbox_bootstrap(&self, Parameters(p): Parameters<BootstrapParams>) -> CallToolResult {
        Self::run("bbox_bootstrap", || self.state.kb.read().bootstrap(&p))
    }

    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread."
    )]
    fn bbox_thread(&self, Parameters(p): Parameters<ThreadParams>) -> CallToolResult {
        Self::run("bbox_thread", || self.state.threads.write().thread(&p))
    }

    #[tool(
        name = "bbox_thread_list",
        description = "Scan threads by lifecycle status and idle age."
    )]
    fn bbox_thread_list(&self, Parameters(p): Parameters<ThreadListParams>) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            self.state.threads.read().thread_list(&p)
        })
    }

    #[tool(
        name = "bbox_note",
        description = "Record a structured side-channel note while working."
    )]
    fn bbox_note(&self, Parameters(p): Parameters<NoteParams>) -> CallToolResult {
        Self::run("bbox_note", || self.state.notes.write().create(&p))
    }

    #[tool(
        name = "bbox_notes",
        description = "List / filter notes by kind, project, session, thread, resolution."
    )]
    fn bbox_notes(&self, Parameters(p): Parameters<NoteListParams>) -> CallToolResult {
        Self::run("bbox_notes", || self.state.notes.read().list(&p))
    }

    #[tool(
        name = "bbox_note_resolve",
        description = "Mark a note acknowledged or addressed."
    )]
    fn bbox_note_resolve(&self, Parameters(p): Parameters<NoteResolveParams>) -> CallToolResult {
        Self::run("bbox_note_resolve", || self.state.notes.write().resolve(&p))
    }

    #[tool(
        name = "bbox_pin",
        description = "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope."
    )]
    fn bbox_pin(&self, Parameters(p): Parameters<PinParams>) -> CallToolResult {
        Self::run("bbox_pin", || self.state.pins.write().pin(&p))
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    fn bbox_inbox(&self, Parameters(p): Parameters<InboxParams>) -> CallToolResult {
        Self::run("bbox_inbox", || {
            let kb = self.state.kb.read();
            let threads = self.state.threads.read();
            let notes = self.state.notes.read();
            let task_store = self.state.task_store.read();
            inbox::compute_inbox(&kb, &threads, &notes, &task_store, &self.state.whiteboards, &p)
        })
    }

    // ── Rule-packets (compressive compilation of observations) ────────

    #[tool(
        name = "bbox_compile",
        description = "Compile a rubric / judge / decision-function into a shareable packet. Reach here when you're writing a priority-ordered rubric, ranking proposals against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones. Symptom: you're about to paste the same rubric text into multiple sub-agent prompts — compile once and dispatch the packet_id instead. Rules are first-match-wins over a predicate AST; validate with bbox_audit before trusting. Packets compose via `Apply{packet_id, expect}` — extract `is_breaking` / `privileged_role` / etc. once, reuse across packets. Full workflow: sm-rule-packets via bbox_knowledge."
    )]
    fn bbox_compile(&self, Parameters(p): Parameters<CompileParams>) -> CallToolResult {
        Self::run("bbox_compile", || self.state.packets.read().compile(&p))
    }

    #[tool(
        name = "bbox_apply",
        description = "Evaluate a packet against one entity — deterministic, no LLM. The receive-side of the packet workflow: a sub-agent that received packet_id from its orchestrator calls this to classify without reinterpreting the rubric. mode=\"first\" returns the first matching rule; mode=\"all\" returns every matching rule plus an aggregate verdict (for review / multi-finding shape). Cheap at arbitrary scale."
    )]
    fn bbox_apply(&self, Parameters(p): Parameters<PacketApplyParams>) -> CallToolResult {
        Self::run("bbox_apply", || self.state.packets.read().apply_tool(&p))
    }

    #[tool(
        name = "bbox_audit",
        description = "Run a packet against a {entity, expected}[] dataset; report fidelity + mismatching rule ids. The self-verify step: a packet with fidelity < 1.0 is lying about its training data. ALWAYS call this after bbox_compile against the observations you derived the rules from — catches over-generalization, rule-ordering bugs, and field-name typos."
    )]
    fn bbox_audit(&self, Parameters(p): Parameters<AuditParams>) -> CallToolResult {
        Self::run("bbox_audit", || self.state.packets.read().audit_tool(&p))
    }

    #[tool(
        name = "bbox_packet_list",
        description = "Discover compiled rule-packets before authoring a new one. Filter by `domain` (exact), `scope` (global/project), or `query` (case-insensitive substring across id, domain, rule ids, classification values). Pass `latest_per_domain=true` to collapse multiple revisions of the same domain. Each summary includes a classification histogram and the first few rule ids so you can judge relevance without calling bbox_apply. If a packet already covers your domain, compose it via `Apply{packet_id, expect}` or reuse via `bbox_apply`. See sm-rule-packets via bbox_knowledge."
    )]
    fn bbox_packet_list(&self, Parameters(p): Parameters<PacketListParams>) -> CallToolResult {
        Self::run("bbox_packet_list", || {
            let mut packets = self.state.packets.read().list_all()?;
            if let Some(domain) = &p.domain {
                packets.retain(|pkt| pkt.domain == *domain);
            }
            if let Some(scope) = &p.scope {
                packets.retain(|pkt| &pkt.scope == scope);
            }
            if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                packets.retain(|pkt| packet_matches_query(pkt, q));
            }
            if p.latest_per_domain.unwrap_or(false) {
                // list_all returns newest-first; keep the first occurrence of each domain.
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                packets.retain(|pkt| seen.insert(pkt.domain.clone()));
            }
            let limit = p.limit.unwrap_or(50).min(500);
            packets.truncate(limit);

            let summaries: Vec<_> = packets.iter().map(packet_summary).collect();

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "count": summaries.len(),
                "limit": limit,
                "packets": summaries,
            }))?)
        })
    }

    #[tool(
        name = "bbox_packet_events",
        description = "Query the packet operation log — every compile / apply / audit / gap event the daemon has recorded, plus `repair_candidate` events emitted by the self-heal scanner when enabled. Use to investigate packet behavior over time: low-fidelity audits, high no_match rates, compile failures, authoring gaps, and packets the scanner has flagged for repair. Filter by op (compile / apply / audit / gap / repair_candidate), packet_id, outcome, or since. Returns newest-first up to `limit` (default 50, max 500)."
    )]
    fn bbox_packet_events(&self, Parameters(p): Parameters<EventsParams>) -> CallToolResult {
        Self::run("bbox_packet_events", || {
            let limit = p.limit.unwrap_or(50).min(500);
            let events = self.state.packets.read().list_events(
                p.op.as_deref(),
                p.packet_id.as_deref(),
                p.outcome.as_deref(),
                p.since.as_deref(),
                limit,
            )?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "count": events.len(),
                "limit": limit,
                "events": events,
            }))?)
        })
    }

    #[tool(
        name = "bbox_packet_gap",
        description = "Log a packet-authoring gap: 'I wanted to compile a rule but the AST couldn't express it'. Use when you fall back to prose, ad-hoc code, or a different tool because a primitive you needed isn't available. The `description` names what you wanted; `ast_feature_requested` names the primitive you wished existed (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`). These gaps are the highest-signal input for prioritizing new AST primitives — every gap logged is a vote for what the packet system can't yet say. Query via bbox_packet_events(op='gap')."
    )]
    fn bbox_packet_gap(&self, Parameters(p): Parameters<GapParams>) -> CallToolResult {
        Self::run("bbox_packet_gap", || {
            let ev = self.state.packets.read().log_gap(
                &p.description,
                p.domain.as_deref(),
                p.attempted_sketch.as_deref(),
                p.fallback_used.as_deref(),
                p.ast_feature_requested.as_deref(),
            )?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "logged": true,
                "timestamp": ev.timestamp,
                "note": "Thank you — this gap is now queryable via bbox_packet_events(op='gap')",
            }))?)
        })
    }
}

// ---------------------------------------------------------------------------
// Bro tools (orchestration)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ExecParams {
    /// Task instruction for the agent
    prompt: String,
    /// Named bro instance to target. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    bro: Option<String>,
    /// Raw provider for ad-hoc tasks
    #[serde(default)]
    provider: Option<String>,
    /// Working directory (absolute path)
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow patterns merged on top of global+project+brofile.
    /// Use to tighten or open the tool surface for this one invocation.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    /// Per-dispatch disallow patterns merged on top of global+project+brofile.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ResumeParams {
    /// Follow-up instruction
    prompt: String,
    /// Named bro instance to resume. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    bro: Option<String>,
    /// Session ID from a prior task (requires provider)
    #[serde(default)]
    session_id: Option<String>,
    /// Provider (required with session_id)
    #[serde(default)]
    provider: Option<String>,
    /// Working directory
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays for this resume only.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WaitParams {
    /// Task ID from exec or resume
    task_id: String,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhenParams {
    /// Team name — waits on each member's most recent task
    #[serde(default)]
    team: Option<String>,
    /// Explicit list of task IDs
    #[serde(default)]
    task_ids: Option<Vec<String>>,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BroadcastParams {
    /// Team name
    team: String,
    /// Prompt sent to every member
    prompt: String,
    /// Working directory override
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays applied to every member.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    /// Task ID to check
    task_id: String,
    /// Number of recent events to include (default: 0)
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct DashboardParams {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CouncilListParams {
    /// Filter to councils whose `project` matches this exact path.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CouncilOpenParams {
    /// Council ID (e.g. `council-7f01324e`).
    pub id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CouncilPostsParams {
    pub id: String,
    /// Return only posts with `sequence > since_seq`. Default 0 (all).
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Cap the response (default 100, max 1000).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CancelParams {
    /// Task ID to cancel
    task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct PruneParams {
    /// Status to prune (failed, completed, cancelled). Defaults to
    /// "failed" — the only status that's almost always safe to drop
    /// without further filtering. Running tasks are never pruned.
    #[serde(default)]
    status: Option<String>,
    /// Optional provider filter (claude, codex, copilot, gemini, vibe).
    #[serde(default)]
    provider: Option<String>,
    /// Drop tasks that started more than this many hours ago.
    #[serde(default)]
    older_than_hours: Option<u64>,
    /// Dry-run: report what would be pruned without removing.
    /// Defaults to false — bro_prune is the explicit pruning verb.
    #[serde(default)]
    dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BrofileParams {
    /// Operation: create, list, get, delete, set_account, list_accounts,
    /// set_provider_default, get_provider_default, list_provider_defaults,
    /// clear_provider_default
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    /// Persona-bound allow/disallow patterns embedded in the brofile.
    /// Apply at every dispatch using this brofile, between project
    /// mcp.json and per-dispatch ExecParams overrides.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamParams {
    /// Operation: save_template, list_templates, delete_template, create, list, dissolve, roster
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    members: Option<Vec<TeamMemberSlot>>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    cancel_running: Option<bool>,
    #[serde(default)]
    advisor: Option<AdvisorSpecParams>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamMemberSlot {
    brofile: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct AdvisorSpecParams {
    /// Special brofile designated as the advisor for this team.
    brofile: String,
    /// Optional advisor alias; defaults to the brofile name.
    #[serde(default)]
    alias: Option<String>,
    /// One-sentence or short-paragraph charter for the advisor.
    charter: String,
    /// Optional extra context that should stay hot across advisor rounds.
    #[serde(default)]
    context: Option<String>,
    /// Halt / escalate conditions the advisor should watch for.
    #[serde(default)]
    halt_conditions: Option<Vec<String>>,
    /// Exit conditions the advisor should watch for.
    #[serde(default)]
    exit_conditions: Option<Vec<String>>,
    /// Optional compiled packet ID the advisor can use mechanically.
    #[serde(default)]
    packet_id: Option<String>,
    /// Wait behavior for internal advisor rounds.
    #[serde(default)]
    mode: Option<orchestration::team::AdvisorMode>,
    /// Optional timeout for internal advisor init/resume waits.
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize)]
struct AdvisorMemberCheckpoint {
    bro: Option<String>,
    task_id: String,
    status: String,
    timed_out: bool,
    keep_going: Option<String>,
    result_snippet: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct AdvisorNoteSummary {
    dispute_count: usize,
    assumption_count: usize,
    surprise_count: usize,
    followup_count: usize,
    blocked_count: usize,
    learned_count: usize,
    done_count: usize,
    recent_unresolved: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AdvisorCheckpoint {
    wait_kind: String,
    team_name: String,
    teamplate: String,
    monitored_task_ids: Vec<String>,
    packet_id: Option<String>,
    total_count: usize,
    completed_count: usize,
    failed_count: usize,
    cancelled_count: usize,
    timed_out_count: usize,
    running_count: usize,
    dispute_count: usize,
    assumption_count: usize,
    surprise_count: usize,
    followup_count: usize,
    blocked_count: usize,
    learned_count: usize,
    done_count: usize,
    members: Vec<AdvisorMemberCheckpoint>,
    notes: AdvisorNoteSummary,
}

// ---------------------------------------------------------------------------
// Progress notifications — MCP progressToken plumbing for blocking waits
// ---------------------------------------------------------------------------
//
// Per MCP spec, progress notifications are correlated to a pending request via
// the progressToken the caller put in `_meta`. The server MUST echo that exact
// token back; otherwise clients drop the notification as unknown. Servers MUST
// NOT send progress notifications unless the caller asked for them.

const PROGRESS_TICK_SECS: u64 = 15;

fn format_bro_line(task: &orch::Task, store_dir: &Path) -> (String, bool) {
    let inner = task.inner.lock();
    let terminal = inner.status.is_terminal();
    let bro_name = orchestration::team::find_bro_name_for_task(&inner.id, store_dir);
    let label = bro_name.unwrap_or_else(|| inner.id[..inner.id.len().min(8)].to_string());
    let elapsed = orch::format_elapsed(inner.started_at, inner.completed_at);
    let events = inner.events.len();
    let activity = if terminal {
        format!("{:?}", inner.status)
    } else {
        inner
            .last_assistant_message
            .as_deref()
            .map(|m| {
                let c = m.replace('\n', " ");
                if c.len() > 80 {
                    format!("{}…", &c[..80])
                } else {
                    c
                }
            })
            .unwrap_or_else(|| {
                if events == 0 {
                    "starting…".into()
                } else {
                    "working…".into()
                }
            })
    };
    (
        format!("[{label}] {elapsed} | {events} ev | {activity}"),
        terminal,
    )
}

fn format_progress_snapshot(tasks: &[Arc<orch::Task>], store_dir: &Path) -> (String, bool) {
    let mut all_terminal = true;
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let (line, terminal) = format_bro_line(t, store_dir);
            if !terminal {
                all_terminal = false;
            }
            line
        })
        .collect();
    (lines.join("\n"), all_terminal)
}

/// Load the effective tool filter set for a dispatch (global + project
/// overlay + default recursion guard unless `allow_recursion`), then
/// translate to provider-specific CLI args. For Gemini, also writes a
/// per-dispatch policy file and returns the path so the caller can
/// clean it up after the child exits.
struct DispatchFilters {
    args: Vec<String>,
    /// Tempfile path for Gemini policy cleanup; None for other providers.
    policy_file: Option<PathBuf>,
}

/// Build a per-dispatch McpFilters overlay from a tool's allow/disallow
/// param vectors. Returns None when both are empty so callers can pass
/// None directly into resolve_dispatch_filters without an empty merge.
fn extra_filters_from_params(
    allow: Option<&[String]>,
    disallow: Option<&[String]>,
) -> Option<orchestration::mcp::McpFilters> {
    let allow = allow.unwrap_or(&[]);
    let disallow = disallow.unwrap_or(&[]);
    if allow.is_empty() && disallow.is_empty() {
        return None;
    }
    Some(orchestration::mcp::McpFilters {
        allow: allow
            .iter()
            .map(|p| orchestration::mcp::normalize_filter_pattern(p))
            .collect(),
        disallow: disallow
            .iter()
            .map(|p| orchestration::mcp::normalize_filter_pattern(p))
            .collect(),
    })
}

/// Combine brofile-embedded filters with per-dispatch params overlay.
/// Brofile applies first (persona scope), then per-dispatch (call scope).
/// Returns None when both are empty/absent.
fn combine_dispatch_filters(
    brofile_filters: Option<&orchestration::mcp::McpFilters>,
    params_filters: Option<&orchestration::mcp::McpFilters>,
) -> Option<orchestration::mcp::McpFilters> {
    match (brofile_filters, params_filters) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(p)) => Some(p.clone()),
        (Some(b), Some(p)) => {
            let mut combined = b.clone();
            combined.merge_from(p);
            Some(combined)
        }
    }
}

fn resolve_dispatch_filters(
    provider: Provider,
    project_dir: Option<&str>,
    allow_recursion: bool,
    task_id: &str,
    extra: Option<&orchestration::mcp::McpFilters>,
) -> DispatchFilters {
    let global = orchestration::mcp::global_store_path()
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok())
        .unwrap_or_default();
    let project = project_dir
        .map(|pd| orchestration::mcp::project_store_path(Path::new(pd)))
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok());

    let mut eff = orchestration::mcp::resolve_effective(
        &global,
        project.as_ref(),
        /* include_default_guard */ !allow_recursion,
    );
    // Per-dispatch overlay merges last (after global, project, default
    // guard) so callers can tighten or open the surface for a single
    // invocation. Disallow patterns in `extra` add to the deny set;
    // allow patterns add to the allow set. Recursion guard still wins
    // because allow doesn't override disallow at provider level.
    if let Some(extra) = extra {
        eff.filters.merge_from(extra);
    }

    let mut args = provider.build_filter_args(&eff.filters);
    let mut policy_file = None;

    if provider == Provider::Gemini {
        match orchestration::mcp::write_gemini_policy_file(task_id, &eff.filters) {
            Ok(Some(path)) => {
                args.push("--policy".into());
                args.push(path.to_string_lossy().into_owned());
                policy_file = Some(path);
            }
            Ok(None) => { /* no filters → no file */ }
            Err(e) => tracing::warn!("gemini policy file write failed: {e:#}"),
        }
    }

    DispatchFilters { args, policy_file }
}

/// Delete a Gemini policy tempfile once the associated task reaches a
/// terminal state. Spawned as a detached tokio task from the dispatch
/// path. No-op if path is None.
pub(crate) fn cleanup_policy_file_when_done(
    task: std::sync::Arc<orch::Task>,
    path: Option<PathBuf>,
) {
    let Some(path) = path else { return };
    tokio::spawn(async move {
        loop {
            {
                let inner = task.inner.lock();
                if inner.status.is_terminal() {
                    break;
                }
            }
            tokio::select! {
                _ = task.notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!("gemini policy cleanup {}: {e}", path.display());
        }
    });
}

fn spawn_progress_notifier(
    tasks: Vec<Arc<orch::Task>>,
    peer: rmcp::service::Peer<rmcp::RoleServer>,
    progress_token: rmcp::model::ProgressToken,
    store_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tracing::info!(target: "blackbox::progress", token = ?progress_token, tasks = tasks.len(), "notifier spawned");
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_TICK_SECS)).await;
            tick += 1;

            let (msg, all_terminal) = format_progress_snapshot(&tasks, &store_dir);

            let send_result = peer
                .send_notification(rmcp::model::ServerNotification::ProgressNotification(
                    rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                        progress_token: progress_token.clone(),
                        progress: tick as f64,
                        total: None,
                        message: Some(msg.clone()),
                    }),
                ))
                .await;
            match send_result {
                Ok(()) => {
                    tracing::debug!(target: "blackbox::progress", tick, terminal = all_terminal, msg_len = msg.len(), "tick sent")
                }
                Err(e) => {
                    tracing::warn!(target: "blackbox::progress", tick, error = %e, "tick send failed")
                }
            }

            if all_terminal {
                break;
            }
        }
    })
}

#[tool_router(router = bro_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_exec",
        description = "Launch an agent task. Returns {taskId, sessionId} immediately."
    )]
    async fn bro_exec(&self, Parameters(p): Parameters<ExecParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let store_dir = self.state.store_dir.clone();

        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters) = match self
            .resolve_exec_target(
                p.bro.as_deref(),
                p.provider.as_deref(),
                p.project_dir.as_deref(),
            ) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&e),
        };

        // Pre-generate task_id so it lands in the ambient [scope] block
        // before subprocess launch — the primary correlation key for
        // bbox_note emissions regardless of when the provider itself
        // emits a session ID.
        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if matches!(provider, Provider::Claude) {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                p.bro.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
        };
        let final_prompt = orch::apply_brofile_lens(
            &orch::apply_ambient(&p.prompt, &ambient_ctx),
            lens.as_deref(),
        );
        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
        );
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );

        // Register Gemini policy-file cleanup once the task terminates.
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        // If targeting a named bro in a team, record the task
        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_resume",
        description = "Continue an existing session with a follow-up."
    )]
    async fn bro_resume(&self, Parameters(p): Parameters<ResumeParams>) -> CallToolResult {
        let store_dir = self.state.store_dir.clone();

        let (provider, session_id, _lens, exec_opts, env_overrides, cwd, brofile_filters) =
            match self.resolve_resume_target(
                p.bro.as_deref(),
                p.session_id.as_deref(),
                p.provider.as_deref(),
                p.project_dir.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => return Self::err_text(&e),
            };

        if !provider.supports_resume() {
            return Self::err_text(&format!("{provider} does not support resume"));
        }

        // Auto-resolve cwd from the session's own recorded origin so
        // agents can resurrect each other across repo boundaries without
        // the caller threading project_dir. Gemini gets a hard refuse on
        // miss because its CLI silently forks a fresh session when the
        // UUID isn't in the cwd's project hash folder (aliasing the
        // resumed session). Claude/Codex error loudly on miss — fall
        // through to the caller's cwd and let them surface the failure.
        let cwd = match provider.resolve_session_cwd(&session_id) {
            Some(p) => Some(p.to_string_lossy().into_owned()),
            None if provider == Provider::Gemini => {
                return Self::err_text(&format!(
                    "Gemini session {session_id} not found in ~/.gemini/tmp/*/chats. Refusing to resume because Gemini silently forks a new session when the UUID isn't in the cwd's project folder (aliasing the resumed session). Verify the session ID or re-dispatch.",
                ));
            }
            None => cwd,
        };

        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let task_id = uuid::Uuid::new_v4().to_string();

        // Re-apply ambient on resume: each resume is its own dispatch with a
        // fresh task_id, and the per-turn recall directive + completion
        // contract need to ride with every follow-up (memory-file
        // reinforcement decays at depth). The brofile lens was injected on
        // exec and lives in the transcript — not re-prepended here.
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                p.bro.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
        };
        let wrapped_prompt = orch::apply_ambient(&p.prompt, &ambient_ctx);

        let mut args = provider.build_resume_args(&session_id, &wrapped_prompt, exec_opts.as_ref());
        // Filters (mechanical recursion guard + user-configured allow/
        // disallow) must ride with every dispatch — exec AND resume.
        // Without this, a resumed session re-acquires the orchestration
        // tool surface the recursion guard was meant to deny.
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
        );
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_wait",
        description = "Block until a single task completes."
    )]
    async fn bro_wait(
        &self,
        Parameters(p): Parameters<WaitParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };

        let caller_token = context.meta.get_progress_token();
        tracing::info!(target: "blackbox::progress", tool = "bro_wait", has_token = caller_token.is_some(), token = ?caller_token, "entry");
        let progress_handle = caller_token.map(|token| {
            spawn_progress_notifier(
                vec![task.clone()],
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        let result = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        let mut out = result;
        if let Some(team_ref) =
            orchestration::team::find_bro_ref_for_task(&p.task_id, &self.state.store_dir)
        {
            out["bro"] = Value::String(team_ref.member_name.clone());
            match self
                .maybe_resume_team_advisor(&team_ref.team_name, "wait", &[out.clone()])
                .await
            {
                Ok(Some(value)) => out["advisor"] = value,
                Ok(None) => {}
                Err(err) => out["advisor"] = json!({"error": err}),
            }
        }
        Self::ok_json(&out)
    }

    #[tool(
        name = "bro_when_all",
        description = "Block until ALL tasks / team members complete."
    )]
    async fn bro_when_all(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        let progress_handle = context.meta.get_progress_token().map(|token| {
            spawn_progress_notifier(
                tasks.clone(),
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        // Wait concurrently (like Promise.all), not sequentially
        let timeout = p.timeout_seconds;
        let store_dir = self.state.store_dir.clone();
        let futs: Vec<_> = tasks
            .iter()
            .map(|task| {
                let task = task.clone();
                let sd = store_dir.clone();
                async move {
                    let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
                    let bro_name = {
                        let inner = task.inner.lock();
                        orchestration::team::find_bro_name_for_task(&inner.id, &sd)
                    };
                    let mut r = if completed {
                        orch::task_result_json(&task)
                    } else {
                        orch::timeout_snapshot_json(&task)
                    };
                    if let Some(name) = bro_name {
                        r["bro"] = Value::String(name);
                    }
                    r
                }
            })
            .collect();

        let results: Vec<Value> = futures::future::join_all(futs).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        let all_done = results.iter().all(|r| r.get("timed_out").is_none());
        let advisor = match p.team.as_deref() {
            Some(team_name) => {
                self.maybe_resume_team_advisor(team_name, "when_all", &results)
                    .await
            }
            None => Ok(None),
        };
        let mut out = json!({ "all_completed": all_done, "results": results });
        match advisor {
            Ok(Some(value)) => out["advisor"] = value,
            Ok(None) => {}
            Err(err) => out["advisor"] = json!({"error": err}),
        }
        Self::ok_json(&out)
    }

    #[tool(
        name = "bro_when_any",
        description = "Block until the FIRST task completes."
    )]
    async fn bro_when_any(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        // Check if any already done
        let any_done = tasks.iter().any(|t| t.inner.lock().status.is_terminal());
        let progress_handle = if !any_done && !tasks.is_empty() {
            context.meta.get_progress_token().map(|token| {
                spawn_progress_notifier(
                    tasks.clone(),
                    context.peer.clone(),
                    token,
                    self.state.store_dir.clone(),
                )
            })
        } else {
            None
        };

        if !any_done && !tasks.is_empty() {
            // Race them
            let futs: Vec<_> = tasks
                .iter()
                .map(|t| {
                    let t = t.clone();
                    Box::pin(async move {
                        orch::wait_for_task(&t).await;
                    })
                })
                .collect();

            match p.timeout_seconds {
                Some(secs) => {
                    let dur = std::time::Duration::from_secs_f64(secs);
                    let _ = tokio::time::timeout(dur, futures::future::select_all(futs)).await;
                }
                None => {
                    futures::future::select_all(futs).await;
                }
            }
        }
        if let Some(h) = progress_handle {
            h.abort();
        }

        let mut results = Vec::new();
        for task in &tasks {
            let inner = task.inner.lock();
            let bro_name =
                orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
            drop(inner);

            let mut r = if task.inner.lock().status.is_terminal() {
                orch::task_result_json(task)
            } else {
                orch::timeout_snapshot_json(task)
            };
            if let Some(name) = bro_name {
                r["bro"] = Value::String(name);
            }
            results.push(r);
        }

        let any_completed = results.iter().any(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "any_completed": any_completed, "results": results }))
    }

    #[tool(
        name = "bro_broadcast",
        description = "Send the same prompt to every team member."
    )]
    async fn bro_broadcast(&self, Parameters(p): Parameters<BroadcastParams>) -> CallToolResult {
        let _team_lock = orchestration::team::lock_teams();
        let team = match orchestration::team::load_team(&p.team, &self.state.store_dir) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown team: {}", p.team)),
        };
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let cwd = p.project_dir.or(team.project_dir.clone());
        let store_dir = self.state.store_dir.clone();
        let mut launched = Vec::new();
        let mut updated_team = team.clone();
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());

        for (i, member) in team.members.iter().enumerate() {
            let brofile = match orchestration::brofile::resolve_brofile(
                &member.brofile,
                &store_dir,
                team.project_dir.as_deref(),
            ) {
                Some(bf) => bf,
                None => {
                    launched.push(json!({"bro": member.name, "error": format!("Brofile not found: {}", member.brofile)}));
                    continue;
                }
            };

            let env_overrides = orchestration::brofile::resolve_provider_env(
                brofile.provider,
                brofile.account.as_deref(),
                brofile.model.as_deref(),
                &store_dir,
            );
            let exec_opts = if brofile.model.is_some() || brofile.effort.is_some() {
                Some(ExecOpts {
                    model: brofile.model.clone(),
                    effort: brofile.effort.clone(),
                })
            } else {
                None
            };
            // Per-member combined extra: brofile.filters + broadcast-level
            // params overlay. Recursion guard is added inside
            // resolve_dispatch_filters; both layers above merge on top.
            let extra = combine_dispatch_filters(brofile.filters.as_ref(), params_extra.as_ref());

            // Build first-turn prompt with ambient scope + brofile lens.
            // Only applies on fresh-session exec paths; resumes use the
            // raw prompt so ambient/lens aren't re-injected each turn.
            let build_exec_prompt = |task_id: &str, session_id: &str| -> String {
                let ctx = orch::AmbientContext {
                    task_id: Some(task_id.to_string()),
                    session_id: Some(session_id.to_string()),
                    project_dir: cwd.clone(),
                    bro_name: Some(member.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(member.name.as_str()),
                        Some(session_id),
                        None,
                        None,
                    ),
                    completion_contract: if allow_recursion {
                        None
                    } else {
                        Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
                    },
                    allow_recursion,
                    provider: Some(brofile.provider),
                };
                orch::apply_brofile_lens(
                    &orch::apply_ambient(&p.prompt, &ctx),
                    brofile.lens.as_deref(),
                )
            };

            let task = if let Some(ref sid) = member.session_id {
                if sid != "pending" {
                    // Auto-resolve cwd from the session's origin so a
                    // broadcast can resurrect members even when the
                    // current team.project_dir differs from where each
                    // member's session was recorded. Gemini refuses on
                    // miss (silent-fork aliasing); claude/codex fall
                    // through and error loudly themselves.
                    let member_cwd = match brofile.provider.resolve_session_cwd(sid) {
                        Some(p) => Some(p.to_string_lossy().into_owned()),
                        None if brofile.provider == Provider::Gemini => {
                            launched.push(json!({
                                "bro": member.name,
                                "error": format!("Gemini session {sid} not found in ~/.gemini/tmp/*/chats — refusing to resume (silent-fork aliasing)"),
                            }));
                            continue;
                        }
                        None => cwd.clone(),
                    };
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let mut args =
                        brofile
                            .provider
                            .build_resume_args(sid, &p.prompt, exec_opts.as_ref());
                    let df = resolve_dispatch_filters(
                        brofile.provider,
                        member_cwd.as_deref(),
                        allow_recursion,
                        &task_id,
                        extra.as_ref(),
                    );
                    args.extend(df.args);
                    let t = orch::spawn_task(
                        task_id,
                        brofile.provider,
                        args,
                        sid.clone(),
                        member_cwd,
                        env_overrides,
                        store_dir.clone(),
                        self.state.task_store.clone(),
                        self.state.tail_tx.clone(),
                    );
                    cleanup_policy_file_when_done(t.clone(), df.policy_file);
                    t
                } else {
                    launched.push(json!({
                        "bro": member.name,
                        "error": "Session discovery still pending from the previous launch; refusing to fork a second session",
                    }));
                    continue;
                }
            } else {
                let task_id = uuid::Uuid::new_v4().to_string();
                let session_id = if matches!(brofile.provider, Provider::Claude) {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    "pending".into()
                };
                let exec_prompt = build_exec_prompt(&task_id, &session_id);
                let mut args = brofile.provider.build_exec_args(
                    &exec_prompt,
                    &session_id,
                    cwd.as_deref(),
                    exec_opts.as_ref(),
                );
                let df = resolve_dispatch_filters(
                    brofile.provider,
                    cwd.as_deref(),
                    allow_recursion,
                    &task_id,
                    extra.as_ref(),
                );
                args.extend(df.args);
                let t = orch::spawn_task(
                    task_id,
                    brofile.provider,
                    args,
                    session_id,
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                );
                cleanup_policy_file_when_done(t.clone(), df.policy_file);
                updated_team.members[i].session_id = Some(t.inner.lock().session_id.clone());
                t
            };

            let tid = task.id();
            updated_team.members[i].task_history.push(tid.clone());
            let sid = task.inner.lock().session_id.clone();
            launched.push(json!({"bro": member.name, "taskId": tid, "sessionId": sid}));
        }

        orchestration::team::save_team(&updated_team, &store_dir);
        Self::ok_json(&json!({"team": p.team, "tasks": launched}))
    }

    #[tool(
        name = "bro_status",
        description = "Non-blocking progress check on a task."
    )]
    fn bro_status(&self, Parameters(p): Parameters<StatusParams>) -> CallToolResult {
        match self.state.task_store.read().get(&p.task_id) {
            Some(task) => Self::ok_json(&orch::task_status_json(&task, p.tail.unwrap_or(0))),
            None => Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        }
    }

    #[tool(name = "bro_dashboard", description = "List recent tasks / sessions.")]
    fn bro_dashboard(&self, Parameters(p): Parameters<DashboardParams>) -> CallToolResult {
        let store = self.state.task_store.read();
        let limit = p.limit.unwrap_or(20);

        let filter_provider = p
            .provider
            .as_deref()
            .and_then(|s| s.parse::<Provider>().ok());
        let filter_status: Option<orch::TaskStatus> = p
            .status
            .as_deref()
            .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok());

        let team_task_ids: Option<std::collections::HashSet<String>> =
            p.team.as_ref().and_then(|name| {
                let team = orchestration::team::load_team(name, &self.state.store_dir)?;
                Some(
                    team.members
                        .iter()
                        .flat_map(|m| m.task_history.clone())
                        .collect(),
                )
            });

        let mut with_ts: Vec<(u64, Value)> = store
            .all_tasks()
            .iter()
            .filter(|t| {
                let inner = t.inner.lock();
                if let Some(fp) = filter_provider {
                    if inner.provider != fp {
                        return false;
                    }
                }
                if let Some(fs) = filter_status {
                    if inner.status != fs {
                        return false;
                    }
                }
                if let Some(ref ids) = team_task_ids {
                    if !ids.contains(&inner.id) {
                        return false;
                    }
                }
                true
            })
            .map(|t| {
                let inner = t.inner.lock();
                let bro_name =
                    orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
                let mut entry = json!({
                    "taskId": inner.id,
                    "provider": inner.provider,
                    "sessionId": inner.session_id,
                    "status": inner.status,
                    "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                    "hasResult": inner.last_assistant_message.is_some(),
                });
                if let Some(name) = bro_name {
                    entry["bro"] = Value::String(name);
                }
                (inner.started_at, entry)
            })
            .collect();
        with_ts.sort_by(|a, b| b.0.cmp(&a.0));
        let entries: Vec<Value> = with_ts.into_iter().take(limit).map(|(_, e)| e).collect();

        Self::ok_json(&json!({"count": entries.len(), "tasks": entries}))
    }

    #[tool(
        name = "bro_prune",
        description = "Drop terminal tasks from the store + persisted tasks.json."
    )]
    fn bro_prune(&self, Parameters(p): Parameters<PruneParams>) -> CallToolResult {
        let target_status = p.status.as_deref().unwrap_or("failed");
        let allowed = ["failed", "completed", "cancelled"];
        if !allowed.contains(&target_status) {
            return Self::err_text(&format!(
                "status must be one of {:?} (got {:?}); running tasks are never pruned",
                allowed, target_status,
            ));
        }
        let parsed_status: orch::TaskStatus =
            match serde_json::from_str(&format!("\"{target_status}\"")) {
                Ok(s) => s,
                Err(e) => return Self::err_text(&format!("status parse: {e}")),
            };
        let filter_provider = p
            .provider
            .as_deref()
            .and_then(|s| s.parse::<Provider>().ok());
        let cutoff_ms = p
            .older_than_hours
            .map(|h| orch::now_ms().saturating_sub(h.saturating_mul(3600 * 1000)));
        let dry_run = p.dry_run.unwrap_or(false);

        let dropped: Vec<String> = if dry_run {
            self.state
                .task_store
                .read()
                .all_tasks()
                .iter()
                .filter_map(|t| {
                    let inner = t.inner.lock();
                    if inner.status != parsed_status {
                        return None;
                    }
                    if let Some(fp) = filter_provider {
                        if inner.provider != fp {
                            return None;
                        }
                    }
                    if let Some(cutoff) = cutoff_ms {
                        if inner.started_at >= cutoff {
                            return None;
                        }
                    }
                    Some(inner.id.clone())
                })
                .collect()
        } else {
            let mut store = self.state.task_store.write();
            let dropped = store.retain_drop(|t| {
                let inner = t.inner.lock();
                // Keep running tasks always.
                if inner.status == orch::TaskStatus::Running {
                    return true;
                }
                // Keep tasks that don't match the filter.
                if inner.status != parsed_status {
                    return true;
                }
                if let Some(fp) = filter_provider {
                    if inner.provider != fp {
                        return true;
                    }
                }
                if let Some(cutoff) = cutoff_ms {
                    if inner.started_at >= cutoff {
                        return true;
                    }
                }
                false
            });
            store.persist(&self.state.store_dir);
            dropped
        };

        Self::ok_json(&json!({
            "dryRun": dry_run,
            "status": target_status,
            "pruned": dropped.len(),
            "taskIds": dropped,
        }))
    }

    #[tool(name = "bro_cancel", description = "Cancel a running task (SIGTERM).")]
    fn bro_cancel(&self, Parameters(p): Parameters<CancelParams>) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };
        match orch::cancel_task(&task, &self.state.task_store, &self.state.store_dir) {
            Ok(()) => {
                let inner = task.inner.lock();
                let _ = self.state.tail_tx.send(TailEvent::TaskCancelled {
                    task_id: inner.id.clone(),
                    elapsed: orch::format_elapsed(inner.started_at, inner.completed_at),
                });
                Self::ok_json(&json!({
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "cancelled",
                }))
            }
            Err(e) => Self::err_text(&e),
        }
    }

    #[tool(
        name = "bro_providers",
        description = "List configured providers, binaries, models."
    )]
    fn bro_providers(&self) -> CallToolResult {
        let mut info = serde_json::Map::new();
        for p in Provider::ALL {
            let bin = p.bin();
            let resolved = orch::providers::resolve_bin(&bin);
            let mut entry = json!({
                "bin": bin,
                "found": resolved.is_some(),
                "supportsResume": p.supports_resume(),
            });
            if let Some(ref path) = resolved {
                entry["path"] = json!(path);
            }
            if !p.models().is_empty() {
                entry["models"] = serde_json::to_value(p.models()).unwrap_or_default();
            }
            if !p.efforts().is_empty() {
                entry["efforts"] = serde_json::to_value(p.efforts()).unwrap_or_default();
            }
            info.insert(p.as_str().to_string(), entry);
        }
        Self::ok_json(&Value::Object(info))
    }

    #[tool(
        name = "bro_brofile",
        description = "Manage brofile templates + accounts (provider+account+lens)."
    )]
    fn bro_brofile(&self, Parameters(p): Parameters<BrofileParams>) -> CallToolResult {
        use orchestration::brofile;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "create" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let filters = extra_filters_from_params(
                    p.allow_tools.as_deref(),
                    p.disallow_tools.as_deref(),
                );
                let bf = brofile::Brofile {
                    name: name.clone(),
                    provider,
                    account: p.account.clone(),
                    lens: p.lens.clone(),
                    model: p.model.clone(),
                    effort: p.effort.clone(),
                    filters,
                };
                brofile::save_brofile(&bf, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"created": name, "scope": scope, "brofile": bf}))
            }
            "list" => {
                let list = brofile::list_brofiles(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "get" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::resolve_brofile(name, store_dir, p.project_dir.as_deref()) {
                    Some(bf) => Self::ok_json(&serde_json::to_value(&bf).unwrap_or_default()),
                    None => Self::err_text(&format!("Brofile not found: {name}")),
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if brofile::delete_brofile(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Brofile not found: {name}"))
                }
            }
            "set_account" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let mut config = brofile::load_config(store_dir);
                config
                    .accounts
                    .insert(name.clone(), brofile::Account { env: p.env.clone() });
                brofile::save_config(&config, store_dir);
                Self::ok_json(&json!({"account": name, "env": p.env}))
            }
            "list_accounts" => {
                let config = brofile::load_config(store_dir);
                Self::ok_json(&serde_json::to_value(&config.accounts).unwrap_or_default())
            }
            "set_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let account = match &p.account {
                    Some(a) if !a.trim().is_empty() => a.trim().to_string(),
                    _ => return Self::err_text("account is required"),
                };
                let mut config = brofile::load_config(store_dir);
                config.provider_defaults.insert(
                    provider,
                    brofile::ProviderDefault {
                        account: account.clone(),
                    },
                );
                brofile::save_config(&config, store_dir);
                Self::ok_json(
                    &json!({"provider": provider.as_str(), "account": account, "updated": true}),
                )
            }
            "get_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let account = brofile::provider_default_account(provider, store_dir);
                Self::ok_json(&json!({"provider": provider.as_str(), "account": account}))
            }
            "list_provider_defaults" => {
                let config = brofile::load_config(store_dir);
                let defaults: std::collections::HashMap<String, String> = config
                    .provider_defaults
                    .into_iter()
                    .map(|(provider, entry)| (provider.to_string(), entry.account))
                    .collect();
                Self::ok_json(&serde_json::to_value(defaults).unwrap_or_default())
            }
            "clear_provider_default" => {
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let mut config = brofile::load_config(store_dir);
                let removed = config.provider_defaults.remove(&provider).is_some();
                brofile::save_config(&config, store_dir);
                Self::ok_json(&json!({"provider": provider.as_str(), "removed": removed}))
            }
            _ => Self::err_text(&format!("Unknown brofile action: {}", p.action)),
        }
    }

    #[tool(
        name = "bro_mcp",
        description = "Manage MCP servers + tool filters for dispatched bros."
    )]
    fn bro_mcp(
        &self,
        Parameters(p): Parameters<orchestration::mcp::McpToolParams>,
    ) -> CallToolResult {
        Self::run("bro_mcp", || orchestration::mcp::handle(&p))
    }

    #[tool(
        name = "bro_team",
        description = "Manage teamplates and instantiated teams."
    )]
    async fn bro_team(&self, Parameters(p): Parameters<TeamParams>) -> CallToolResult {
        use orchestration::team;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "save_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let members = match &p.members {
                    Some(m) if !m.is_empty() => m,
                    _ => return Self::err_text("members is required"),
                };
                // Validate brofile names
                for m in members {
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        p.project_dir.as_deref(),
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let advisor = match self.resolve_team_advisor_config(
                    p.advisor.as_ref(),
                    store_dir,
                    p.project_dir.as_deref(),
                ) {
                    Ok(cfg) => cfg,
                    Err(e) => return Self::err_text(&e),
                };
                let tp = team::Teamplate {
                    name: name.clone(),
                    members: members
                        .iter()
                        .map(|m| team::TeamplateMember {
                            brofile: m.brofile.clone(),
                            alias: m.alias.clone(),
                            count: m.count.unwrap_or(1),
                        })
                        .collect(),
                    advisor,
                };
                team::save_teamplate(&tp, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"saved": name, "scope": scope}))
            }
            "list_templates" => {
                let list = team::list_teamplates(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "delete_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if team::delete_teamplate(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Teamplate not found: {name}"))
                }
            }
            "create" => {
                let template = match &p.template {
                    Some(t) => t,
                    None => return Self::err_text("template is required"),
                };
                let tp =
                    match team::resolve_teamplate(template, store_dir, p.project_dir.as_deref()) {
                        Some(tp) => tp,
                        None => return Self::err_text(&format!("Teamplate not found: {template}")),
                    };
                // Validate all brofiles exist before instantiating
                for m in &tp.members {
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        p.project_dir.as_deref(),
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let advisor_override = match self.resolve_team_advisor_config(
                    p.advisor.as_ref(),
                    store_dir,
                    p.project_dir.as_deref(),
                ) {
                    Ok(cfg) => cfg,
                    Err(e) => return Self::err_text(&e),
                };
                if let Some(ref cfg) = advisor_override {
                    if orchestration::brofile::resolve_brofile(
                        &cfg.brofile,
                        store_dir,
                        p.project_dir.as_deref(),
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", cfg.brofile));
                    }
                }
                let team_name = p
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{template}-{}", orch::now_ms()));
                let mut tp = tp;
                if advisor_override.is_some() {
                    tp.advisor = advisor_override;
                }
                let mut t =
                    team::instantiate_team(&tp, &team_name, p.project_dir.as_deref(), store_dir);
                if let Err(e) = self.initialize_team_advisor(&mut t).await {
                    return Self::err_text(&e);
                }
                Self::ok_json(&json!({
                    "created": t.name,
                    "teamplate": tp.name,
                    "members": t.members.iter().map(|m| json!({"name": m.name, "brofile": m.brofile})).collect::<Vec<_>>(),
                    "advisor": t.advisor.as_ref().map(|a| json!({
                        "name": a.name,
                        "brofile": a.config.brofile,
                        "sessionId": a.session_id,
                        "taskCount": a.task_history.len(),
                        "packetId": a.config.packet_id,
                        "mode": a.config.mode.as_ref(),
                    })),
                }))
            }
            "list" => {
                let teams = team::load_all_teams(store_dir);
                let list: Vec<Value> = teams
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "teamplate": t.teamplate,
                            "memberCount": t.members.len(),
                            "createdAt": t.created_at,
                            "projectDir": t.project_dir,
                            "advisor": t.advisor.as_ref().map(|a| json!({
                                "name": a.name,
                                "brofile": a.config.brofile,
                                "sessionId": a.session_id,
                                "packetId": a.config.packet_id,
                                "mode": a.config.mode.as_ref(),
                            })),
                        })
                    })
                    .collect();
                Self::ok_json(&json!(list))
            }
            "dissolve" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                if p.cancel_running.unwrap_or(false) {
                    let task_store = self.state.task_store.read();
                    for member in &loaded_team.members {
                        for tid in &member.task_history {
                            if let Some(task) = task_store.get(tid) {
                                let _ = orch::cancel_task(
                                    &task,
                                    &self.state.task_store,
                                    &self.state.store_dir,
                                );
                            }
                        }
                    }
                }
                team::remove_team(name, store_dir);
                Self::ok_json(&json!({"dissolved": name}))
            }
            "roster" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                let task_store = self.state.task_store.read();
                let roster: Vec<Value> = loaded_team
                    .members
                    .iter()
                    .map(|m| {
                        let account = orchestration::brofile::resolve_brofile(
                            &m.brofile,
                            store_dir,
                            loaded_team.project_dir.as_deref(),
                        )
                        .and_then(|bf| {
                            orchestration::brofile::effective_account(
                                bf.provider,
                                bf.account.as_deref(),
                                store_dir,
                            )
                        });
                        let latest_tid = m.task_history.last();
                        let latest = latest_tid.and_then(|id| task_store.get(id)).map(|t| {
                        let inner = t.inner.lock();
                        json!({
                            "taskId": inner.id,
                            "status": inner.status,
                            "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                        })
                    });
                        json!({
                            "name": m.name,
                            "brofile": m.brofile,
                            "account": account,
                            "sessionId": m.session_id,
                            "taskCount": m.task_history.len(),
                            "latestTask": latest,
                        })
                    })
                    .collect();
                Self::ok_json(&json!({
                    "team": name,
                    "teamplate": loaded_team.teamplate,
                    "advisor": loaded_team.advisor.as_ref().map(|a| json!({
                        "name": a.name,
                        "brofile": a.config.brofile,
                        "sessionId": a.session_id,
                        "taskCount": a.task_history.len(),
                        "packetId": a.config.packet_id,
                        "mode": a.config.mode.as_ref(),
                        "charter": a.config.charter,
                    })),
                    "members": roster
                }))
            }
            _ => Self::err_text(&format!("Unknown team action: {}", p.action)),
        }
    }

    #[tool(
        name = "bro_orchestrate_author",
        description = "Compile a prose charter into a validated workflow spec. Dispatches an authoring LLM with the sm-workflow-orchestration runbook + a minimal reference example, parses its JSON response, cross-validates via the engine's compile step, retries once on compile failure with the error appended, and returns the validated spec — ready to pass to `bro_orchestrate_run`. Closes the authoring loop: operators describe the arc in prose, get a JSON spec back (with per-node `next` transitions), dispatch without hand-writing the graph."
    )]
    async fn bro_orchestrate_author(
        &self,
        Parameters(p): Parameters<OrchestrateAuthorParams>,
    ) -> CallToolResult {
        // Load the runbook + a reference example.
        let runbook = match system_memory::get("sm-workflow-orchestration") {
            Some(sm) => sm.content,
            None => {
                return Self::err_text(
                    "sm-workflow-orchestration runbook not found — internal error",
                );
            }
        };
        let reference_example = include_str!("../examples/workflows/e2e-gated.json");
        let hint_line = p
            .hint
            .as_deref()
            .map(|h| format!("\nShape hint: match the `{h}` pattern from the runbook if it fits the charter.\n"))
            .unwrap_or_default();

        let base_prompt = format!(
            "You are a workflow spec compiler. Convert a prose charter into a validated workflow JSON spec for the blackbox `bro_orchestrate_run` engine.\n\n\
=== REFERENCE RUNBOOK ===\n{runbook}\n\n\
=== REFERENCE EXAMPLE (e2e-gated.json) ===\n{reference_example}\n\n\
=== CHARTER ===\n{charter}\n{hint_line}\n\
=== OUTPUT INSTRUCTIONS ===\n\
Output ONLY the JSON workflow spec — no preamble, no prose explanation, no trailing commentary. Start with `{{` and end with `}}`. You may wrap in ```json fences; the parser handles both.\n\n\
Constraints:\n\
- Use actor kinds only from {{executor, ensemble}}. Persona / role / contract (advisor, triager, planner, facilitator, specialist, …) is the brofile lens + prompt + on_exit `parse_json` validator — not an engine type.\n\
- Cross-reference every `actor` field in nodes to a declared actor name.\n\
- Every activity node in the graph must have a matching entry in `nodes`.\n\
- Every `nodes` entry (except ones with `subworkflow`) needs an `actor`.\n\
- Top-level `start` names the entry node; every node carries a `next` clause whose `type` is one of `goto` / `branch` / `fork` / `terminal`. There is no `graph` string.\n\
- If you reference a gate or policy packet ID, use a placeholder like `packet-TODO` — the operator will fill it in after compilation.\n\
- Do NOT invent new actor kinds or graph primitives.\n",
            charter = p.charter,
        );

        let first_task = match self
            .workflow_dispatch_executor(&p.brofile, &base_prompt, p.project_dir.as_deref(), None)
            .await
        {
            Ok(t) => t,
            Err(e) => return Self::err_text(&format!("authoring dispatch failed: {e}")),
        };
        let completed = orch::wait_for_task_with_timeout(&first_task, Some(600.0)).await;
        if !completed {
            return Self::err_text("authoring dispatch timed out");
        }
        let first_output = orch::task_result_json(&first_task)
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let first_session_id = first_task.inner.lock().session_id.clone();

        // Try to compile. If it fails, retry once with the error.
        match extract_and_compile_workflow(&first_output) {
            Ok(spec) => Self::ok_json(&serde_json::json!({
                "workflow": spec,
                "attempts": 1,
                "author_session_id": first_session_id,
            })),
            Err(first_err) => {
                let retry_prompt = format!(
                    "Your previous spec failed validation with this error:\n\n{first_err}\n\nRevise and output the corrected JSON spec. Same output rules — no preamble, no trailing prose."
                );
                // Resume the same session so the LLM sees its prior output.
                let retry_task = match self
                    .workflow_dispatch_executor(
                        &p.brofile,
                        &retry_prompt,
                        p.project_dir.as_deref(),
                        Some(&first_session_id),
                    )
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        return Self::err_text(&format!(
                            "authoring retry dispatch failed: {e}; first error: {first_err}"
                        ));
                    }
                };
                let retry_completed =
                    orch::wait_for_task_with_timeout(&retry_task, Some(600.0)).await;
                if !retry_completed {
                    return Self::err_text(&format!(
                        "authoring retry timed out; first error: {first_err}"
                    ));
                }
                let retry_output = orch::task_result_json(&retry_task)
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match extract_and_compile_workflow(&retry_output) {
                    Ok(spec) => Self::ok_json(&serde_json::json!({
                        "workflow": spec,
                        "attempts": 2,
                        "author_session_id": first_session_id,
                        "first_error": first_err,
                    })),
                    Err(second_err) => Self::err_text(&format!(
                        "authoring failed after 2 attempts. First error: {first_err} | Second error: {second_err}"
                    )),
                }
            }
        }
    }

    #[tool(
        name = "bro_orchestrate_run",
        description = "Dispatch a workflow. Takes a full spec (actors, nodes with per-node `next` transitions: goto / branch / fork / terminal) and blocks until the arc terminates. Returns the event log, per-node outputs, and the `arc_thread_id` for post-hoc audit via `bbox_notes(thread_id=...)` or `bro orchestrate status`. Pass `dry_run=true` to validate + summarize without dispatching any bros. Replaces long skill-prose protocols like overmind/crucible — the daemon owns the state machine, dispatched bros are stateless function-call turns. See `sm-workflow-orchestration` via `bbox_knowledge`, `schema/workflow.schema.json` for the JSON Schema, and `examples/workflows/` for the shape catalog."
    )]
    async fn bro_orchestrate_run(
        &self,
        Parameters(p): Parameters<OrchestrateRunParams>,
    ) -> CallToolResult {
        let spec: workflow::Workflow = match serde_json::from_value(p.workflow) {
            Ok(s) => s,
            Err(e) => {
                return Self::err_text(&format!("workflow parse failed: {e}"));
            }
        };
        let compiled = match workflow::compile(spec) {
            Ok(c) => c,
            Err(e) => return Self::err_text(&format!("workflow compile failed: {e}")),
        };
        // Capability validation — walk every actor's brofile/team →
        // provider and verify the actor's `requires` capabilities are
        // covered. Hard fail rather than silent route-around.
        if let Err(e) = validate_workflow_capabilities(&compiled, &self.state) {
            return Self::err_text(&format!("workflow capability validation failed: {e}"));
        }
        if p.dry_run.unwrap_or(false) {
            let result = workflow::engine::dry_run(&compiled);
            return Self::ok_json(&serde_json::to_value(&result).unwrap_or_default());
        }
        let initial_vars = p.initial_vars.unwrap_or_default();
        let result = workflow::run_workflow_with_initial_vars(
            self,
            &compiled,
            p.project_dir,
            p.max_steps,
            initial_vars,
        )
        .await;
        Self::ok_json(&serde_json::to_value(&result).unwrap_or_default())
    }

    #[tool(
        name = "bro_arc_signal",
        description = "Resolve a pending Wait by signal name + correlation tuple. Same dispatch path that the webhook router uses for `signal_arc` verdicts — surfaced as MCP so an operator can manually advance an arc that's blocked on an external event."
    )]
    async fn bro_arc_signal(&self, Parameters(p): Parameters<ArcSignalParams>) -> CallToolResult {
        let correlation = p.correlate.unwrap_or_default();
        let payload = p
            .payload
            .unwrap_or_else(|| Value::Object(correlation.clone()));
        let result = signal_arc_dispatch(&self.state, &p.signal, correlation, payload).await;
        Self::ok_json(&result)
    }

    #[tool(
        name = "bro_arc_status",
        description = "Read-only structured query against active and recently-finished arcs. Returns the current ArcSnapshot (current_node, completed_nodes, in_flight_nodes, last_verdict, visit_counts, started_at) plus pending-wait registrations for the arc."
    )]
    async fn bro_arc_status(&self, Parameters(p): Parameters<ArcStatusParams>) -> CallToolResult {
        let snapshots: Vec<&ArcSnapshot> = if let Some(arc_id) = &p.arc_id {
            self.state
                .running_arcs
                .read()
                .values()
                .filter(|s| s.arc_thread_id == *arc_id)
                .cloned()
                .collect::<Vec<_>>()
                .iter()
                .map(|_| unreachable!()) // we cloned above; collect adapter
                .collect()
        } else {
            // Default: all running.
            let map = self.state.running_arcs.read();
            return Self::ok_json(&serde_json::json!({
                "snapshots": map.values().collect::<Vec<_>>(),
                "pending_waits": self.state.wait_store.snapshot(),
            }));
        };
        let _ = snapshots;
        let map = self.state.running_arcs.read();
        let wanted = p.arc_id.unwrap_or_default();
        let snap = map.values().find(|s| s.arc_thread_id == wanted).cloned();
        let waits = self
            .state
            .wait_store
            .snapshot()
            .into_iter()
            .filter(|w| w.arc_id == wanted)
            .collect::<Vec<_>>();
        Self::ok_json(&serde_json::json!({
            "snapshot": snap,
            "pending_waits": waits,
        }))
    }

    #[tool(
        name = "bro_arc_cancel",
        description = "Cancel a running workflow arc by id. Trips the arc's cancellation token; the runner observes between node iterations and inside Wait suspensions, bails out with status `cancelled`, runs `on_arc_cancel` (if declared) followed by `on_arc_exit`, and writes a `blocked` note (`workflow cancelled`) on the arc's thread. Returns `{cancelled: true|false}` — false means no token registered for that arc id (already terminated, never started, or wrong id)."
    )]
    async fn bro_arc_cancel(&self, Parameters(p): Parameters<ArcCancelParams>) -> CallToolResult {
        let cancelled = self.state.cancel_arc(&p.arc_id);
        Self::ok_json(&serde_json::json!({
            "arc_id": p.arc_id,
            "cancelled": cancelled,
        }))
    }

    #[tool(
        name = "bro_signals",
        description = "Recent signal-dispatch events as a bounded ring buffer (last ~200). Every call to the signal router records one entry: (timestamp, signal, correlation, outcome, matched_arc_id, matched_wait_id, idle_pending). `outcome` is `matched` (resolved a wait) or `no_matching_wait` (fell idle); on idle, `idle_pending` carries the pending-with-same-signal snapshot at dispatch time so the diff between what arrived and what was waiting is one read away. Filter by `signal=` (exact match) and `since=` (ISO timestamp). Replaces the journalctl|grep workflow for debugging webhook → routing → signal → wait paths."
    )]
    async fn bro_signals(&self, Parameters(p): Parameters<SignalsParams>) -> CallToolResult {
        let log = self.state.signal_log.read();
        let limit = p.limit.unwrap_or(50).min(SIGNAL_LOG_CAP);
        let mut out: Vec<&SignalEvent> = log
            .iter()
            .filter(|e| match &p.signal {
                Some(s) => e.signal == *s,
                None => true,
            })
            .filter(|e| match &p.since {
                Some(ts) => e.timestamp.as_str() >= ts.as_str(),
                None => true,
            })
            .filter(|e| match &p.outcome {
                Some(o) => e.outcome == *o,
                None => true,
            })
            .collect();
        // Newest first.
        out.reverse();
        out.truncate(limit);
        Self::ok_json(&serde_json::json!({
            "events": out,
            "total_in_buffer": log.len(),
            "buffer_capacity": SIGNAL_LOG_CAP,
        }))
    }

    #[tool(
        name = "bro_webhook_replay",
        description = "Replay an arbitrary payload through an installed webhook's extractor + routing packet WITHOUT dispatching the verdict. Returns the extracted entity, the routing verdict's classification, and the resolved consequent (after `${entity.X}` substitution). Skips signature verification — same path as the HTTP `/webhook/:name/replay` endpoint, surfaced as MCP so routing-rule iteration happens inside the tool surface. Records the replay into the same delivery ring buffer (`source: replay`) so `bro_webhook_deliveries` shows it."
    )]
    async fn bro_webhook_replay(
        &self,
        Parameters(p): Parameters<WebhookReplayParams>,
    ) -> CallToolResult {
        let headers = p
            .headers
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        match webhook_replay_inner(&self.state, &p.name, &p.body, &headers) {
            Ok(v) => Self::ok_json(&v),
            Err((status, msg)) => {
                Self::err_text(&format!("replay failed ({}): {msg}", status.as_u16()))
            }
        }
    }

    #[tool(
        name = "bro_webhook_deliveries",
        description = "Recent webhook deliveries as a bounded ring buffer (last ~200). Each entry: (received_at, webhook_name, source, headers, extracted_entity, verdict_classification, response_status, response_body). `source` is `webhook` for live deliveries and `replay` for the no-signature replay endpoint. `verdict_classification` echoes how the routing packet classified the event (`start_arc` / `signal_arc` / `cancel_arc` / `ignore` / `dead_letter` / `no_match` / `duplicate_dropped` / `error`). Filter by `name=` (webhook name) and `since=` (ISO timestamp). Replaces poking the upstream code-host's hook-task table or grepping the daemon's tracing log to debug routing-rule misses."
    )]
    async fn bro_webhook_deliveries(
        &self,
        Parameters(p): Parameters<WebhookDeliveriesParams>,
    ) -> CallToolResult {
        let log = self.state.webhook_delivery_log.read();
        let limit = p.limit.unwrap_or(50).min(WEBHOOK_LOG_CAP);
        let mut out: Vec<&WebhookDelivery> = log
            .iter()
            .filter(|d| match &p.name {
                Some(n) => d.webhook_name == *n,
                None => true,
            })
            .filter(|d| match &p.since {
                Some(ts) => d.received_at.as_str() >= ts.as_str(),
                None => true,
            })
            .filter(|d| match &p.verdict_classification {
                Some(v) => d.verdict_classification == *v,
                None => true,
            })
            .collect();
        // Newest first.
        out.reverse();
        out.truncate(limit);
        Self::ok_json(&serde_json::json!({
            "deliveries": out,
            "total_in_buffer": log.len(),
            "buffer_capacity": WEBHOOK_LOG_CAP,
        }))
    }

    #[tool(
        name = "bro_webhook_install",
        description = "Install a webhook endpoint reachable at POST /webhook/<name>. Signature verification, extractor projection, and routing-packet dispatch are mechanical at the daemon. Routing packets must already be operator-installed in the global packet store."
    )]
    async fn bro_webhook_install(
        &self,
        Parameters(p): Parameters<WebhookInstallParams>,
    ) -> CallToolResult {
        let spec: webhooks::WebhookSpec = match serde_json::from_value(p.spec) {
            Ok(s) => s,
            Err(e) => return Self::err_text(&format!("webhook spec parse failed: {e}")),
        };
        // Reject schemes that aren't safe under the daemon's bind
        // (today: SignatureScheme::None requires loopback). Defense
        // in depth — verify_signature also enforces, but rejecting
        // here keeps the on-disk registry clean.
        if let Err(e) = webhooks::install_check(&spec.signature, self.state.bind_is_loopback) {
            return Self::err_text(&format!("webhook install rejected: {e}"));
        }
        // Persist for restart durability.
        let dir = self.state.store_dir.join("webhooks");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("webhook persist failed: {e}"));
        }
        self.state.webhooks.install(spec.clone());
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "endpoint": format!("/webhook/{}", spec.name),
        }))
    }

    #[tool(
        name = "bro_webhook_list",
        description = "List installed webhook endpoints with their signature scheme + routing packet."
    )]
    async fn bro_webhook_list(&self) -> CallToolResult {
        let list = self.state.webhooks.list();
        Self::ok_json(&serde_json::json!({"webhooks": list}))
    }

    #[tool(
        name = "bro_poller_install",
        description = "Install a scheduled HTTP-source poller that converges on the same routing pipeline as webhook ingress. Use when the upstream doesn't push (no webhook capability) or the daemon has no public ingress. Spec carries: name, every_seconds (>= BBOX_POLLER_MIN_INTERVAL_SECS, default 5), source (HttpFetchSpec), optional iterate (Selector — array path to explode response into N events), per-event extractor, optional dedup_id_path (Selector for stable id, in-memory recent-seen ring per poller), routing_packet, optional default_project_dir. Persisted to disk + tick loop spawned immediately; reinstall replaces the running task."
    )]
    async fn bro_poller_install(
        &self,
        Parameters(p): Parameters<PollerInstallParams>,
    ) -> CallToolResult {
        let spec: pollers::PollerSpec = match serde_json::from_value(p.spec) {
            Ok(s) => s,
            Err(e) => return Self::err_text(&format!("poller spec parse failed: {e}")),
        };
        // Persist for restart durability.
        let dir = self.state.store_dir.join("pollers");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("poller persist failed: {e}"));
        }
        self.state.pollers.install(spec.clone());
        let handle = pollers::spawn_loop(self.state.clone(), spec.clone());
        self.state.pollers.track_handle(&spec.name, handle);
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "every_seconds": spec.every_seconds,
        }))
    }

    #[tool(
        name = "bro_poller_list",
        description = "List installed pollers with their schedule + source URL + routing packet."
    )]
    async fn bro_poller_list(&self) -> CallToolResult {
        let list = self.state.pollers.list();
        Self::ok_json(&serde_json::json!({"pollers": list}))
    }

    #[tool(
        name = "bro_cron_install",
        description = "Install a calendar-driven cron inlet — sibling of webhook + poller. Same routing pipeline (extractor → routing packet → dispatch_routed_event), different trigger source: wall-clock schedule, no fetch. Spec: name, schedule (6-field cron expr `sec min hour dom mon dow`), optional payload (operator-supplied entity fields), optional concurrency cap (default 1, set 0 to disable), routing_packet, optional default_project_dir. Synthetic entity fields `cron_name` + `tick_at` are merged in at tick time so routing rules can discriminate."
    )]
    async fn bro_cron_install(
        &self,
        Parameters(p): Parameters<CronInstallParams>,
    ) -> CallToolResult {
        let spec: crons::CronSpec = match serde_json::from_value(p.spec) {
            Ok(s) => s,
            Err(e) => return Self::err_text(&format!("cron spec parse failed: {e}")),
        };
        if let Err(e) = crons::validate_schedule(&spec.schedule) {
            return Self::err_text(&format!("cron schedule invalid: {e}"));
        }
        let dir = self.state.store_dir.join("crons");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", spec.name));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("cron persist failed: {e}"));
        }
        self.state.crons.install(spec.clone());
        let handle = crons::spawn_loop(self.state.clone(), spec.clone());
        self.state.crons.track_handle(&spec.name, handle);
        Self::ok_json(&serde_json::json!({
            "status": "installed",
            "name": spec.name,
            "schedule": spec.schedule,
            "concurrency": spec.concurrency,
        }))
    }

    #[tool(
        name = "bro_cron_list",
        description = "List installed crons with schedule + concurrency cap + routing packet."
    )]
    async fn bro_cron_list(&self) -> CallToolResult {
        let list = self.state.crons.list();
        Self::ok_json(&serde_json::json!({"crons": list}))
    }

    #[tool(
        name = "bro_cron_upcoming",
        description = "Compute the next N scheduled times for a cron expression as RFC3339 strings. Pure function — does not touch the registry."
    )]
    async fn bro_cron_upcoming(
        &self,
        Parameters(p): Parameters<CronUpcomingParams>,
    ) -> CallToolResult {
        let n = p.count.unwrap_or(5).clamp(1, 100);
        match crons::upcoming_times(&p.schedule, n) {
            Ok(times) => Self::ok_json(&serde_json::json!({
                "schedule": p.schedule,
                "upcoming": times,
            })),
            Err(e) => Self::err_text(&format!("schedule '{}': {e}", p.schedule)),
        }
    }

    // ── Whiteboard tools — multi-agent deliberation surface ─────

    #[tool(
        name = "whiteboard_open",
        description = "Open a new whiteboard for structured deliberation. The board collects posts (blind phase), annotations (validate/debate phases), and votes (debate phase) from registered agents, advanced through phases by a facilitator-or-operator role. Returns when the board is created and the opener is registered as facilitator. Idempotent re-open against an existing id is rejected — use whiteboard_state to inspect."
    )]
    async fn whiteboard_open(
        &self,
        Parameters(p): Parameters<WhiteboardOpenParams>,
    ) -> CallToolResult {
        let project = p.project.clone().unwrap_or_default();
        let domain = p.domain.clone().unwrap_or_else(|| "facilitation".into());
        if let Err(e) = self.state.whiteboards.open(
            &p.board_id,
            &p.topic,
            &project,
            p.arc_thread_id.as_deref(),
            &p.opened_by,
        ) {
            return Self::err_text(&format!("whiteboard_open: {e}"));
        }
        if let Err(e) = self.state.whiteboards.register(
            &p.board_id,
            &p.opened_by,
            whiteboards::Role::Facilitator,
            &domain,
        ) {
            return Self::err_text(&format!("whiteboard_open register opener: {e}"));
        }
        Self::ok_json(&serde_json::json!({
            "status": "opened",
            "board_id": p.board_id,
            "topic": p.topic,
            "phase": "blind",
            "facilitator": p.opened_by,
        }))
    }

    #[tool(
        name = "whiteboard_register",
        description = "Register an agent on an existing board. Idempotent — re-registration with the same name is a no-op. Roles: `specialist` (post + annotate + vote), `facilitator` (transition + post + annotate + vote), `operator` (same powers as facilitator; convention is for human / external Claude joiners)."
    )]
    async fn whiteboard_register(
        &self,
        Parameters(p): Parameters<WhiteboardRegisterParams>,
    ) -> CallToolResult {
        let role = match p.role.as_str() {
            "specialist" => whiteboards::Role::Specialist,
            "facilitator" => whiteboards::Role::Facilitator,
            "operator" => whiteboards::Role::Operator,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_register: unknown role '{other}' (use specialist / facilitator / operator)"
                ));
            }
        };
        match self
            .state
            .whiteboards
            .register(&p.board_id, &p.agent_name, role, &p.domain)
        {
            Ok(()) => Self::ok_json(&serde_json::json!({
                "status": "registered",
                "board_id": p.board_id,
                "agent_name": p.agent_name,
                "role": p.role,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_register: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_post",
        description = "Post a structured claim/proposal/concern to a whiteboard during its blind phase. Type one of: proposal, claim, concern, informational. Optional fields target_file / target_location / severity / finding_refs / cascade_targets enable conflict detection downstream."
    )]
    async fn whiteboard_post(
        &self,
        Parameters(p): Parameters<WhiteboardPostParams>,
    ) -> CallToolResult {
        let post_type = match p.post_type.as_str() {
            "proposal" => whiteboards::PostType::Proposal,
            "claim" => whiteboards::PostType::Claim,
            "concern" => whiteboards::PostType::Concern,
            "informational" => whiteboards::PostType::Informational,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_post: unknown type '{other}' (use proposal / claim / concern / informational)"
                ));
            }
        };
        let severity = match p.severity.as_deref() {
            Some("critical") => Some(whiteboards::Severity::Critical),
            Some("high") => Some(whiteboards::Severity::High),
            Some("medium") => Some(whiteboards::Severity::Medium),
            Some("low") => Some(whiteboards::Severity::Low),
            Some(other) => {
                return Self::err_text(&format!("whiteboard_post: unknown severity '{other}'"));
            }
            None => None,
        };
        match self.state.whiteboards.post(
            &p.board_id,
            &p.agent_name,
            post_type,
            &p.title,
            &p.body,
            p.target_file.as_deref(),
            p.target_location.as_deref(),
            severity,
            p.finding_refs.unwrap_or_default(),
            p.cascade_targets.unwrap_or_default(),
        ) {
            Ok(post_id) => Self::ok_json(&serde_json::json!({
                "status": "posted",
                "board_id": p.board_id,
                "post_id": post_id,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_post: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_state",
        description = "Read board state filtered for the requesting agent. Phaser-style visibility: blind phase shows only own posts; later phases reveal full board. Includes phase, phase_age_secs, ready_for_transition advisory flag, post / annotation / vote arrays scoped to what this agent should see."
    )]
    async fn whiteboard_state(
        &self,
        Parameters(p): Parameters<WhiteboardStateParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_state: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let view = whiteboards::filter_for_agent(&board_arc.read(), &p.agent_name);
        match view {
            Ok(v) => Self::ok_json(&serde_json::to_value(&v).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("whiteboard_state: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_annotate",
        description = "Annotate a post during the validate or debate phase. Validate phase accepts only `validation` (with required `result`: confirmed / refuted / inconclusive). Debate phase accepts `challenge`, `corroborate`, or `resolve` (resolve must reference a challenge id via `resolves`)."
    )]
    async fn whiteboard_annotate(
        &self,
        Parameters(p): Parameters<WhiteboardAnnotateParams>,
    ) -> CallToolResult {
        let ann = match p.annotation_type.as_str() {
            "challenge" => whiteboards::AnnotationType::Challenge,
            "corroborate" => whiteboards::AnnotationType::Corroborate,
            "resolve" => whiteboards::AnnotationType::Resolve,
            "validation" => whiteboards::AnnotationType::Validation,
            other => {
                return Self::err_text(&format!("whiteboard_annotate: unknown type '{other}'"));
            }
        };
        let result = match p.result.as_deref() {
            Some("confirmed") => Some(whiteboards::ValidationResult::Confirmed),
            Some("refuted") => Some(whiteboards::ValidationResult::Refuted),
            Some("inconclusive") => Some(whiteboards::ValidationResult::Inconclusive),
            Some(other) => {
                return Self::err_text(&format!("whiteboard_annotate: unknown result '{other}'"));
            }
            None => None,
        };
        match self.state.whiteboards.annotate(
            &p.board_id,
            &p.agent_name,
            &p.post_id,
            ann,
            &p.body,
            result,
            p.resolves.as_deref(),
        ) {
            Ok(ann_id) => Self::ok_json(&serde_json::json!({
                "status": "annotated",
                "board_id": p.board_id,
                "annotation_id": ann_id,
                "post_id": p.post_id,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_annotate: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_vote",
        description = "Cast an advisory vote on a post during the debate phase. One vote per agent per post — re-vote replaces. Vote: accept, reject, or defer."
    )]
    async fn whiteboard_vote(
        &self,
        Parameters(p): Parameters<WhiteboardVoteParams>,
    ) -> CallToolResult {
        let v = match p.vote.as_str() {
            "accept" => whiteboards::VoteValue::Accept,
            "reject" => whiteboards::VoteValue::Reject,
            "defer" => whiteboards::VoteValue::Defer,
            other => return Self::err_text(&format!("whiteboard_vote: unknown vote '{other}'")),
        };
        match self.state.whiteboards.vote(
            &p.board_id,
            &p.agent_name,
            &p.post_id,
            v,
            p.reason.as_deref(),
        ) {
            Ok(replaced) => Self::ok_json(&serde_json::json!({
                "status": if replaced { "vote_replaced" } else { "voted" },
                "board_id": p.board_id,
                "post_id": p.post_id,
                "vote": p.vote,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_vote: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_transition",
        description = "Advance the board to a new phase. Facilitator or operator role required. Sequence: blind → read → validate → debate → resolve → archived; read → debate is a legal skip. Transition emits a `board-transitioned` signal correlated to (board_id, target_phase) so any wait node observing the board resumes."
    )]
    async fn whiteboard_transition(
        &self,
        Parameters(p): Parameters<WhiteboardTransitionParams>,
    ) -> CallToolResult {
        let target = match p.target_phase.as_str() {
            "read" => whiteboards::Phase::Read,
            "validate" => whiteboards::Phase::Validate,
            "debate" => whiteboards::Phase::Debate,
            "resolve" => whiteboards::Phase::Resolve,
            "archived" => whiteboards::Phase::Archived,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_transition: unknown target_phase '{other}'"
                ));
            }
        };
        let result = self.state.whiteboards.transition(
            &p.board_id,
            &p.agent_name,
            target,
            p.summary.as_deref(),
        );
        match result {
            Ok((from, to)) => {
                // Fire the routed signal so wait_for_phase nodes resume.
                let state = self.state.clone();
                let board_id = p.board_id.clone();
                let from_str = from.as_str().to_string();
                let to_str = to.as_str().to_string();
                tokio::spawn(async move {
                    let entity = serde_json::json!({
                        "board_id": board_id,
                        "from_phase": from_str,
                        "to_phase": to_str,
                    });
                    let mut correlate = serde_json::Map::new();
                    correlate.insert("board".into(), serde_json::json!(board_id));
                    correlate.insert("phase".into(), serde_json::json!(to_str));
                    let verdict = routing::RoutingVerdict::SignalArc {
                        signal: "board-transitioned".into(),
                        correlate,
                        payload: Some(entity.clone()),
                    };
                    let _ =
                        dispatch_routing_verdict_direct(state, "whiteboard", verdict, entity).await;
                });
                Self::ok_json(&serde_json::json!({
                    "status": "transitioned",
                    "board_id": p.board_id,
                    "from": from.as_str(),
                    "to": to.as_str(),
                }))
            }
            Err(e) => Self::err_text(&format!("whiteboard_transition: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_conflicts",
        description = "Auto-detect conflicts between posts on a board. Returns three kinds: `direct_overlap` (same target_file + identical target_location), `cascade_collision` (post A cascades to post B's direct target), `severity_disagreement` (same finding_ref, distinct severities). Available in any phase past blind."
    )]
    async fn whiteboard_conflicts(
        &self,
        Parameters(p): Parameters<WhiteboardConflictsParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_conflicts: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let board = board_arc.read();
        if !board.agents.contains_key(&p.agent_name) {
            return Self::err_text(&format!(
                "agent '{}' not registered on board '{}'",
                p.agent_name, p.board_id
            ));
        }
        if board.phase == whiteboards::Phase::Blind {
            return Self::err_text("whiteboard_conflicts: not available in blind phase");
        }
        let conflicts = whiteboards::detect_conflicts(&board);
        Self::ok_json(&serde_json::json!({
            "phase": board.phase.as_str(),
            "post_count": board.posts.len(),
            "conflict_count": conflicts.len(),
            "conflicts": conflicts,
        }))
    }

    #[tool(
        name = "whiteboard_summarize",
        description = "Condensed board summary without full post bodies. Returns counts per type, vote tally per post, conflict count, unresolved-challenge count, agent status (has_posted), phase age, ready_for_transition advisory."
    )]
    async fn whiteboard_summarize(
        &self,
        Parameters(p): Parameters<WhiteboardSummarizeParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_summarize: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let board = board_arc.read();
        if !board.agents.contains_key(&p.agent_name) {
            return Self::err_text(&format!(
                "agent '{}' not registered on board '{}'",
                p.agent_name, p.board_id
            ));
        }
        let phase_age = chrono::DateTime::parse_from_rfc3339(
            &board
                .phase_history
                .last()
                .map(|h| h.at.clone())
                .unwrap_or_default(),
        )
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0);
        let mut posts_by_type = std::collections::BTreeMap::<&str, u32>::new();
        for post in &board.posts {
            let key = match post.post_type {
                whiteboards::PostType::Proposal => "proposal",
                whiteboards::PostType::Claim => "claim",
                whiteboards::PostType::Concern => "concern",
                whiteboards::PostType::Informational => "informational",
            };
            *posts_by_type.entry(key).or_default() += 1;
        }
        let posted: std::collections::HashSet<&str> =
            board.posts.iter().map(|p| p.agent.as_str()).collect();
        let agents_status: serde_json::Map<String, serde_json::Value> = board
            .agents
            .iter()
            .map(|(name, info)| {
                (
                    name.clone(),
                    serde_json::json!({
                        "role": match info.role {
                            whiteboards::Role::Specialist => "specialist",
                            whiteboards::Role::Facilitator => "facilitator",
                            whiteboards::Role::Operator => "operator",
                        },
                        "domain": info.domain,
                        "has_posted": posted.contains(name.as_str()),
                    }),
                )
            })
            .collect();
        let conflicts = if board.phase == whiteboards::Phase::Blind {
            Vec::new()
        } else {
            whiteboards::detect_conflicts(&board)
        };
        let challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Challenge)
            .count();
        let resolved: std::collections::HashSet<&str> = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Resolve)
            .filter_map(|a| a.resolves.as_deref())
            .collect();
        let unresolved_challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Challenge)
            .filter(|c| !resolved.contains(c.id.as_str()))
            .count();
        Self::ok_json(&serde_json::json!({
            "board_id": board.id,
            "topic": board.topic,
            "phase": board.phase.as_str(),
            "phase_age_secs": phase_age,
            "ready_for_transition": board.ready_for_transition(phase_age),
            "post_count": board.posts.len(),
            "posts_by_type": posts_by_type,
            "annotation_count": board.annotations.len(),
            "vote_count": board.votes.len(),
            "vote_tally": board.vote_tally(),
            "conflict_count": conflicts.len(),
            "challenge_count": challenges,
            "unresolved_challenges": unresolved_challenges,
            "agents": agents_status,
        }))
    }

    #[tool(
        name = "whiteboard_archive",
        description = "Archive the board. Resolve phase only. Strips active state, moves to `<store>/whiteboards/archive/<id>.json`, returns summary statistics."
    )]
    async fn whiteboard_archive(
        &self,
        Parameters(p): Parameters<WhiteboardArchiveParams>,
    ) -> CallToolResult {
        match self.state.whiteboards.archive(&p.board_id, &p.agent_name) {
            Ok(summary) => Self::ok_json(&serde_json::to_value(&summary).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("whiteboard_archive: {e}")),
        }
    }

    #[tool(
        name = "bro_workflow_install",
        description = "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced."
    )]
    async fn bro_workflow_install(
        &self,
        Parameters(p): Parameters<WorkflowInstallParams>,
    ) -> CallToolResult {
        let spec: workflow::Workflow = match serde_json::from_value(p.spec) {
            Ok(s) => s,
            Err(e) => return Self::err_text(&format!("workflow spec parse failed: {e}")),
        };
        let compiled = match workflow::compile(spec.clone()) {
            Ok(c) => c,
            Err(e) => return Self::err_text(&format!("workflow compile failed: {e}")),
        };
        if let Err(e) = validate_workflow_capabilities(&compiled, &self.state) {
            return Self::err_text(&format!("capability validation failed: {e}"));
        }
        let id = p.id.unwrap_or_else(|| spec.name.clone());
        let dir = self.state.store_dir.join("workflows");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{id}.json"));
        if let Err(e) = std::fs::write(
            &path,
            serde_json::to_string_pretty(&spec).unwrap_or_default(),
        ) {
            return Self::err_text(&format!("workflow persist failed: {e}"));
        }
        self.state
            .workflow_registry
            .write()
            .insert(id.clone(), spec);
        Self::ok_json(&serde_json::json!({"status": "installed", "id": id}))
    }

    #[tool(
        name = "bro_workflow_list",
        description = "List installed workflow specs by id."
    )]
    async fn bro_workflow_list(&self) -> CallToolResult {
        let map = self.state.workflow_registry.read();
        let names: Vec<String> = map.keys().cloned().collect();
        Self::ok_json(&serde_json::json!({"workflows": names}))
    }

    #[tool(
        name = "bro_council_list",
        description = "List active and closed councils. Optional `project` filter narrows by project_dir."
    )]
    fn bro_council_list(&self, Parameters(p): Parameters<CouncilListParams>) -> CallToolResult {
        let summaries = self.state.councils.list_summaries(p.project.as_deref());
        Self::ok_json(&serde_json::json!({"councils": summaries}))
    }

    #[tool(
        name = "bro_council_open",
        description = "Read full council state: metadata, charter, posts, and current envelope status."
    )]
    fn bro_council_open(&self, Parameters(p): Parameters<CouncilOpenParams>) -> CallToolResult {
        let Some(council) = self.state.councils.get(&p.id) else {
            return Self::err_text(&format!("unknown council: {}", p.id));
        };
        let s = council.session.read().clone();
        let posts = council.posts.read().clone();
        let envelopes = council.envelopes.read().clone();
        let summary = council::CouncilSummary {
            id: s.id.clone(),
            team_id: s.team_id.clone(),
            project: s.project.clone(),
            topic: s.topic.clone(),
            status: s.status,
            members: s.member_sessions.keys().cloned().collect(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            post_count: posts.len() as u64,
        };
        Self::ok_json(&serde_json::json!({
            "summary": summary,
            "posts": posts,
            "envelopes": envelopes,
            "charter": s.charter,
        }))
    }

    #[tool(
        name = "bro_council_posts",
        description = "Paginated council transcript. `since_seq` returns posts with sequence > since_seq; `limit` caps response (default 100, max 1000)."
    )]
    fn bro_council_posts(&self, Parameters(p): Parameters<CouncilPostsParams>) -> CallToolResult {
        let Some(council) = self.state.councils.get(&p.id) else {
            return Self::err_text(&format!("unknown council: {}", p.id));
        };
        let since = p.since_seq.unwrap_or(0);
        let limit = p.limit.unwrap_or(100).min(1000);
        let posts: Vec<council::CouncilPost> = council
            .posts
            .read()
            .iter()
            .filter(|post| post.sequence > since)
            .take(limit)
            .cloned()
            .collect();
        Self::ok_json(&serde_json::json!({
            "council_id": p.id,
            "posts": posts,
        }))
    }
}

/// Walk each ActorSpec.requires -> resolve actor brofiles/teams -> provider
/// capabilities. Empty `requires` is satisfied.
pub(crate) fn validate_workflow_capabilities(
    compiled: &workflow::CompiledWorkflow,
    state: &Arc<SharedState>,
) -> Result<(), String> {
    for (actor_name, actor) in &compiled.spec.actors {
        if actor.requires.is_empty() {
            continue;
        }
        let providers = resolve_actor_providers(actor, state)?;
        if providers.is_empty() {
            return Err(format!(
                "actor '{actor_name}' requires {:?} but resolves to no providers",
                actor.requires
            ));
        }
        for provider in &providers {
            let caps = provider.capabilities();
            let missing: Vec<_> = actor
                .requires
                .iter()
                .filter(|r| !caps.contains(r))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "actor '{actor_name}' requires {:?} but provider '{provider}' lacks {:?}",
                    actor.requires, missing
                ));
            }
        }
    }
    for (node_id, node) in &compiled.spec.nodes {
        if let Some(sub) = &node.subworkflow {
            let sub_compiled = workflow::compile((**sub).clone())
                .map_err(|e| format!("subworkflow on '{node_id}' compile: {e}"))?;
            validate_workflow_capabilities(&sub_compiled, state)
                .map_err(|e| format!("subworkflow on '{node_id}': {e}"))?;
        }
    }
    Ok(())
}

fn resolve_actor_providers(
    actor: &workflow::schema::ActorSpec,
    state: &Arc<SharedState>,
) -> Result<Vec<orchestration::providers::Provider>, String> {
    use std::collections::HashSet;
    let mut providers: HashSet<orchestration::providers::Provider> = HashSet::new();
    match actor.kind {
        workflow::schema::ActorKind::Executor => {
            let brofile_name = actor
                .brofile
                .as_deref()
                .ok_or_else(|| format!("actor (kind={:?}) missing brofile", actor.kind))?;
            let bf = orchestration::brofile::resolve_brofile(brofile_name, &state.store_dir, None)
                .ok_or_else(|| format!("brofile '{brofile_name}' not found"))?;
            providers.insert(bf.provider);
        }
        workflow::schema::ActorKind::Ensemble => {
            let team_name = actor
                .team
                .as_deref()
                .ok_or_else(|| "ensemble actor missing team".to_string())?;
            let team = orchestration::team::load_team(team_name, &state.store_dir)
                .ok_or_else(|| format!("team '{team_name}' not found"))?;
            for member in team.members.iter() {
                let bf = orchestration::brofile::resolve_brofile(
                    &member.brofile,
                    &state.store_dir,
                    None,
                )
                .ok_or_else(|| {
                    format!(
                        "team '{team_name}' member '{}' brofile '{}' not found",
                        member.name, member.brofile
                    )
                })?;
                providers.insert(bf.provider);
            }
        }
    }
    Ok(providers.into_iter().collect())
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ArcSignalParams {
    /// Signal name to deliver (e.g. `pr-merged`, `pr-feedback`).
    pub signal: String,
    /// Correlation tuple to match against pending waits. A wait
    /// matches when every key/value here is present in the wait's
    /// registered correlation. Empty correlation = broadcast.
    #[serde(default)]
    pub correlate: Option<serde_json::Map<String, Value>>,
    /// Optional payload delivered to the resumed wait as
    /// `${last_signal.payload}`. When omitted, the correlation
    /// tuple is used (legacy default — kept so callers that don't
    /// have a payload don't need to manufacture one).
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ArcStatusParams {
    /// Optional arc id (== arc_thread_id from a WorkflowRunResult).
    /// When omitted, returns all running arcs + pending waits.
    #[serde(default)]
    pub arc_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ArcCancelParams {
    /// Arc id (from `WorkflowRunResult.arc_id` / arc_thread_id) to cancel.
    pub arc_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WebhookReplayParams {
    /// Installed webhook name to replay against.
    pub name: String,
    /// Webhook body payload (the JSON Forgejo / GitHub / etc. would
    /// post). Top-level fields are extractor inputs.
    pub body: Value,
    /// Optional inbound headers; keys are lowercased before extractor
    /// projection. Use this to provide event-type / delivery-id
    /// headers (e.g. `{"x-gitea-event": "pull_request"}`) when the
    /// extractor reads from `$._headers.<name>`.
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WebhookDeliveriesParams {
    /// Filter to a specific webhook name.
    #[serde(default)]
    pub name: Option<String>,
    /// Filter to deliveries at or after this ISO 8601 timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Filter by routing verdict classification.
    #[serde(default)]
    pub verdict_classification: Option<String>,
    /// Max deliveries returned, newest-first. Default 50, max = buffer cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct SignalsParams {
    /// Filter to a specific signal name (e.g. `pr-ready`). Exact match.
    #[serde(default)]
    pub signal: Option<String>,
    /// Filter to events at or after this ISO 8601 timestamp.
    #[serde(default)]
    pub since: Option<String>,
    /// Filter by outcome: `matched` or `no_matching_wait`.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Max events returned, newest-first. Default 50, max = buffer cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WebhookInstallParams {
    /// Full WebhookSpec JSON (name, signature, extractor, routing_packet).
    pub spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct PollerInstallParams {
    /// Full PollerSpec JSON (name, every_seconds, source, optional
    /// iterate, extractor, optional dedup_id_path, routing_packet,
    /// optional default_project_dir).
    pub spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CronInstallParams {
    /// Full CronSpec JSON (name, schedule, optional payload, optional
    /// concurrency cap, routing_packet, optional default_project_dir).
    pub spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CronUpcomingParams {
    /// Cron schedule expression (6-field `sec min hour dom mon dow`).
    pub schedule: String,
    /// How many upcoming times to return. Default 5, max 100.
    #[serde(default)]
    pub count: Option<usize>,
}

// ── Whiteboard params ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardOpenParams {
    /// Stable id for the board. Often `<workflow-name>:<arc_id>` or a
    /// deliberation-specific slug ("adr-2026-04-27").
    pub board_id: String,
    /// Free-form topic — surfaces in inbox + agent prompts.
    pub topic: String,
    /// Project root associated with this board. Used for inbox
    /// scoping; doesn't affect storage path.
    #[serde(default)]
    pub project: Option<String>,
    /// Optional arc thread id binding the board to a specific arc.
    /// Set when the engine opens the board on behalf of an arc; absent
    /// when external clients open ad-hoc boards.
    #[serde(default)]
    pub arc_thread_id: Option<String>,
    /// Agent name credited with opening (auto-registered as facilitator).
    pub opened_by: String,
    /// Domain hint for the opener's role (default: "facilitation").
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardRegisterParams {
    pub board_id: String,
    pub agent_name: String,
    /// Role on the board: `specialist`, `facilitator`, or `operator`.
    pub role: String,
    pub domain: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardPostParams {
    pub board_id: String,
    pub agent_name: String,
    /// Post type: `proposal`, `claim`, `concern`, `informational`.
    #[serde(rename = "type")]
    pub post_type: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub target_file: Option<String>,
    #[serde(default)]
    pub target_location: Option<String>,
    /// Severity: `critical`, `high`, `medium`, `low`. Optional.
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub finding_refs: Option<Vec<String>>,
    #[serde(default)]
    pub cascade_targets: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardStateParams {
    pub board_id: String,
    pub agent_name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardAnnotateParams {
    pub board_id: String,
    pub agent_name: String,
    pub post_id: String,
    /// Annotation type: `challenge`, `corroborate`, `resolve`, `validation`.
    #[serde(rename = "type")]
    pub annotation_type: String,
    pub body: String,
    /// Required for `validation`: `confirmed` / `refuted` / `inconclusive`.
    #[serde(default)]
    pub result: Option<String>,
    /// Required for `resolve`: id of the challenge annotation being resolved.
    #[serde(default)]
    pub resolves: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardTransitionParams {
    pub board_id: String,
    pub agent_name: String,
    /// Target phase: `read`, `validate`, `debate`, `resolve`, `archived`.
    pub target_phase: String,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardVoteParams {
    pub board_id: String,
    pub agent_name: String,
    pub post_id: String,
    /// Vote: `accept`, `reject`, `defer`.
    pub vote: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardConflictsParams {
    pub board_id: String,
    pub agent_name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardSummarizeParams {
    pub board_id: String,
    pub agent_name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhiteboardArchiveParams {
    pub board_id: String,
    pub agent_name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WorkflowInstallParams {
    /// Optional id for registry lookup. Defaults to `spec.name`.
    #[serde(default)]
    pub id: Option<String>,
    /// Full Workflow spec JSON.
    pub spec: Value,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct OrchestrateRunParams {
    /// Full workflow spec (Workflow struct serialized as JSON). Must
    /// contain `name`, `version`, `actors`, `nodes`, and `graph` (an
    /// per-node `next` transitions). Optional `policy_packet` for
    /// arc-level advisor-as-packet evaluation.
    pub workflow: Value,
    /// Working directory passed to every dispatched bro.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Cap on activity-node steps (default: 50).
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// When true: parse + cross-validate + summarize; do NOT dispatch.
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// Optional initial vars seeded into the arc's ArcContext at
    /// start. Schema-validated against `Workflow.vars_schema` before
    /// the run begins.
    #[serde(default)]
    pub initial_vars: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct OrchestrateAuthorParams {
    /// Prose charter — what the arc should accomplish. Describe the
    /// actors, the phases, any gate/retry/halt conditions the author
    /// should encode. The compiler turns this into a validated
    /// workflow spec.
    pub charter: String,
    /// Brofile to dispatch as the authoring LLM. Should be a capable
    /// instruction-following model; Claude Haiku is usually sufficient.
    pub brofile: String,
    /// Optional shape hint — e.g. "crucible", "blind-convergence",
    /// "linear", "optimistic-review". If given, the authoring prompt
    /// suggests matching the named pattern.
    #[serde(default)]
    pub hint: Option<String>,
    /// Working directory for the authoring dispatch.
    #[serde(default)]
    pub project_dir: Option<String>,
}

/// Extract a JSON workflow spec from an LLM's response text and
/// validate it via `workflow::compile`. Tolerates: raw JSON, ```json
/// fenced blocks, and prose preamble/trailing commentary. Returns the
/// original JSON Value (pre-compile) on success so callers can re-
/// emit the exact spec the author produced.
fn extract_and_compile_workflow(text: &str) -> Result<Value, String> {
    let candidates = extract_json_candidates(text);
    if candidates.is_empty() {
        return Err("no JSON object found in the author's output".into());
    }
    let mut last_err = String::new();
    for cand in candidates {
        let parsed: Value = match serde_json::from_str(&cand) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("JSON parse failed: {e}");
                continue;
            }
        };
        let spec: workflow::Workflow = match serde_json::from_value(parsed.clone()) {
            Ok(s) => s,
            Err(e) => {
                last_err = format!("workflow schema mismatch: {e}");
                continue;
            }
        };
        match workflow::compile(spec) {
            Ok(_) => return Ok(parsed),
            Err(e) => {
                last_err = format!("workflow cross-validation failed: {e}");
            }
        }
    }
    Err(last_err)
}

fn extract_json_candidates(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Strategy 1: fenced ```json ... ``` blocks. Most LLMs wrap.
    let mut remaining = text;
    while let Some(fence_start) = remaining.find("```json") {
        let after = &remaining[fence_start + "```json".len()..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(fence_end) = body.find("```") {
            out.push(body[..fence_end].trim().to_string());
            remaining = &body[fence_end + 3..];
        } else {
            break;
        }
    }
    // Strategy 2: bare ``` blocks (no language tag).
    let mut remaining = text;
    while let Some(fence_start) = remaining.find("```") {
        let after = &remaining[fence_start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(fence_end) = body.find("```") {
            let candidate = body[..fence_end].trim();
            if candidate.starts_with('{') {
                out.push(candidate.to_string());
            }
            remaining = &body[fence_end + 3..];
        } else {
            break;
        }
    }
    // Strategy 3: first `{` to last `}` of the whole text.
    if let Some(first) = text.find('{') {
        if let Some(last) = text.rfind('}') {
            if last > first {
                out.push(text[first..=last].to_string());
            }
        }
    }
    // Dedup while preserving order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

// ---------------------------------------------------------------------------
// Helper methods on BlackboxServer
// ---------------------------------------------------------------------------

impl BlackboxServer {
    fn resolve_team_advisor_config(
        &self,
        advisor: Option<&AdvisorSpecParams>,
        store_dir: &Path,
        project_dir: Option<&str>,
    ) -> Result<Option<orchestration::team::TeamAdvisorConfig>, String> {
        let Some(advisor) = advisor else {
            return Ok(None);
        };
        if advisor.charter.trim().is_empty() {
            return Err("advisor.charter is required and cannot be empty".into());
        }
        let brofile =
            orchestration::brofile::resolve_brofile(&advisor.brofile, store_dir, project_dir)
                .ok_or_else(|| format!("Brofile not found: {}", advisor.brofile))?;
        if !brofile.provider.supports_resume() {
            return Err(format!(
                "Advisor brofile {} uses provider {} which does not support resume",
                advisor.brofile, brofile.provider
            ));
        }
        Ok(Some(orchestration::team::TeamAdvisorConfig {
            brofile: advisor.brofile.clone(),
            alias: advisor.alias.clone(),
            charter: advisor.charter.clone(),
            context: advisor.context.clone(),
            halt_conditions: advisor.halt_conditions.clone().unwrap_or_default(),
            exit_conditions: advisor.exit_conditions.clone().unwrap_or_default(),
            packet_id: advisor.packet_id.clone(),
            timeout_seconds: advisor.timeout_seconds,
            mode: advisor.mode.unwrap_or_default(),
        }))
    }

    fn build_team_advisor_init_prompt(
        &self,
        team: &orchestration::team::Team,
        advisor: &orchestration::team::TeamAdvisor,
    ) -> String {
        let member_list = team
            .members
            .iter()
            .map(|m| format!("- {} ({})", m.name, m.brofile))
            .collect::<Vec<_>>()
            .join("\n");
        let halt_list = if advisor.config.halt_conditions.is_empty() {
            "- none declared".to_string()
        } else {
            advisor
                .config
                .halt_conditions
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let exit_list = if advisor.config.exit_conditions.is_empty() {
            "- none declared".to_string()
        } else {
            advisor
                .config
                .exit_conditions
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let context = advisor.config.context.as_deref().unwrap_or("(none)");
        let packet_id = advisor.config.packet_id.as_deref().unwrap_or("(none)");
        format!(
            "You are the advisor for team \"{team_name}\".\n\n\
Role:\n\
- monitor big-picture progression only\n\
- stay out of code-level execution unless explicitly asked\n\
- use the charter, halt conditions, exit conditions, and packet result to steer\n\
- when the checkpoint indicates drift/blockage/exit, say so plainly\n\n\
Team members:\n{member_list}\n\n\
Charter:\n{charter}\n\n\
Context:\n{context}\n\n\
Halt conditions:\n{halt_list}\n\n\
Exit conditions:\n{exit_list}\n\n\
Compiled packet for mechanical evaluation:\n- {packet_id}\n\n\
From now on, you will receive structured checkpoint updates after wait boundaries.\n\
Respond tersely with:\n\
Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO\n\
Rationale: <1-3 sentences>\n\
Next step: <one concrete steering suggestion>\n",
            team_name = team.name,
            member_list = member_list,
            charter = advisor.config.charter,
            context = context,
            halt_list = halt_list,
            exit_list = exit_list,
            packet_id = packet_id,
        )
    }

    async fn dispatch_team_advisor_prompt(
        &self,
        team: &mut orchestration::team::Team,
        prompt: String,
    ) -> Result<(Arc<orch::Task>, Option<f64>), String> {
        let advisor = match team.advisor.as_mut() {
            Some(a) => a,
            None => return Err("team has no advisor configured".into()),
        };
        let store_dir = self.state.store_dir.clone();
        let brofile = orchestration::brofile::resolve_brofile(
            &advisor.config.brofile,
            &store_dir,
            team.project_dir.as_deref(),
        )
        .ok_or_else(|| format!("Brofile not found: {}", advisor.config.brofile))?;
        let provider = brofile.provider;
        let env_overrides = orchestration::brofile::resolve_provider_env(
            provider,
            brofile.account.as_deref(),
            brofile.model.as_deref(),
            &store_dir,
        );
        let exec_opts = if brofile.model.is_some() || brofile.effort.is_some() {
            Some(ExecOpts {
                model: brofile.model.clone(),
                effort: brofile.effort.clone(),
            })
        } else {
            None
        };
        let task_id = uuid::Uuid::new_v4().to_string();
        let timeout = advisor.config.timeout_seconds;
        let cwd = team.project_dir.clone();
        let task = match advisor.session_id.as_deref() {
            Some("pending") => {
                return Err(format!(
                    "Advisor {} is still waiting for session discovery; refusing to launch a second session",
                    advisor.name
                ));
            }
            Some(session_id) => {
                let ambient_ctx = orch::AmbientContext {
                    task_id: Some(task_id.clone()),
                    session_id: Some(session_id.to_string()),
                    project_dir: cwd.clone(),
                    bro_name: Some(advisor.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(advisor.name.as_str()),
                        Some(session_id),
                        None,
                        None,
                    ),
                    completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
                    allow_recursion: false,
                    provider: Some(provider),
                };
                let wrapped_prompt = orch::apply_ambient(&prompt, &ambient_ctx);
                let mut args =
                    provider.build_resume_args(session_id, &wrapped_prompt, exec_opts.as_ref());
                let dispatch_filters = resolve_dispatch_filters(
                    provider,
                    cwd.as_deref(),
                    false,
                    &task_id,
                    brofile.filters.as_ref(),
                );
                args.extend(dispatch_filters.args);
                let task = orch::spawn_task(
                    task_id.clone(),
                    provider,
                    args,
                    session_id.to_string(),
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                );
                cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
                task
            }
            None => {
                let session_id = if matches!(provider, Provider::Claude) {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    "pending".into()
                };
                let ambient_ctx = orch::AmbientContext {
                    task_id: Some(task_id.clone()),
                    session_id: Some(session_id.clone()),
                    project_dir: cwd.clone(),
                    bro_name: Some(advisor.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(advisor.name.as_str()),
                        Some(session_id.as_str()),
                        None,
                        None,
                    ),
                    completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
                    allow_recursion: false,
                    provider: Some(provider),
                };
                let wrapped_prompt = orch::apply_brofile_lens(
                    &orch::apply_ambient(&prompt, &ambient_ctx),
                    brofile.lens.as_deref(),
                );
                let mut args = provider.build_exec_args(
                    &wrapped_prompt,
                    &session_id,
                    cwd.as_deref(),
                    exec_opts.as_ref(),
                );
                let dispatch_filters = resolve_dispatch_filters(
                    provider,
                    cwd.as_deref(),
                    false,
                    &task_id,
                    brofile.filters.as_ref(),
                );
                args.extend(dispatch_filters.args);
                let task = orch::spawn_task(
                    task_id.clone(),
                    provider,
                    args,
                    session_id,
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                );
                cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
                task
            }
        };

        advisor.task_history.push(task_id);
        advisor.session_id = Some(task.inner.lock().session_id.clone());
        orchestration::team::save_team(team, &self.state.store_dir);
        Ok((task, timeout))
    }

    fn persist_advisor_session_to_team(&self, team_name: &str, task: &Arc<orch::Task>) {
        let Some(mut team) = orchestration::team::load_team(team_name, &self.state.store_dir)
        else {
            return;
        };
        let Some(advisor) = team.advisor.as_mut() else {
            return;
        };
        let session_id = task.inner.lock().session_id.clone();
        if session_id != "pending" {
            advisor.session_id = Some(session_id);
            orchestration::team::save_team(&team, &self.state.store_dir);
        }
    }

    async fn await_team_advisor_task(
        &self,
        team_name: &str,
        task: Arc<orch::Task>,
        timeout: Option<f64>,
    ) -> Result<Value, String> {
        let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
        self.persist_advisor_session_to_team(team_name, &task);
        Ok(if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        })
    }

    async fn initialize_team_advisor(
        &self,
        team: &mut orchestration::team::Team,
    ) -> Result<(), String> {
        let Some(advisor) = team.advisor.as_ref() else {
            return Ok(());
        };
        if advisor
            .session_id
            .as_deref()
            .filter(|s| *s != "pending")
            .is_some()
        {
            return Ok(());
        }
        let prompt = self.build_team_advisor_init_prompt(team, advisor);
        let team_name = team.name.clone();
        let (task, timeout) = self.dispatch_team_advisor_prompt(team, prompt).await?;
        let _ = self
            .await_team_advisor_task(&team_name, task, timeout)
            .await?;
        Ok(())
    }

    fn summarize_notes_for_tasks(&self, task_ids: &[String]) -> AdvisorNoteSummary {
        use notes::{NoteKind, NoteResolution};

        let mut summary = AdvisorNoteSummary::default();
        let task_set: std::collections::HashSet<&str> =
            task_ids.iter().map(String::as_str).collect();
        let mut recent_unresolved = Vec::new();

        for note in self.state.notes.read().all().iter().rev() {
            let Some(task_id) = note.task_id.as_deref() else {
                continue;
            };
            if !task_set.contains(task_id) {
                continue;
            }
            match note.kind {
                NoteKind::Dispute => summary.dispute_count += 1,
                NoteKind::Assumption => summary.assumption_count += 1,
                NoteKind::Surprise => summary.surprise_count += 1,
                NoteKind::Followup => summary.followup_count += 1,
                NoteKind::Blocked => summary.blocked_count += 1,
                NoteKind::Learned => summary.learned_count += 1,
                NoteKind::Done => summary.done_count += 1,
            }
            if note.resolution == NoteResolution::Unresolved && recent_unresolved.len() < 5 {
                recent_unresolved.push(format!("{}: {}", note.kind.as_ref(), note.body));
            }
        }
        summary.recent_unresolved = recent_unresolved;
        summary
    }

    fn build_advisor_checkpoint(
        &self,
        team: &orchestration::team::Team,
        wait_kind: &str,
        results: &[Value],
    ) -> AdvisorCheckpoint {
        let monitored_task_ids: Vec<String> = results
            .iter()
            .filter_map(|r| {
                r.get("taskId")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .collect();
        let notes = self.summarize_notes_for_tasks(&monitored_task_ids);
        let mut members = Vec::new();
        let mut completed_count = 0usize;
        let mut failed_count = 0usize;
        let mut cancelled_count = 0usize;
        let mut timed_out_count = 0usize;
        let mut running_count = 0usize;

        for result in results {
            let status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let timed_out = result.get("timed_out").is_some();
            if timed_out {
                timed_out_count += 1;
                running_count += 1;
            } else {
                match status.as_str() {
                    "completed" | "Completed" => completed_count += 1,
                    "failed" | "Failed" => failed_count += 1,
                    "cancelled" | "Cancelled" => cancelled_count += 1,
                    _ => running_count += 1,
                }
            }
            let result_snippet = result
                .get("result")
                .and_then(Value::as_str)
                .map(|s| s.trim().replace('\n', " "))
                .filter(|s| !s.is_empty())
                .map(|s| {
                    if s.len() > 160 {
                        format!("{}…", &s[..160])
                    } else {
                        s
                    }
                })
                .or_else(|| {
                    result
                        .get("lastAssistantSnippet")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                });
            members.push(AdvisorMemberCheckpoint {
                bro: result
                    .get("bro")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                task_id: result
                    .get("taskId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                status,
                timed_out,
                keep_going: result
                    .get("keep_going")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                result_snippet,
            });
        }

        AdvisorCheckpoint {
            wait_kind: wait_kind.to_string(),
            team_name: team.name.clone(),
            teamplate: team.teamplate.clone(),
            packet_id: team
                .advisor
                .as_ref()
                .and_then(|a| a.config.packet_id.clone()),
            monitored_task_ids,
            total_count: results.len(),
            completed_count,
            failed_count,
            cancelled_count,
            timed_out_count,
            running_count,
            dispute_count: notes.dispute_count,
            assumption_count: notes.assumption_count,
            surprise_count: notes.surprise_count,
            followup_count: notes.followup_count,
            blocked_count: notes.blocked_count,
            learned_count: notes.learned_count,
            done_count: notes.done_count,
            members,
            notes,
        }
    }

    fn apply_advisor_packet(
        &self,
        packet_id: &str,
        checkpoint: &AdvisorCheckpoint,
    ) -> Result<Value, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store.load(packet_id).map_err(|e| format!("{e:#}"))?;
        let entity = serde_json::to_value(checkpoint).map_err(|e| e.to_string())?;
        let prediction = apply_packet_with(&packet, &entity, &*packet_store);
        Ok(match prediction {
            Some(prediction) => json!({
                "packetId": packet.id,
                "match": true,
                "ruleId": prediction.rule_id,
                "classification": prediction.classification,
                "consequent": prediction.consequent,
                "confidence": prediction.confidence,
            }),
            None => json!({
                "packetId": packet.id,
                "match": false,
            }),
        })
    }

    async fn maybe_resume_team_advisor(
        &self,
        team_name: &str,
        wait_kind: &str,
        results: &[Value],
    ) -> Result<Option<Value>, String> {
        let mut team = match orchestration::team::load_team(team_name, &self.state.store_dir) {
            Some(team) => team,
            None => return Ok(None),
        };
        let Some(advisor) = team.advisor.as_ref() else {
            return Ok(None);
        };
        let checkpoint = self.build_advisor_checkpoint(&team, wait_kind, results);
        let packet_eval = match advisor.config.packet_id.as_deref() {
            Some(packet_id) => Some(self.apply_advisor_packet(packet_id, &checkpoint)?),
            None => None,
        };
        let checkpoint_json =
            serde_json::to_string_pretty(&checkpoint).map_err(|e| e.to_string())?;
        let packet_section = packet_eval
            .as_ref()
            .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
            .unwrap_or_else(|| "{\n  \"configured\": false\n}".to_string());
        let prompt = format!(
            "Team wait checkpoint.\n\n\
Checkpoint entity:\n{checkpoint_json}\n\n\
Mechanical packet evaluation:\n{packet_section}\n\n\
Interpret the checkpoint against the charter and respond with:\n\
Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO\n\
Rationale: <1-3 sentences>\n\
Next step: <one concrete steering suggestion>\n"
        );
        let advisor_mode = advisor.config.mode;
        let team_name_owned = team.name.clone();
        let (task, timeout) = self.dispatch_team_advisor_prompt(&mut team, prompt).await?;
        let advisor_result = match advisor_mode {
            orchestration::team::AdvisorMode::Blocking => {
                let result = self
                    .await_team_advisor_task(&team_name_owned, task.clone(), timeout)
                    .await?;
                json!({
                    "mode": "blocking",
                    "taskId": task.id(),
                    "result": result,
                })
            }
            orchestration::team::AdvisorMode::Background => {
                let server = self.clone();
                let team_name = team_name_owned.clone();
                let task_clone = task.clone();
                tokio::spawn(async move {
                    let _ = server
                        .await_team_advisor_task(&team_name, task_clone, timeout)
                        .await;
                });
                let inner = task.inner.lock();
                json!({
                    "mode": "background",
                    "scheduled": true,
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "running",
                })
            }
        };
        Ok(Some(json!({
            "checkpoint": checkpoint,
            "packet": packet_eval,
            "advisor": advisor_result,
        })))
    }

    #[allow(clippy::type_complexity)]
    fn resolve_exec_target(
        &self,
        bro_name: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            match orchestration::team::resolve_bro_selector(name, &teams)? {
                Some(bro_match) => {
                    let member = &bro_match.team.members[bro_match.member_idx];
                    let bf = orchestration::brofile::resolve_brofile(
                        &member.brofile,
                        store_dir,
                        bro_match.team.project_dir.as_deref(),
                    )
                    .ok_or(format!("Brofile not found: {}", member.brofile))?;
                    let env = orchestration::brofile::resolve_provider_env(
                        bf.provider,
                        bf.account.as_deref(),
                        bf.model.as_deref(),
                        store_dir,
                    );
                    let opts = if bf.model.is_some() || bf.effort.is_some() {
                        Some(ExecOpts {
                            model: bf.model.clone(),
                            effort: bf.effort.clone(),
                        })
                    } else {
                        None
                    };
                    let cwd = project_dir
                        .map(String::from)
                        .or(bro_match.team.project_dir.clone());
                    return Ok((bf.provider, bf.lens, opts, env, cwd, bf.filters));
                }
                None => {
                    // Standalone brofile fallback
                }
            }
            let bf = orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                .ok_or(format!("Unknown bro or brofile: {name}"))?;
            let env = orchestration::brofile::resolve_provider_env(
                bf.provider,
                bf.account.as_deref(),
                bf.model.as_deref(),
                store_dir,
            );
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            return Ok((
                bf.provider,
                bf.lens,
                opts,
                env,
                project_dir.map(String::from),
                bf.filters,
            ));
        }

        if let Some(p) = raw_provider {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            let env = orchestration::brofile::resolve_provider_env(provider, None, None, store_dir);
            return Ok((
                provider,
                None,
                None,
                env,
                project_dir.map(String::from),
                None,
            ));
        }

        Err("Provide either bro or provider".into())
    }

    #[allow(clippy::type_complexity)]
    fn resolve_resume_target(
        &self,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            String,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            let bro_match = orchestration::team::resolve_bro_selector(name, &teams)?
                .ok_or_else(|| {
                    if orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                        .is_some()
                    {
                        format!(
                            "Brofile \"{name}\" is not in a team — use exec first or provide session_id + provider"
                        )
                    } else {
                        format!("Unknown bro: {name}")
                    }
                })?;
            let member = &bro_match.team.members[bro_match.member_idx];
            let sid = member
                .session_id
                .as_deref()
                .filter(|s| *s != "pending")
                .ok_or(format!(
                    "Bro \"{name}\" has no active session — use exec first"
                ))?;
            let bf = orchestration::brofile::resolve_brofile(
                &member.brofile,
                store_dir,
                bro_match.team.project_dir.as_deref(),
            )
            .ok_or(format!("Brofile not found: {}", member.brofile))?;
            let env = orchestration::brofile::resolve_provider_env(
                bf.provider,
                bf.account.as_deref(),
                bf.model.as_deref(),
                store_dir,
            );
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            let cwd = project_dir
                .map(String::from)
                .or(bro_match.team.project_dir.clone());
            return Ok((
                bf.provider,
                sid.to_string(),
                bf.lens,
                opts,
                env,
                cwd,
                bf.filters,
            ));
        }

        if let (Some(sid), Some(p)) = (session_id, raw_provider) {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            let env = orchestration::brofile::resolve_provider_env(provider, None, None, store_dir);
            return Ok((
                provider,
                sid.to_string(),
                None,
                None,
                env,
                project_dir.map(String::from),
                None,
            ));
        }

        Err("Provide either bro or session_id + provider".into())
    }

    fn resolve_when_targets(
        &self,
        team_name: Option<&str>,
        task_ids: Option<&[String]>,
    ) -> Result<Vec<String>, String> {
        if let Some(name) = team_name {
            let team = orchestration::team::load_team(name, &self.state.store_dir)
                .ok_or(format!("Unknown team: {name}"))?;
            let ids: Vec<String> = team
                .members
                .iter()
                .filter_map(|m| m.task_history.last().cloned())
                .collect();
            if ids.is_empty() {
                return Err(format!("No tasks found for team {name}"));
            }
            return Ok(ids);
        }
        if let Some(ids) = task_ids {
            if ids.is_empty() {
                return Err("Empty task_ids array".into());
            }
            return Ok(ids.to_vec());
        }
        Err("Provide either team or task_ids".into())
    }

    fn record_task_to_bro(&self, bro_name: &str, task: &Arc<orch::Task>) {
        // Stamp the task with a default label up-front so brofile-only
        // dispatches (no team match) still surface in `bro tail` with a
        // name. Team-attributed dispatches will overwrite below with a
        // more precise `<team>::<member>` label.
        task.inner.lock().bro_label = Some(bro_name.to_string());

        let _lock = orchestration::team::lock_teams();
        let tid = task.id();
        let teams = orchestration::team::load_all_teams(&self.state.store_dir);
        let Ok(bro_match_opt) = orchestration::team::resolve_bro_selector(bro_name, &teams) else {
            return;
        };
        let Some(bro_match) = bro_match_opt else {
            return;
        };
        let target_team = bro_match.team.name.clone();
        let target_member_idx = bro_match.member_idx;
        let task_sid = task.inner.lock().session_id.clone();

        for mut team in teams {
            if team.name != target_team {
                continue;
            }
            let member = &mut team.members[target_member_idx];
            member.task_history.push(tid.clone());
            // Track the latest launch immediately, including "pending",
            // so later team rounds fail closed instead of starting a
            // second session before provider-side discovery completes.
            member.session_id = Some(task_sid.clone());
            // Stamp a precise team::member label on the task so the
            // tail handler can attribute even when later resolution
            // (find_bro_ref_for_task) hits the duplicate-name
            // ambiguity case (two team members sharing a brofile).
            task.inner.lock().bro_label = Some(format!("{}::{}", team.name, member.name));
            orchestration::team::save_team(&team, &self.state.store_dir);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Blackbox: unified transcript search, knowledge management, and multi-provider agent orchestration")
    }
}

// ---------------------------------------------------------------------------
// Bro roster endpoint — resolves selectors to concrete per-bro lane info
// (provider, session_id, transcript file path). Consumed by `bro tail`
// to know WHICH JSONL files to open and follow.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RosterQuery {
    /// Comma-separated bro names (union of matches across all teams)
    #[serde(default)]
    bros: Option<String>,
    /// Comma-separated team names (each contributes all members). Accepts
    /// legacy `team=` singular form as an alias.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated session IDs — synthetic adhoc lanes bypassing team membership.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names (claude/codex/gemini/copilot/vibe) — final filter.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

#[derive(Debug, Serialize)]
struct BroRosterEntry {
    bro: String,
    bro_selector: String,
    team: String,
    provider: String,
    account: Option<String>,
    session_id: Option<String>,
    jsonl_path: Option<String>,
    brofile: String,
    model: Option<String>,
}

fn split_csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn infer_provider_from_path(path: &Path) -> Option<Provider> {
    let s = path.to_string_lossy();
    if s.contains("/.codex/sessions/") {
        Some(Provider::Codex)
    } else if s.contains("/.gemini/tmp/") {
        Some(Provider::Gemini)
    } else if s.contains("/.copilot/session-state/") {
        Some(Provider::Copilot)
    } else if s.contains("/.vibe/logs/session/") {
        Some(Provider::Vibe)
    } else if s.contains("/projects/") {
        Some(Provider::Claude)
    } else {
        None
    }
}

fn build_member_entry(
    team: &orchestration::team::Team,
    member: &orchestration::team::TeamMember,
    store_dir: &Path,
    config: &index::ReindexConfig,
) -> BroRosterEntry {
    let brofile = orchestration::brofile::resolve_brofile(
        &member.brofile,
        store_dir,
        team.project_dir.as_deref(),
    );
    let provider = brofile.as_ref().map(|b| b.provider);
    let session_id = member
        .session_id
        .as_ref()
        .filter(|s| s.as_str() != "pending")
        .cloned();
    let jsonl_path = session_id
        .as_deref()
        .and_then(|sid| index::find_session_file(sid, &config.roots, config.codex_root.as_deref()))
        .map(|p| p.to_string_lossy().into_owned());
    BroRosterEntry {
        bro: member.name.clone(),
        bro_selector: format!("{}::{}", team.name, member.name),
        team: team.name.clone(),
        provider: provider
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into()),
        account: brofile.as_ref().and_then(|b| {
            orchestration::brofile::effective_account(b.provider, b.account.as_deref(), store_dir)
        }),
        session_id,
        jsonl_path,
        brofile: member.brofile.clone(),
        model: brofile.and_then(|b| b.model),
    }
}

fn roster_entry_key(entry: &BroRosterEntry) -> String {
    if let Some(ref sid) = entry.session_id {
        format!("session::{sid}")
    } else {
        format!("member::{}", entry.bro_selector)
    }
}

/// Request body for POST `/orchestrate`. `workflow` is the parsed
/// workflow spec; `project_dir` is the working directory to pass to all
/// dispatched bros; `max_steps` caps the loop (defaults to 50 server-side).
#[derive(Debug, Deserialize)]
struct OrchestrateRequest {
    workflow: workflow::Workflow,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    /// When true, parse + cross-validate the workflow and return a
    /// textual plan — do not dispatch any bros. Intended as a pre-flight
    /// check before the real run.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct OrchestrateStatusQuery {
    thread_id: String,
}

#[derive(Debug, Serialize)]
struct OrchestrateStatusResponse {
    thread_id: String,
    notes: Vec<Value>,
    /// Most recent `ANCHOR [...]` body — the rolling compaction summary
    /// emitted at each step boundary.
    latest_anchor: Option<String>,
}

#[derive(Debug, Serialize)]
struct OrchestrateListEntry {
    thread_id: String,
    name: Option<String>,
    topic: String,
    status: String,
    created_at: String,
    last_activity: String,
    project: Option<String>,
    latest_anchor: Option<String>,
    final_status: Option<String>,
    note_count: usize,
}

async fn orchestrate_list_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    // Snapshot threads + notes via the raw stores so we don't hold
    // any parking_lot guard across an await (there aren't any awaits
    // in this handler, but the pattern is still cleaner).
    let entries: Vec<OrchestrateListEntry> = {
        let threads = state.threads.read();
        let notes = state.notes.read();
        let mut out: Vec<OrchestrateListEntry> = threads
            .all()
            .iter()
            .filter(|t| {
                matches!(t.kind, Some(crate::threads::ThreadKind::WorkItem))
                    && t.name.as_deref().is_some_and(|n| n.starts_with("wf-"))
            })
            .map(|t| {
                let tid = &t.id;
                let mut latest_anchor: Option<(String, String)> = None;
                let mut final_status: Option<String> = None;
                let mut note_count = 0usize;
                for n in notes.all() {
                    if n.thread_id.as_deref() != Some(tid.as_str()) {
                        continue;
                    }
                    note_count += 1;
                    let body = n.body.as_str();
                    if body.starts_with("ANCHOR ") {
                        let is_newer = latest_anchor
                            .as_ref()
                            .map(|(ts, _)| n.created_at.as_str() > ts.as_str())
                            .unwrap_or(true);
                        if is_newer {
                            latest_anchor = Some((n.created_at.clone(), body.to_string()));
                        }
                    }
                    if body.starts_with("workflow ") && body.contains("completed in") {
                        final_status = Some("completed".into());
                    } else if body.starts_with("workflow errored") {
                        final_status = Some("errored".into());
                    } else if body.starts_with("paused at user node") {
                        final_status = Some("paused".into());
                    } else if body.starts_with("policy halt") {
                        final_status = Some("policy_halt".into());
                    }
                }
                OrchestrateListEntry {
                    thread_id: t.id.clone(),
                    name: t.name.clone(),
                    topic: t.topic.clone(),
                    status: t.status.as_ref().to_string(),
                    created_at: t.created_at.clone(),
                    last_activity: t.last_activity.clone(),
                    project: if t.project.is_empty() {
                        None
                    } else {
                        Some(t.project.clone())
                    },
                    latest_anchor: latest_anchor.map(|(_, b)| b),
                    final_status,
                    note_count,
                }
            })
            .collect();
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        out
    };
    axum::Json(entries).into_response()
}

#[derive(Debug, Deserialize)]
struct OrchestratePeekQuery {
    /// Optional thread_id filter — when set, return only that arc's
    /// snapshot. When absent, return all running_arcs entries.
    #[serde(default)]
    thread_id: Option<String>,
}

async fn orchestrate_peek_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(q): Query<OrchestratePeekQuery>,
) -> impl axum::response::IntoResponse {
    let map = state.running_arcs.read();
    match q.thread_id {
        Some(tid) => match map.get(&tid) {
            Some(s) => axum::Json(serde_json::to_value(s).unwrap_or_default()),
            None => axum::Json(serde_json::json!({
                "error": format!("no arc snapshot for thread_id={tid}")
            })),
        },
        None => {
            let mut all: Vec<&ArcSnapshot> = map.values().collect();
            all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            axum::Json(serde_json::to_value(&all).unwrap_or_default())
        }
    }
}

async fn orchestrate_status_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(q): Query<OrchestrateStatusQuery>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    // Snapshot notes linked to this thread via the raw store.
    let entries: Vec<Value> = {
        let store = state.notes.read();
        store
            .all()
            .iter()
            .filter(|n| n.thread_id.as_deref() == Some(q.thread_id.as_str()))
            .map(|n| serde_json::to_value(n).unwrap_or_default())
            .collect()
    };
    let latest_anchor = entries
        .iter()
        .filter(|e| {
            e.get("body")
                .and_then(Value::as_str)
                .map(|b| b.starts_with("ANCHOR "))
                .unwrap_or(false)
        })
        .max_by_key(|e| {
            e.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .and_then(|e| e.get("body").and_then(Value::as_str).map(String::from));
    axum::Json(OrchestrateStatusResponse {
        thread_id: q.thread_id,
        notes: entries,
        latest_anchor,
    })
    .into_response()
}

async fn orchestrate_stream_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateRequest>,
) -> axum::response::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::Event;
    use axum::response::Sse;
    let compiled = workflow::compile(req.workflow);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

    // Kick off the run on a background task; events stream via tx.
    tokio::spawn(async move {
        let state_clone = state.clone();
        let server = BlackboxServer::new(state_clone);
        match compiled {
            Err(e) => {
                let _ = tx.send(json!({
                    "kind": "compile_error",
                    "data": {"message": e.to_string()},
                    "timestamp": crate::util::now_iso(),
                }));
            }
            Ok(compiled) => {
                let result = workflow::run_workflow_streaming(
                    &server,
                    &compiled,
                    req.project_dir,
                    req.max_steps,
                    tx.clone(),
                )
                .await;
                // Terminal frame: the full result. Clients should
                // detect `kind: "result"` as end-of-run.
                let _ = tx.send(json!({
                    "kind": "result",
                    "data": result,
                    "timestamp": crate::util::now_iso(),
                }));
            }
        }
        // tx dropped here closes the stream.
    });

    let stream = async_stream::stream! {
        while let Some(ev) = rx.recv().await {
            let s = ev.to_string();
            yield Ok::<_, std::convert::Infallible>(Event::default().data(s));
        }
    };
    Sse::new(stream)
}

async fn orchestrate_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateRequest>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let compiled = match workflow::compile(req.workflow) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile failed: {e}"),
            )
                .into_response();
        }
    };
    if req.dry_run {
        return axum::Json(workflow::engine::dry_run(&compiled)).into_response();
    }
    let server = BlackboxServer::new(state);
    let result = workflow::run_workflow(&server, &compiled, req.project_dir, req.max_steps).await;
    axum::Json(result).into_response()
}

/// HTTP webhook ingestion endpoint. URL: `POST /webhook/:name`.
///
/// Pipeline (in order):
///   1. Look up WebhookSpec by name (404 if unknown)
///   2. Verify signature scheme against headers + raw body
///   3. Optional delivery-id dedup (Forgejo: X-Gitea-Delivery)
///   4. Run extractor over payload → flat entity
///   5. Apply routing packet → RoutingVerdict
///   6. Dispatch verdict (start_arc | signal_arc | cancel_arc | ignore | dead_letter)
async fn webhook_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let header_map = headers_to_lowercase_map(&headers);
    let header_subset = header_subset_for_log(&header_map);
    let body_bytes: &[u8] = &body;
    let outcome = process_webhook(&state, &name, &header_map, body_bytes).await;
    let (status, response_body) = match &outcome {
        Ok(v) => (200u16, v.clone()),
        Err(e) => (400u16, json!({"error": e.to_string()})),
    };
    let entity = response_body
        .get("extracted_entity")
        .cloned()
        .unwrap_or(Value::Null);
    let verdict_classification = response_body
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if status == 200 {
                "unknown".into()
            } else {
                "error".into()
            }
        });
    state.record_webhook(WebhookDelivery {
        received_at: util::now_iso(),
        webhook_name: name.clone(),
        source: "webhook".into(),
        headers: header_subset,
        extracted_entity: entity,
        verdict_classification,
        response_status: status,
        response_body: response_body.clone(),
    });
    match outcome {
        Ok(verdict_json) => (axum::http::StatusCode::OK, axum::Json(verdict_json)).into_response(),
        Err(e) => {
            tracing::warn!("webhook /{name}: {e}");
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("webhook error: {e}"),
            )
                .into_response()
        }
    }
}

/// Replay an arbitrary payload through a webhook's extractor + routing
/// packet WITHOUT dispatching the verdict. Returns the extracted entity
/// + routing verdict so authors can debug without firing arcs.
/// URL: `POST /webhook/:name/replay`. Skips signature verification.
async fn webhook_replay_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    let header_map = headers_to_lowercase_map(&headers);
    use axum::response::IntoResponse;
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("payload not JSON: {e}"),
            )
                .into_response();
        }
    };
    match webhook_replay_inner(&state, &name, &payload, &header_map) {
        Ok(response_body) => axum::Json(response_body).into_response(),
        Err((status, msg)) => (status, msg).into_response(),
    }
}

/// Shared replay path used by both the HTTP `/webhook/:name/replay`
/// endpoint and the `bro_webhook_replay` MCP tool. Records the result
/// into the delivery ring buffer with `source: replay`.
fn webhook_replay_inner(
    state: &Arc<SharedState>,
    name: &str,
    payload: &Value,
    headers: &HashMap<String, String>,
) -> Result<Value, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let spec = state
        .webhooks
        .get(name)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown webhook '{name}'")))?;
    let combined = combine_payload_and_headers(payload, headers);
    let entity = spec
        .extractor
        .extract(&combined)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("extractor failed: {e}")))?;
    let prediction = {
        let store = state.packets.read();
        let packet = store.load(&spec.routing_packet).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("routing packet load: {e}"),
            )
        })?;
        apply_packet_with(&packet, &entity, &*store)
    };
    let verdict_kind = prediction
        .as_ref()
        .map(|p| p.classification.clone())
        .unwrap_or_else(|| "no_match".into());
    let verdict = prediction.map(|p| p.consequent.to_json());
    let response_body = json!({
        "entity": entity.clone(),
        "verdict_classification": verdict_kind.clone(),
        "verdict_consequent": verdict,
    });
    state.record_webhook(WebhookDelivery {
        received_at: util::now_iso(),
        webhook_name: name.to_string(),
        source: "replay".into(),
        headers: header_subset_for_log(headers),
        extracted_entity: entity,
        verdict_classification: verdict_kind,
        response_status: 200,
        response_body: response_body.clone(),
    });
    Ok(response_body)
}

/// Subset of inbound headers preserved in the webhook delivery log.
/// Lowercased `x-*` headers carry the routing-relevant signal (event
/// type, delivery id, signature header). Bulk Forgejo/GitHub
/// boilerplate (`accept`, `user-agent`, `content-length`) and the
/// signature value itself are dropped — keeps the buffer small and
/// avoids leaking signature bytes into the read surface.
fn header_subset_for_log(headers: &HashMap<String, String>) -> serde_json::Map<String, Value> {
    headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-"))
        .filter(|(k, _)| !k.contains("signature"))
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect()
}

fn headers_to_lowercase_map(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_lowercase(), s.to_string()))
        })
        .collect()
}

/// Wrap a webhook body into a single Value that the Extractor can
/// project from. Body fields stay at the top level (so canonical
/// `$.action` / `$.pull_request.number` paths work) and headers are
/// available under `$._headers.<name>` for header-driven routing
/// (Forgejo's event type is in `X-Gitea-Event`, not the body).
fn combine_payload_and_headers(payload: &Value, headers: &HashMap<String, String>) -> Value {
    let mut map = match payload {
        Value::Object(m) => m.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("_payload".into(), other.clone());
            m
        }
    };
    let header_obj: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    map.insert("_headers".into(), Value::Object(header_obj));
    Value::Object(map)
}

/// True iff the bind host string resolves to a loopback address.
/// Recognized: `127.0.0.0/8` literals, `localhost` (string match —
/// resolution is host-config dependent and we keep it conservative),
/// `::1`. `0.0.0.0` and any other IPv4 are treated as non-loopback.
fn is_loopback_bind(bind_host: &str) -> bool {
    let h = bind_host.trim();
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

async fn process_webhook(
    state: &Arc<SharedState>,
    name: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> anyhow::Result<Value> {
    let spec = state
        .webhooks
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown webhook '{name}'"))?;

    // Signature verification (loopback flag controls the `none`
    // scheme escape hatch — defense in depth alongside install_check).
    webhooks::verify_signature(&spec.signature, headers, body, state.bind_is_loopback)
        .map_err(|e| anyhow::anyhow!("signature: {e}"))?;

    // Delivery-id dedup (idempotency).
    let delivery_id = spec
        .delivery_id_header
        .as_deref()
        .and_then(|h| headers.get(&h.to_lowercase()))
        .map(|s| s.as_str());
    if !state.webhooks.check_delivery_id(name, delivery_id) {
        tracing::info!(
            "webhook '{name}': dropped duplicate delivery {:?}",
            delivery_id
        );
        return Ok(json!({"status": "duplicate_dropped"}));
    }

    let payload: Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("payload not JSON: {e}"))?;

    // Combined extractor input: payload fields at top level (so
    // ordinary Forgejo paths like `$.action`, `$.pull_request.number`
    // work) PLUS `_headers` for header-driven event-type routing.
    let combined = combine_payload_and_headers(&payload, headers);

    // Project payload via extractor.
    let entity = spec
        .extractor
        .extract(&combined)
        .map_err(|e| anyhow::anyhow!("extractor: {e}"))?;

    // Apply routing packet.
    let prediction = {
        let store = state.packets.read();
        let packet = store
            .load(&spec.routing_packet)
            .map_err(|e| anyhow::anyhow!("routing packet load: {e}"))?;
        apply_packet_with(&packet, &entity, &*store)
    };

    let consequent_json = match prediction {
        Some(p) => p.consequent.to_json(),
        None => {
            tracing::warn!(
                "webhook '{name}': routing packet '{}' produced no_match — dead-lettering. entity={}",
                spec.routing_packet,
                entity
            );
            return Ok(json!({
                "status": "no_match",
                "reason": "routing packet returned no_match (default → dead-letter)",
                "extracted_entity": entity,
            }));
        }
    };

    // Resolve `${entity.X}` references inside the routing verdict
    // (typed: `${entity.pr_number}` becomes `Number(117)`, not the
    // string `"117"`) so routing rules can carry typed correlation
    // tuples + payload selections without the rule author hand-
    // encoding entity scalars.
    let resolved_consequent = routing::resolve_entity_template(&entity, &consequent_json);
    let verdict = routing::RoutingVerdict::parse(&resolved_consequent)
        .map_err(|e| anyhow::anyhow!("verdict parse: {e}"))?;

    dispatch_verdict(
        state.clone(),
        &spec.name,
        spec.default_project_dir.clone(),
        verdict,
        entity,
    )
    .await
}

/// Apply a routing packet to an extracted entity and dispatch the
/// resulting verdict. The shared dispatch entry point used by every
/// event inlet (webhooks AND pollers) — both reduce to "I have an
/// entity + a routing-packet id, route it." Inlet-specific concerns
/// (signature verify, schedule, dedup) live in the caller.
pub(crate) async fn dispatch_routed_event(
    state: Arc<SharedState>,
    inlet_name: &str,
    routing_packet_id: &str,
    entity: Value,
    default_project_dir: Option<String>,
) -> anyhow::Result<Value> {
    let prediction = {
        let store = state.packets.read();
        let packet = store
            .load(routing_packet_id)
            .map_err(|e| anyhow::anyhow!("routing packet load: {e}"))?;
        apply_packet_with(&packet, &entity, &*store)
    };
    let consequent_json = match prediction {
        Some(p) => p.consequent.to_json(),
        None => {
            tracing::warn!(
                "{inlet_name}: routing packet '{routing_packet_id}' produced no_match — dead-lettering",
            );
            return Ok(json!({
                "status": "no_match",
                "reason": "routing packet returned no_match (default → dead-letter)",
                "extracted_entity": entity,
            }));
        }
    };
    let resolved_consequent = routing::resolve_entity_template(&entity, &consequent_json);
    let verdict = routing::RoutingVerdict::parse(&resolved_consequent)
        .map_err(|e| anyhow::anyhow!("verdict parse: {e}"))?;
    dispatch_verdict(state, inlet_name, default_project_dir, verdict, entity).await
}

/// Dispatch a pre-built RoutingVerdict directly, skipping the
/// routing-packet evaluation step. Used by the whiteboard transition
/// path: when a phase advances, the engine knows the verdict shape
/// (always `signal_arc { signal: "board-transitioned", correlate: ... }`),
/// no extractor or packet round-trip needed.
pub(crate) async fn dispatch_routing_verdict_direct(
    state: Arc<SharedState>,
    inlet_name: &str,
    verdict: routing::RoutingVerdict,
    entity: Value,
) -> anyhow::Result<Value> {
    dispatch_verdict(state, inlet_name, None, verdict, entity).await
}

async fn dispatch_verdict(
    state: Arc<SharedState>,
    inlet_name: &str,
    default_project_dir: Option<String>,
    verdict: routing::RoutingVerdict,
    entity: Value,
) -> anyhow::Result<Value> {
    use routing::RoutingVerdict;
    match verdict {
        RoutingVerdict::Ignore => Ok(json!({"status": "ignored"})),
        RoutingVerdict::DeadLetter { reason } => {
            tracing::warn!("{inlet_name}: dead-lettered (reason={:?})", reason);
            Ok(json!({
                "status": "dead_letter",
                "reason": reason,
                "extracted_entity": entity,
            }))
        }
        RoutingVerdict::SignalArc {
            signal,
            correlate,
            payload,
        } => {
            // Carry the routing verdict's payload (or, when absent,
            // the full extracted entity) through to the resumed wait
            // as `${last_signal.payload}`. Without this hooks like
            // `set_var feedback_text = ${last_signal.payload.review.body}`
            // would only see the correlation tuple.
            let signal_payload = payload.unwrap_or_else(|| entity.clone());
            let resolved = signal_arc_dispatch(&state, &signal, correlate, signal_payload).await;
            Ok(resolved)
        }
        RoutingVerdict::CancelArc { correlate } => {
            // Match running arcs whose pending-wait correlation is a
            // superset of `correlate`: every key in the verdict's
            // tuple must be present with the same value somewhere on
            // the arc's wait registrations. Empty correlate matches
            // every running arc (the broadcast-cancel form). Each
            // matching arc gets its CancellationToken tripped.
            let mut cancelled: Vec<String> = Vec::new();
            // Snapshot the wait store and find arc ids whose
            // registrations contain a tuple matching `correlate`.
            let snapshot = state.wait_store.snapshot();
            let mut matching_arc_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for w in snapshot {
                let matches = correlate.is_empty()
                    || correlate
                        .iter()
                        .all(|(k, v)| w.correlation.get(k) == Some(v));
                if matches {
                    matching_arc_ids.insert(w.arc_id);
                }
            }
            for arc_id in matching_arc_ids {
                if state.cancel_arc(&arc_id) {
                    cancelled.push(arc_id);
                }
            }
            Ok(json!({
                "status": "cancel_arc_dispatched",
                "cancelled_arcs": cancelled,
                "correlate": correlate,
            }))
        }
        RoutingVerdict::StartArc {
            workflow: workflow_id,
            initial_vars,
        } => {
            let registry = state.workflow_registry.clone();
            let spec_clone = {
                let map = registry.read();
                map.get(&workflow_id).cloned()
            };
            let workflow_spec = spec_clone.ok_or_else(|| {
                anyhow::anyhow!("start_arc verdict references unknown workflow id '{workflow_id}'")
            })?;
            let compiled = workflow::compile(workflow_spec)
                .map_err(|e| anyhow::anyhow!("workflow compile: {e}"))?;
            // Validate brofile/team capability composition against the
            // workflow's actor `requires` lists. Webhook ingress used
            // to skip this and let dispatch silently downgrade — fix
            // is to gate the spawn on the same check the MCP / HTTP
            // dispatch paths already use.
            if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
                return Err(anyhow::anyhow!(
                    "workflow '{workflow_id}' capability validation: {e}"
                ));
            }
            // Merge: extracted entity → initial_vars → caller's
            // explicit verdict initial_vars. Last writer wins, so
            // a routing rule's verdict can override entity fields if
            // it really needs to. Workflow vars_schema validates;
            // unknown keys are accepted (open schema by design).
            let mut merged_vars = serde_json::Map::new();
            if let Value::Object(m) = &entity {
                for (k, v) in m {
                    // Skip the synthetic `_headers` collection — it's
                    // there for routing predicates, not for the arc.
                    if k == "_headers" {
                        continue;
                    }
                    if !matches!(v, Value::Null) {
                        merged_vars.insert(k.clone(), v.clone());
                    }
                }
            }
            for (k, v) in initial_vars {
                merged_vars.insert(k, v);
            }
            // project_dir resolution priority:
            //   1. ${INLET_NAME_UPPERCASE}_PROJECT_DIR env override
            //      (works for webhooks AND pollers — both pass their
            //      `name` as inlet_name)
            //   2. inlet's `default_project_dir`
            //   3. None (worktree hooks will fail explicitly — better
            //      than silent fallback to cwd)
            let env_var = format!(
                "{}_PROJECT_DIR",
                inlet_name.to_uppercase().replace('-', "_")
            );
            let project_dir = std::env::var(&env_var).ok().or(default_project_dir);
            let workflow_id_clone = workflow_id.clone();
            // If the inlet that triggered this arc was a cron, the
            // cron registry has already incremented its in-flight
            // counter (in crons::run_one_tick → try_claim). Decrement
            // when the arc terminates so the next tick is admissible.
            // Inlets are labeled `cron:<name>` upstream; parse out the
            // name here.
            let cron_name = inlet_name.strip_prefix("cron:").map(|s| s.to_string());
            let crons_for_done = state.crons.clone();
            let server = BlackboxServer::new(state.clone());
            tokio::spawn(async move {
                let _ = workflow::run_workflow_with_initial_vars(
                    &server,
                    &compiled,
                    project_dir,
                    Some(50),
                    merged_vars,
                )
                .await;
                if let Some(name) = cron_name {
                    crons_for_done.mark_done(&name);
                }
            });
            Ok(json!({
                "status": "arc_started",
                "workflow": workflow_id_clone,
            }))
        }
    }
}

/// Dispatch an installed workflow by registry id, with optional initial
/// vars. Mirrors the `start_arc` routing verdict in webhook handling
/// but exposes it for direct CLI / scripted invocation.
#[derive(Debug, Deserialize)]
struct OrchestrateByIdRequest {
    workflow_id: String,
    #[serde(default)]
    initial_vars: serde_json::Map<String, Value>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
}

async fn orchestrate_by_id_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateByIdRequest>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec = match state
        .workflow_registry
        .read()
        .get(&req.workflow_id)
        .cloned()
    {
        Some(s) => s,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("workflow id '{}' not in registry", req.workflow_id),
            )
                .into_response();
        }
    };
    let compiled = match workflow::compile(spec) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile failed: {e}"),
            )
                .into_response();
        }
    };
    let server = BlackboxServer::new(state.clone());
    if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("capability validation failed: {e}"),
        )
            .into_response();
    }
    let result = workflow::run_workflow_with_initial_vars(
        &server,
        &compiled,
        req.project_dir,
        req.max_steps,
        req.initial_vars,
    )
    .await;
    axum::Json(result).into_response()
}

#[derive(Debug, Deserialize)]
struct IrcStatusQuery {
    #[serde(default)]
    tail: Option<usize>,
}

async fn irc_exec_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ExecParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_exec(Parameters(req)).await)
}

async fn irc_resume_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ResumeParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_resume(Parameters(req)).await)
}

async fn irc_broadcast_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<BroadcastParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(
        BlackboxServer::new(state)
            .bro_broadcast(Parameters(req))
            .await,
    )
}

async fn irc_status_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Query(query): Query<IrcStatusQuery>,
) -> axum::Json<CallToolResult> {
    axum::Json(
        BlackboxServer::new(state).bro_status(Parameters(StatusParams {
            task_id,
            tail: query.tail,
        })),
    )
}

async fn irc_dashboard_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<DashboardParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_dashboard(Parameters(query)))
}

async fn irc_cancel_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<CancelParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_cancel(Parameters(req)))
}

async fn irc_team_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(team_name): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    match orchestration::team::load_team(&team_name, &state.store_dir) {
        Some(team) => axum::Json(json!({
            "team": team.name,
            "members": team.members.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            format!("unknown team: {team_name}"),
        )
            .into_response(),
    }
}

// ── Admin HTTP endpoints (plain JSON; no MCP framing) ──────────────
//
// These wrap the same operations the MCP tools expose so install
// scripts can use plain `curl`. They're loopback-only via the listener
// binding.

async fn admin_packet_compile(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<Value>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let p: packets::CompileParams = match serde_json::from_value(req) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile params parse: {e}"),
            )
                .into_response();
        }
    };
    let result = state.packets.read().compile(&p);
    match result {
        Ok(msg) => axum::Json(json!({"status": "ok", "message": msg})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("compile: {e:#}"),
        )
            .into_response(),
    }
}

async fn read_artifact_source(source: &str) -> anyhow::Result<Value> {
    const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        let response = client.get(source).send().await?.error_for_status()?;
        let scheme = response.url().scheme();
        if scheme != "http" && scheme != "https" {
            anyhow::bail!("artifact source redirected to unsupported scheme `{scheme}`");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !(content_type.contains("application/json")
            || content_type.contains("text/json")
            || content_type.contains("text/plain"))
        {
            anyhow::bail!("artifact source content-type must be JSON or text/plain");
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_ARTIFACT_BYTES as u64)
        {
            anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > MAX_ARTIFACT_BYTES {
                anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes)?
    } else {
        std::fs::read_to_string(source)?
    };
    Ok(serde_json::from_str(&raw)?)
}

async fn install_artifact_from_params(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    let value = read_artifact_source(&p.source).await?;
    install_artifact_value(state, p, value).await
}

async fn install_artifact_value(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
    value: Value,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    match p.kind {
        artifacts::ArtifactKind::Workflow => {
            let spec: workflow::Workflow = serde_json::from_value(value.clone())?;
            let compiled = workflow::compile(spec.clone())?;
            if let Err(e) = validate_workflow_capabilities(&compiled, state) {
                anyhow::bail!("workflow capability validation failed: {e}");
            }
            let id = p.name.clone().unwrap_or_else(|| spec.name.clone());
            let dir = state.store_dir.join("workflows");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&spec).unwrap_or_default(),
            )?;
            state.workflow_registry.write().insert(id, spec);
        }
        artifacts::ArtifactKind::Packet => {
            let params: packets::CompileParams = serde_json::from_value(value.clone())?;
            state.packets.read().compile(&params)?;
        }
        artifacts::ArtifactKind::Brofile => {
            let brofile: orchestration::brofile::Brofile = serde_json::from_value(value.clone())?;
            orchestration::brofile::save_brofile(&brofile, "global", &state.store_dir, None);
        }
    }
    state
        .artifacts
        .write()
        .install_value(p.kind, p.source, &value, p.name, p.version, p.supersedes)
        .and_then(|meta| {
            if let Some(prev) = meta.supersedes.as_deref() {
                deactivate_artifact(state, meta.kind, prev)?;
            }
            Ok(meta)
        })
}

fn deactivate_artifact(
    state: &Arc<SharedState>,
    kind: artifacts::ArtifactKind,
    name: &str,
) -> anyhow::Result<()> {
    match kind {
        artifacts::ArtifactKind::Workflow => {
            state.workflow_registry.write().remove(name);
            let path = state
                .store_dir
                .join("workflows")
                .join(format!("{name}.json"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        artifacts::ArtifactKind::Packet => {
            state.packets.read().remove_domain(name)?;
        }
        artifacts::ArtifactKind::Brofile => {
            orchestration::brofile::delete_brofile(name, "global", &state.store_dir, None);
        }
    }
    Ok(())
}

fn rebuild_edge_index_from_shared(state: &SharedState) {
    let edges_dir = edge_index::edges_dir_from_bro_store(&state.store_dir);
    let idx = state.idx.read();
    let kb = state.kb.read();
    let threads = state.threads.read();
    let notes = state.notes.read();
    let task_store = state.task_store.read();
    let rebuilt = edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
        index: &idx,
        knowledge: &kb,
        threads: &threads,
        notes: &notes,
        task_store: &task_store,
        edges_dir,
    });
    *state.edge_index.write() = rebuilt;
}

/// Watcher thread that rebuilds the EdgeIndex when the underlying tantivy
/// corpus has grown. The auto-reindex thread writes new docs + edge sidecars
/// every interval, but it can't trigger a rebuild itself (it spawns before
/// SharedState exists). This watcher polls `idx.num_docs()` and triggers a
/// rebuild whenever the count advances, which folds in the new project_file
/// edges (IN_FILE / CONTAINS_SYMBOL / NEXT_CHUNK / etc.) so the agentic
/// graph surface stays current without manual intervention.
fn spawn_edge_index_rebuild_watcher(state: Arc<SharedState>, interval: std::time::Duration) {
    std::thread::Builder::new()
        .name("blackbox-edge-rebuild".into())
        .spawn(move || {
            // Initial settle so the boot-time rebuild already ran.
            std::thread::sleep(std::time::Duration::from_secs(20));
            let mut last_seen: u64 = state.idx.read().num_docs();
            loop {
                std::thread::sleep(interval);
                let current = state.idx.read().num_docs();
                if current > last_seen {
                    let started = std::time::Instant::now();
                    rebuild_edge_index_from_shared(&state);
                    tracing::info!(
                        prev_docs = last_seen,
                        new_docs = current,
                        elapsed_ms = started.elapsed().as_millis(),
                        "edge-index watcher: corpus grew, EdgeIndex rebuilt"
                    );
                    last_seen = current;
                }
            }
        })
        .expect("failed to spawn edge index rebuild watcher");
}

fn trigger_project_bootstrap_arc(state: Arc<SharedState>, record: ProjectRecord) {
    let Some(spec) = state
        .workflow_registry
        .read()
        .get("project-bootstrap-arc")
        .cloned()
    else {
        tracing::debug!(
            project_id = %record.project_id,
            "project-bootstrap-arc is not installed; registration recorded without arc trigger"
        );
        return;
    };
    let compiled = match workflow::compile(spec) {
        Ok(compiled) => compiled,
        Err(err) => {
            tracing::warn!(error = %err, "project-bootstrap-arc compile failed");
            return;
        }
    };
    if let Err(err) = validate_workflow_capabilities(&compiled, &state) {
        tracing::warn!(error = %err, "project-bootstrap-arc capability validation failed");
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime available; skipped project-bootstrap-arc trigger");
        return;
    };
    let project_dir = Some(record.canonical_path.clone());
    let mut vars = serde_json::Map::new();
    vars.insert("project_id".to_string(), Value::String(record.project_id));
    vars.insert(
        "project_path".to_string(),
        Value::String(record.canonical_path),
    );
    if let Some(repo_id) = record.repo_id {
        vars.insert("repo_id".to_string(), Value::String(repo_id));
    }
    handle.spawn(async move {
        let server = BlackboxServer::new(state);
        let _ = workflow::run_workflow_with_initial_vars(
            &server,
            &compiled,
            project_dir,
            Some(50),
            vars,
        )
        .await;
    });
}

#[derive(Debug, Deserialize)]
struct AdminWorkflowInstallReq {
    #[serde(default)]
    id: Option<String>,
    spec: Value,
}

async fn admin_workflow_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminWorkflowInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: workflow::Workflow = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("workflow parse: {e}"),
            )
                .into_response();
        }
    };
    let compiled = match workflow::compile(spec.clone()) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("workflow compile: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("capability validation: {e}"),
        )
            .into_response();
    }
    let id = req.id.unwrap_or_else(|| spec.name.clone());
    let dir = state.store_dir.join("workflows");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{id}.json"));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("workflow persist: {e}"),
        )
            .into_response();
    }
    state.workflow_registry.write().insert(id.clone(), spec);
    axum::Json(json!({"status": "installed", "id": id})).into_response()
}

async fn admin_artifact_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactInstallParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match install_artifact_from_params(&state, req).await {
        Ok(meta) => axum::Json(json!({"status": "installed", "artifact": meta})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact install: {e:#}"),
        )
            .into_response(),
    }
}

async fn admin_artifact_list(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<ArtifactListParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state.artifacts.read().list(&query) {
        Ok(rows) => axum::Json(json!({"artifacts": rows})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("artifact list: {e:#}"),
        )
            .into_response(),
    }
}

async fn admin_artifact_supersede(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactSupersedeParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state
        .artifacts
        .write()
        .supersede(req.kind, &req.name, &req.superseded_by)
    {
        Ok(meta) => match deactivate_artifact(&state, req.kind, &req.name) {
            Ok(()) => axum::Json(json!({"status": "superseded", "artifact": meta})).into_response(),
            Err(e) => (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact deactivate: {e:#}"),
            )
                .into_response(),
        },
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact supersede: {e:#}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct AdminWebhookInstallReq {
    spec: Value,
}

#[derive(Debug, Deserialize)]
struct AdminPollerInstallReq {
    spec: Value,
}

async fn admin_poller_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminPollerInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: pollers::PollerSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("poller parse: {e}"),
            )
                .into_response();
        }
    };
    let dir = state.store_dir.join("pollers");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("poller persist: {e}"),
        )
            .into_response();
    }
    state.pollers.install(spec.clone());
    let handle = pollers::spawn_loop(state.clone(), spec.clone());
    state.pollers.track_handle(&spec.name, handle);
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "every_seconds": spec.every_seconds,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct AdminCronInstallReq {
    spec: Value,
}

async fn admin_cron_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminCronInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: crons::CronSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("cron parse: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = crons::validate_schedule(&spec.schedule) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("cron schedule invalid: {e}"),
        )
            .into_response();
    }
    let dir = state.store_dir.join("crons");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("cron persist: {e}"),
        )
            .into_response();
    }
    state.crons.install(spec.clone());
    let handle = crons::spawn_loop(state.clone(), spec.clone());
    state.crons.track_handle(&spec.name, handle);
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "schedule": spec.schedule,
        "concurrency": spec.concurrency,
    }))
    .into_response()
}

async fn admin_webhook_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminWebhookInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: webhooks::WebhookSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("webhook parse: {e}"),
            )
                .into_response();
        }
    };
    // Reject schemes incompatible with current bind (parallel to
    // bro_webhook_install + restore-on-startup).
    if let Err(e) = webhooks::install_check(&spec.signature, state.bind_is_loopback) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("webhook install rejected: {e}"),
        )
            .into_response();
    }
    let dir = state.store_dir.join("webhooks");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("webhook persist: {e}"),
        )
            .into_response();
    }
    state.webhooks.install(spec.clone());
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "endpoint": format!("/webhook/{}", spec.name),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct AdminBrofileUpsertReq {
    name: String,
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    account: Option<String>,
}

async fn admin_brofile_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminBrofileUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let provider: orchestration::providers::Provider = match req.provider.parse() {
        Ok(p) => p,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("unknown provider '{}'", req.provider),
            )
                .into_response();
        }
    };
    let bf = orchestration::brofile::Brofile {
        name: req.name.clone(),
        provider,
        account: req.account,
        lens: req.lens,
        model: req.model,
        effort: req.effort,
        filters: None,
    };
    orchestration::brofile::save_brofile(&bf, "global", &state.store_dir, None);
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

#[derive(Debug, Deserialize)]
struct AdminTeamUpsertReq {
    name: String,
    members: Vec<String>,
}

async fn admin_team_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminTeamUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let teamplate = orchestration::team::Teamplate {
        name: req.name.clone(),
        members: req
            .members
            .iter()
            .enumerate()
            .map(|(i, brofile)| orchestration::team::TeamplateMember {
                brofile: brofile.clone(),
                alias: Some(format!("m{}", i + 1)),
                count: 1,
            })
            .collect(),
        advisor: None,
    };
    orchestration::team::save_teamplate(&teamplate, "global", &state.store_dir, None);
    let team = orchestration::team::Team {
        name: req.name.clone(),
        teamplate: req.name.clone(),
        members: req
            .members
            .iter()
            .enumerate()
            .map(|(i, brofile)| orchestration::team::TeamMember {
                name: format!("m{}", i + 1),
                brofile: brofile.clone(),
                session_id: None,
                task_history: Vec::new(),
            })
            .collect(),
        advisor: None,
        project_dir: None,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let _lock = orchestration::team::lock_teams();
    orchestration::team::save_team(&team, &state.store_dir);
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

async fn signal_arc_dispatch(
    state: &Arc<SharedState>,
    signal: &str,
    correlation: serde_json::Map<String, Value>,
    payload: Value,
) -> Value {
    let store = &state.wait_store;
    let pending_before: Vec<_> = store
        .snapshot()
        .into_iter()
        .filter(|w| w.signal == signal)
        .collect();
    let m = store.match_and_take(signal, &correlation);
    let Some((resolved_slot, notify, arc_id, wait_id)) = m else {
        tracing::info!(
            "signal '{signal}' arrived with correlation {correlation:?} — no matching wait (idle). pending_with_same_signal={:?}",
            pending_before
                .iter()
                .map(|w| (w.arc_id.clone(), w.wait_id.clone(), w.correlation.clone()))
                .collect::<Vec<_>>(),
        );
        state.record_signal(SignalEvent {
            timestamp: util::now_iso(),
            signal: signal.to_string(),
            correlation: correlation.clone(),
            outcome: "no_matching_wait".into(),
            matched_arc_id: None,
            matched_wait_id: None,
            idle_pending: pending_before.clone(),
        });
        return json!({
            "status": "no_matching_wait",
            "signal": signal,
            "correlation": correlation,
            "pending_with_same_signal": pending_before,
        });
    };
    tracing::info!(
        "signal '{signal}' arrived with correlation {correlation:?} — resolved wait arc={arc_id} wait_id={wait_id}",
    );
    state.record_signal(SignalEvent {
        timestamp: util::now_iso(),
        signal: signal.to_string(),
        correlation: correlation.clone(),
        outcome: "matched".into(),
        matched_arc_id: Some(arc_id.clone()),
        matched_wait_id: Some(wait_id.clone()),
        idle_pending: Vec::new(),
    });
    let sig = crate::workflow::context::SignalRef {
        name: signal.to_string(),
        payload,
        correlation,
        received_at: util::now_iso(),
    };
    *resolved_slot.lock() = Some(sig);
    notify.notify_one();
    json!({
        "status": "wait_resolved",
        "arc_id": arc_id,
        "wait_id": wait_id,
        "signal": signal,
    })
}

async fn roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<RosterQuery>,
) -> Result<axum::Json<Vec<BroRosterEntry>>, axum::http::StatusCode> {
    let store_dir = state.store_dir.clone();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();

    let no_selectors =
        wanted_teams.is_empty() && wanted_bros.is_empty() && wanted_sessions.is_empty();

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();

    // Team selectors — each contributes all members. Unknown teams are
    // skipped silently; the empty roster speaks for itself at the CLI layer.
    for tn in &wanted_teams {
        if let Some(team) = orchestration::team::load_team(tn, &store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors — include every match across all teams (deduped by team::bro).
    if !wanted_bros.is_empty() {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                if !wanted_bros.iter().any(|b| b == &member.name) {
                    continue;
                }
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Session selectors — synthetic adhoc lanes.
    for sid in &wanted_sessions {
        let key = format!("session::{sid}");
        if !seen.insert(key) {
            continue;
        }
        let path = index::find_session_file(sid, &config.roots, config.codex_root.as_deref());
        let provider = path.as_deref().and_then(infer_provider_from_path);
        entries.push(BroRosterEntry {
            bro: sid.chars().take(8).collect(),
            bro_selector: sid.clone(),
            team: "adhoc".into(),
            provider: provider
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into()),
            account: None,
            session_id: Some(sid.clone()),
            jsonl_path: path.map(|p| p.to_string_lossy().into_owned()),
            brofile: String::new(),
            model: None,
        });
    }

    // No selectors → full roster across every team (legacy default).
    if no_selectors {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors that the team-walk above didn't resolve fall
    // through here: we synthesize ad-hoc entries from currently-known
    // tasks whose `bro_label` matches. This is the only path that
    // surfaces brofile-only dispatched bros (workflow implementer /
    // advisor nodes) — they have no team membership, so the team
    // walk skips them. Without this, `bro tail keystone-impl` returns
    // an empty roster and the CLI bails with "bro does not exist".
    if !wanted_bros.is_empty() {
        let task_store = state.task_store.read();
        for task in task_store.all_tasks() {
            let inner = task.inner.lock();
            let label = match &inner.bro_label {
                Some(l) => l.clone(),
                None => continue,
            };
            // Match either bare-label (`keystone-impl`) or the
            // `team::member` form so callers can use either.
            let (team, member) = match label.split_once("::") {
                Some((t, m)) => (t.to_string(), m.to_string()),
                None => ("adhoc".to_string(), label.clone()),
            };
            let matches = wanted_bros.iter().any(|w| w == &member || w == &label);
            if !matches {
                continue;
            }
            let key = format!("{team}::{member}");
            if !seen.insert(key) {
                continue;
            }
            let session_id = if inner.session_id == "pending" {
                None
            } else {
                Some(inner.session_id.clone())
            };
            let jsonl_path = session_id.as_deref().and_then(|sid| {
                index::find_session_file(sid, &config.roots, config.codex_root.as_deref())
                    .map(|p| p.to_string_lossy().into_owned())
            });
            entries.push(BroRosterEntry {
                bro: member,
                bro_selector: label,
                team,
                provider: inner.provider.to_string(),
                account: None,
                session_id,
                jsonl_path,
                brofile: String::new(),
                model: None,
            });
        }
    }

    if !wanted_providers.is_empty() {
        entries.retain(|e| {
            e.provider
                .parse::<Provider>()
                .ok()
                .map(|p| wanted_providers.contains(&p))
                .unwrap_or(false)
        });
    }

    Ok(axum::Json(entries))
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TailQuery {
    /// Comma-separated team names — union of members. Accepts legacy `team=`.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated bro names. Accepts legacy `bro=`.
    #[serde(default, alias = "bro")]
    bros: Option<String>,
    /// Comma-separated session IDs — matches events by their task's session_id.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names. Accepts legacy `provider=`.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

async fn tail_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<TailQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.tail_tx.subscribe();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();
    let no_selectors = wanted_teams.is_empty()
        && wanted_bros.is_empty()
        && wanted_sessions.is_empty()
        && wanted_providers.is_empty();
    let store_dir = state.store_dir.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let tid = event.task_id();
                    let (task_provider, task_session_id, task_bro_label) = {
                        let store = state.task_store.read();
                        store.get(tid)
                            .map(|t| {
                                let inner = t.inner.lock();
                                (
                                    Some(inner.provider),
                                    Some(inner.session_id.clone()),
                                    inner.bro_label.clone(),
                                )
                            })
                            .unwrap_or((None, None, None))
                    };
                    let bro_ref = orchestration::team::find_bro_ref_for_task(tid, &store_dir);

                    // Effective selector + label resolution. Team-based lookup
                    // (find_bro_ref_for_task) wins when the dispatching path
                    // attributed via task_history. Otherwise fall back to the
                    // task's `bro_label` — set during dispatch so brofile-only
                    // workflow nodes (implementer / advisor) and ensemble
                    // members with duplicate-name brofiles surface in tail
                    // instead of being anonymous.
                    let (effective_member, effective_team, effective_label) = match &bro_ref {
                        Some(r) => {
                            let label = format!("{}::{}", r.team_name, r.member_name);
                            (Some(r.member_name.clone()), Some(r.team_name.clone()), Some(label))
                        }
                        None => {
                            let label = task_bro_label.clone();
                            let (team, member) = match label.as_deref() {
                                Some(s) => match s.split_once("::") {
                                    Some((t, m)) => (Some(t.to_string()), Some(m.to_string())),
                                    None => (None, Some(s.to_string())),
                                },
                                None => (None, None),
                            };
                            (member, team, label)
                        }
                    };

                    // Provider is a filter that applies on top of the selector
                    // union. Bros/sessions/teams are OR'd together: match ANY
                    // specified selector across them; a category being empty
                    // means it contributes no matches (but also doesn't reject).
                    let provider_ok = wanted_providers.is_empty()
                        || task_provider.map(|p| wanted_providers.contains(&p)).unwrap_or(false);
                    let selectors_specified = !wanted_bros.is_empty()
                        || !wanted_sessions.is_empty()
                        || !wanted_teams.is_empty();
                    let selector_match = if !selectors_specified {
                        true
                    } else {
                        let bro_m = match (&effective_member, &effective_label) {
                            (Some(m), Some(l)) => wanted_bros
                                .iter()
                                .any(|w| w == m || w == l),
                            (Some(m), None) => wanted_bros.iter().any(|w| w == m),
                            (None, Some(l)) => wanted_bros.iter().any(|w| w == l),
                            _ => false,
                        };
                        let session_m = task_session_id.as_deref()
                            .map(|s| wanted_sessions.iter().any(|w| w == s))
                            .unwrap_or(false);
                        let team_m_via_history = wanted_teams.iter().any(|tn| {
                            orchestration::team::load_team(tn, &store_dir)
                                .map(|team| team.members.iter()
                                    .any(|m| m.task_history.iter().any(|id| id == tid)))
                                .unwrap_or(false)
                        });
                        let team_m_via_label = match &effective_team {
                            Some(t) => wanted_teams.iter().any(|w| w == t),
                            None => false,
                        };
                        bro_m || session_m || team_m_via_history || team_m_via_label
                    };
                    if !(no_selectors || (provider_ok && selector_match)) {
                        continue;
                    }

                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(member) = &effective_member {
                        evt_json["bro_name"] = Value::String(member.clone());
                    }
                    if let Some(label) = &effective_label {
                        evt_json["bro_selector"] = Value::String(label.clone());
                    }
                    if let Some(team) = &effective_team {
                        evt_json["team_name"] = Value::String(team.clone());
                    }
                    if let Some(ref sid) = task_session_id {
                        if sid.as_str() != "pending" {
                            evt_json["session_id"] = Value::String(sid.clone());
                            if let Some(path) = index::find_session_file(
                                sid,
                                &config.roots,
                                config.codex_root.as_deref(),
                            ) {
                                evt_json["jsonl_path"] =
                                    Value::String(path.to_string_lossy().into_owned());
                            }
                        }
                    }
                    let data = serde_json::to_string(&evt_json).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("tail subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    let migrated = util::migrate_legacy_defaults(&home)?;

    // Logging
    let log_dir = util::blackbox_log_dir(&home);
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(3)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blackbox")
        .filename_suffix("log")
        .build(&log_dir)
        .expect("failed to create log appender");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "blackbox=info".into());

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!("PANIC: {}", info);
    }));
    for msg in migrated {
        tracing::info!("migrated legacy blackbox path: {msg}");
    }

    // Transcript index roots
    let roots: Vec<(String, PathBuf)> = if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
        val.split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                let expanded = if path.starts_with('~') {
                    home.join(&path[2..])
                } else {
                    PathBuf::from(path)
                };
                Some((name.to_string(), expanded))
            })
            .collect()
    } else {
        let mut found = vec![("claude".to_string(), home.join(".claude"))];
        if let Ok(entries) = std::fs::read_dir(&home) {
            let mut extras: Vec<(String, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with(".claude-")
                        && !name.contains("shared")
                        && e.path().join("projects").exists()
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let label = name.trim_start_matches(".claude-").to_string();
                    (label, e.path())
                })
                .collect();
            extras.sort_by(|a, b| a.0.cmp(&b.0));
            found.extend(extras);
        }
        found
    };

    let codex_root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() {
                Some(default)
            } else {
                None
            }
        });

    let index_path = util::blackbox_index_path(&home);

    tracing::info!(
        "Roots: {:?}",
        roots
            .iter()
            .map(|(n, p)| format!("{n}={}", p.display()))
            .collect::<Vec<_>>()
    );
    if let Some(ref cr) = codex_root {
        tracing::info!("Codex root: {}", cr.display());
    }
    tracing::info!("Index path: {}", index_path.display());

    let projects_path = util::blackbox_projects_path(&home);
    let kb_path = util::blackbox_knowledge_path(&home);
    let idx = TranscriptIndex::open_or_create(
        &index_path,
        roots,
        codex_root,
        projects_path.clone(),
        kb_path.clone(),
    )?;
    let projects_store = ProjectRegistry::open(&projects_path)?;
    tracing::info!("Project registry: {}", projects_path.display());

    let mut kb = Knowledge::open(&kb_path)?;
    tracing::info!("Knowledge store: {}", kb_path.display());

    // Sync the auto-generated tool reference into the knowledge store
    // so every agent's global memory picks up the current tool surface
    // on the next render. Idempotent: no-op when content is unchanged.
    match tool_docs::sync_into_knowledge(&mut kb) {
        Ok(r) if r.wrote => tracing::info!("Tool reference synced ({} bytes)", r.bytes),
        Ok(_) => tracing::debug!("Tool reference already up to date"),
        Err(e) => tracing::warn!("Tool reference sync failed: {e:#}"),
    }

    // Register blackbox in each installed provider's MCP config so that
    // every `{provider} ...` invocation (dispatched bros or interactive
    // sessions) sees the daemon without requiring user-managed config.
    // Resolves the "subprocessed bros don't see bbox tools" gap
    // discovered in the self-test pass.
    let bbox_port: u16 = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7264);
    let bbox_url = format!("http://127.0.0.1:{bbox_port}/mcp");
    let bbox_mcp_name = util::blackbox_mcp_name();
    // Export for provider arg-builders so they can inject `--mcp-config`
    // etc. at dispatch time — ensures dispatched subprocesses see
    // blackbox regardless of which config file their CLI inherits.
    std::env::set_var("BLACKBOX_MCP_URL", &bbox_url);
    std::env::set_var("BLACKBOX_MCP_NAME", &bbox_mcp_name);
    let report = orchestration::mcp::self_register_blackbox(&bbox_mcp_name, &bbox_url);
    tracing::info!(
        "blackbox MCP self-registration (name={}): {}",
        bbox_mcp_name,
        report.summary()
    );
    for (p, outcome) in &report.per_provider {
        if let orchestration::mcp::SelfRegisterOutcome::Error { detail } = outcome {
            tracing::warn!("self-register {p}: {detail}");
        }
    }

    // Sweep orphaned Gemini policy tempfiles from crashed/force-killed
    // dispatches. Files younger than 24h are kept in case they belong
    // to live tasks.
    match orchestration::mcp::sweep_stale_gemini_policies(24) {
        Ok(n) if n > 0 => tracing::info!("swept {n} stale gemini policy file(s)"),
        Ok(_) => {}
        Err(e) => tracing::debug!("gemini policy sweep: {e:#}"),
    }

    let th_path = util::blackbox_threads_path(&home);
    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());

    let notes_path = util::blackbox_notes_path(&home);
    let notes_store = Notes::open(&notes_path)?;
    tracing::info!("Notes store: {}", notes_path.display());

    let pins_path = util::blackbox_pins_path(&home);
    let pins_store = Pins::open(&pins_path)?;
    tracing::info!("Pins store: {}", pins_path.display());

    let packets_dir = util::blackbox_packets_dir(&home);
    let packets_store = Packets::open(&packets_dir)?;
    tracing::info!("Packets store: {}", packets_dir.join("packets").display());

    let artifacts_dir = util::blackbox_artifacts_dir(&home);
    let artifacts_store = artifacts::ArtifactCatalog::open(&artifacts_dir)?;
    tracing::info!("Artifact catalog: {}", artifacts_store.root().display());

    // Orchestration state
    let store_dir = PathBuf::from(
        std::env::var("BRO_STORE")
            .unwrap_or_else(|_| util::bro_home_dir(&home).to_string_lossy().to_string()),
    );
    let task_ttl = std::env::var("BRO_TASK_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400_000u64);
    let task_store = TaskStore::load(&store_dir, task_ttl);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);

    // Spawn background reindex thread
    let reindex_interval = std::env::var("BLACKBOX_REINDEX_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );

    // Bind address resolution is hoisted here so SharedState carries
    // a definitive `bind_is_loopback` flag; the listener bind below
    // uses the same value. Default 127.0.0.1; BBOX_BIND=0.0.0.0 to
    // accept docker-bridged webhooks.
    let bind_host = std::env::var("BBOX_BIND").unwrap_or_else(|_| "127.0.0.1".into());
    let bind_is_loopback = is_loopback_bind(&bind_host);

    let edge_index = edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
        index: &idx,
        knowledge: &kb,
        threads: &th,
        notes: &notes_store,
        task_store: &task_store,
        edges_dir: edge_index::edges_dir_from_bro_store(&store_dir),
    });

    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        kb: RwLock::new(kb),
        threads: RwLock::new(th),
        notes: RwLock::new(notes_store),
        pins: RwLock::new(pins_store),
        projects: RwLock::new(projects_store),
        packets: RwLock::new(packets_store),
        artifacts: RwLock::new(artifacts_store),
        edge_index: RwLock::new(edge_index),
        path_cache: RwLock::new(path_cache::PathCache::default()),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx: tail_tx.clone(),
        store_dir: store_dir.clone(),
        running_arcs: RwLock::new(HashMap::new()),
        wait_store: Arc::new(crate::workflow::wait::WaitStore::new()),
        webhooks: Arc::new(webhooks::WebhookRegistry::new()),
        pollers: Arc::new(pollers::PollerRegistry::new()),
        crons: Arc::new(crons::CronRegistry::new()),
        whiteboards: Arc::new(whiteboards::WhiteboardRegistry::new()),
        workflow_registry: Arc::new(RwLock::new(HashMap::new())),
        bind_is_loopback,
        signal_log: RwLock::new(std::collections::VecDeque::with_capacity(SIGNAL_LOG_CAP)),
        webhook_delivery_log: RwLock::new(std::collections::VecDeque::with_capacity(
            WEBHOOK_LOG_CAP,
        )),
        arc_cancel_tokens: RwLock::new(HashMap::new()),
        councils: Arc::new(council::CouncilRegistry::new()),
        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
    });
    vectors::install_global(Arc::new(vectors::VectorStore::open(
        vectors::default_vectors_dir(),
    )?));
    embed_queue::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    embed_queue::install_contradiction_state(shared.clone());
    embed_queue::install(embed::queue::EmbedQueueHandle::start_default());

    // Watch the tantivy corpus and rebuild the EdgeIndex whenever new docs
    // land via the auto-reindex thread (60s poll interval is sufficient
    // since the reindex tick is 120s by default).
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));

    // Restore webhook + workflow registries from disk so installs
    // survive daemon restart. Re-run install_check at restore time —
    // a webhook installed under loopback that's now being restored
    // under a public bind must NOT silently re-enable.
    let webhook_dir = shared.store_dir.join("webhooks");
    for spec in webhooks::load_all(&webhook_dir) {
        match webhooks::install_check(&spec.signature, shared.bind_is_loopback) {
            Ok(()) => {
                tracing::info!("restoring webhook '{}'", spec.name);
                shared.webhooks.install(spec);
            }
            Err(e) => {
                tracing::warn!(
                    "skipping restore of webhook '{}': install_check failed: {e}",
                    spec.name
                );
            }
        }
    }
    // Pollers — re-spawn the per-spec tick loop on startup so installs
    // survive daemon restart. Same store_dir/<name>.json shape as
    // webhooks; tick loop owns the schedule.
    let poller_dir = shared.store_dir.join("pollers");
    for spec in pollers::load_all(&poller_dir) {
        tracing::info!(
            "restoring poller '{}' (every {}s)",
            spec.name,
            spec.every_seconds
        );
        shared.pollers.install(spec.clone());
        let handle = pollers::spawn_loop(shared.clone(), spec.clone());
        shared.pollers.track_handle(&spec.name, handle);
    }
    // Crons — same restore-on-startup story. Schedule-validation
    // failures here log + skip rather than crash the daemon, mirroring
    // the webhook restore semantics (operator-installed specs may have
    // outlived a syntax change).
    let cron_dir = shared.store_dir.join("crons");
    for spec in crons::load_all(&cron_dir) {
        match crons::validate_schedule(&spec.schedule) {
            Ok(()) => {
                tracing::info!(
                    "restoring cron '{}' (schedule '{}', concurrency {})",
                    spec.name,
                    spec.schedule,
                    spec.concurrency
                );
                shared.crons.install(spec.clone());
                let handle = crons::spawn_loop(shared.clone(), spec.clone());
                shared.crons.track_handle(&spec.name, handle);
            }
            Err(e) => {
                tracing::warn!("skipping restore of cron '{}': {e}", spec.name);
            }
        }
    }
    // Whiteboards — restore active boards from disk so phase state +
    // posts + annotations + votes survive daemon restart. Boards mid-
    // arc benefit most; archived boards live separately at
    // <store>/whiteboards/archive/.
    let whiteboard_dir = shared.store_dir.join("whiteboards");
    if let Err(e) = shared.whiteboards.set_storage_dir(whiteboard_dir.clone()) {
        tracing::warn!("whiteboards storage init failed: {e}");
    } else {
        let restored = shared.whiteboards.list_ids().len();
        if restored > 0 {
            tracing::info!("restored {restored} active whiteboard(s)");
        }
    }
    // Councils — restore session/posts/envelopes from
    // <store>/councils/<id>/, then respawn drain workers for any
    // queued envelopes. Envelopes left in `Draining` from a prior
    // crash are reconciled by `respawn_workers_after_restart`:
    // marked done if a referencing post landed before the crash,
    // requeued (with attempt_count++) otherwise, failed once the
    // attempt budget is exhausted.
    let council_dir = shared.store_dir.join("councils");
    if let Err(e) = shared.councils.set_storage_dir(council_dir.clone()) {
        tracing::warn!("council storage init failed: {e}");
    } else {
        let restored = shared.councils.list_ids().len();
        if restored > 0 {
            tracing::info!("restored {restored} council(s)");
        }
        shared
            .councils
            .respawn_workers_after_restart(shared.clone());
    }
    let workflow_dir = shared.store_dir.join("workflows");
    if workflow_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&workflow_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.extension().is_some_and(|e| e == "json") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(spec) = serde_json::from_slice::<workflow::Workflow>(&bytes) {
                        let id = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(&spec.name)
                            .to_string();
                        tracing::info!("restoring workflow '{id}'");
                        shared.workflow_registry.write().insert(id, spec);
                    }
                }
            }
        }
    }

    // Packet self-heal scanner — off by default. Walks recent
    // packet events on an interval, flags candidates (high no_match
    // rate, low audit fidelity) by writing `op="repair_candidate"`
    // events. Does NOT dispatch repair agents — that's a separate
    // feature gated behind its own flag (not yet implemented).
    let scanner_config = ScannerConfig::from_env();
    if scanner_config.enabled {
        tracing::info!(
            interval_secs = scanner_config.interval.as_secs(),
            window_hours = scanner_config.window.as_secs() / 3600,
            no_match_threshold = scanner_config.no_match_threshold,
            fidelity_threshold = scanner_config.fidelity_threshold,
            "packet self-heal scanner: enabled"
        );
        let shared_for_scanner = shared.clone();
        tokio::spawn(async move {
            let cfg = scanner_config;
            let mut ticker = tokio::time::interval(cfg.interval);
            // Discard the immediate t=0 tick; run the first pass after
            // one interval so short-interval dev setups don't stampede
            // at startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let result = {
                    let guard = shared_for_scanner.packets.read();
                    guard.scanner_step(&cfg)
                };
                match result {
                    Ok(cands) if !cands.is_empty() => {
                        tracing::info!(
                            flagged = cands.len(),
                            "packet self-heal scanner: flagged repair candidates"
                        );
                    }
                    Ok(_) => {
                        tracing::debug!("packet self-heal scanner: no candidates this tick");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "packet self-heal scanner: tick failed");
                    }
                }
            }
        });
    } else {
        tracing::debug!("packet self-heal scanner: disabled");
    }

    // MCP service
    let port: u16 = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7264);

    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_stateful_mode(true);

    let shared_for_mcp = shared.clone();
    let session_keep_alive = std::env::var("BBOX_MCP_SESSION_KEEPALIVE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(6 * 60 * 60);
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive =
        Some(std::time::Duration::from_secs(session_keep_alive));
    let mcp_service: StreamableHttpService<BlackboxServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            session_manager.into(),
            config,
        );

    let app = axum::Router::new()
        .route("/tail", axum::routing::get(tail_handler))
        .route("/roster", axum::routing::get(roster_handler))
        .route("/orchestrate", axum::routing::post(orchestrate_handler))
        .route(
            "/orchestrate/stream",
            axum::routing::post(orchestrate_stream_handler),
        )
        .route(
            "/orchestrate/status",
            axum::routing::get(orchestrate_status_handler),
        )
        .route(
            "/orchestrate/list",
            axum::routing::get(orchestrate_list_handler),
        )
        .route(
            "/orchestrate/peek",
            axum::routing::get(orchestrate_peek_handler),
        )
        .route("/webhook/{name}", axum::routing::post(webhook_handler))
        .route(
            "/webhook/{name}/replay",
            axum::routing::post(webhook_replay_handler),
        )
        .route(
            "/orchestrate/by-id",
            axum::routing::post(orchestrate_by_id_handler),
        )
        .route("/irc/exec", axum::routing::post(irc_exec_handler))
        .route("/irc/resume", axum::routing::post(irc_resume_handler))
        .route("/irc/broadcast", axum::routing::post(irc_broadcast_handler))
        .route(
            "/irc/status/{task_id}",
            axum::routing::get(irc_status_handler),
        )
        .route("/irc/dashboard", axum::routing::get(irc_dashboard_handler))
        .route("/irc/cancel", axum::routing::post(irc_cancel_handler))
        .route(
            "/irc/team/{team_name}",
            axum::routing::get(irc_team_handler),
        )
        .route(
            "/admin/packet/compile",
            axum::routing::post(admin_packet_compile),
        )
        .route(
            "/admin/workflow/install",
            axum::routing::post(admin_workflow_install),
        )
        .route(
            "/admin/artifact/install",
            axum::routing::post(admin_artifact_install),
        )
        .route(
            "/admin/artifact/list",
            axum::routing::get(admin_artifact_list),
        )
        .route(
            "/admin/artifact/supersede",
            axum::routing::post(admin_artifact_supersede),
        )
        .route(
            "/admin/webhook/install",
            axum::routing::post(admin_webhook_install),
        )
        .route(
            "/admin/poller/install",
            axum::routing::post(admin_poller_install),
        )
        .route(
            "/admin/cron/install",
            axum::routing::post(admin_cron_install),
        )
        .route(
            "/admin/brofile/upsert",
            axum::routing::post(admin_brofile_upsert),
        )
        .route("/admin/team/upsert", axum::routing::post(admin_team_upsert))
        .route(
            "/council",
            axum::routing::post(council::http::create).get(council::http::list),
        )
        .route(
            "/council/{id}",
            axum::routing::get(council::http::open).delete(council::http::close),
        )
        .route(
            "/council/{id}/post",
            axum::routing::post(council::http::post),
        )
        .route(
            "/council/{id}/tail",
            axum::routing::get(council::http::tail),
        )
        .with_state(shared.clone())
        .nest_service("/mcp", mcp_service);

    // Bind address resolved above (hoisted so SharedState gets the
    // loopback flag). Default `127.0.0.1`; BBOX_BIND=0.0.0.0 opens
    // the listener to docker-bridged peers — closed-network only.
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;
    tracing::info!(
        "blackboxd listening on http://{bind_host}:{port}/mcp (loopback={bind_is_loopback})"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    // Persist tasks on shutdown
    shared.task_store.read().persist(&store_dir);
    tracing::info!("blackboxd shut down");
    Ok(())
}

fn tier0_cosine_threshold_from_env() -> f32 {
    const DEFAULT: f32 = 0.85;
    match std::env::var("BBOX_TIER0_COSINE_THRESHOLD") {
        Ok(raw) => match raw.parse::<f32>() {
            Ok(value) if (0.0..=1.0).contains(&value) => value,
            Ok(value) => {
                tracing::warn!(
                    value,
                    "BBOX_TIER0_COSINE_THRESHOLD outside [0.0, 1.0]; using default"
                );
                DEFAULT
            }
            Err(err) => {
                tracing::warn!(
                    value = raw,
                    error = %err,
                    "invalid BBOX_TIER0_COSINE_THRESHOLD; using default"
                );
                DEFAULT
            }
        },
        Err(_) => DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        let index = TranscriptIndex::open_or_create(
            &tmp.path().join("index"),
            Vec::new(),
            None,
            tmp.path().join("projects.json"),
            tmp.path().join("knowledge.json"),
        )
        .unwrap();
        let kb = Knowledge::open(&tmp.path().join("knowledge.json")).unwrap();
        let threads = Threads::open(&tmp.path().join("threads.json")).unwrap();
        let notes = Notes::open(&tmp.path().join("notes.json")).unwrap();
        let pins = Pins::open(&tmp.path().join("pins.json")).unwrap();
        let projects = ProjectRegistry::open(tmp.path().join("projects.json")).unwrap();
        let packets = Packets::open(tmp.path()).unwrap();
        let artifacts = artifacts::ArtifactCatalog::open(tmp.path().join("artifacts")).unwrap();
        let (tail_tx, _) = broadcast::channel::<TailEvent>(16);
        let state = Arc::new(SharedState {
            idx: RwLock::new(index),
            kb: RwLock::new(kb),
            threads: RwLock::new(threads),
            notes: RwLock::new(notes),
            pins: RwLock::new(pins),
            projects: RwLock::new(projects),
            packets: RwLock::new(packets),
            artifacts: RwLock::new(artifacts),
            edge_index: RwLock::new(edge_index::EdgeIndex::default()),
            path_cache: RwLock::new(path_cache::PathCache::default()),
            task_store: Arc::new(RwLock::new(TaskStore::new())),
            tail_tx,
            store_dir: tmp.path().join("bro"),
            running_arcs: RwLock::new(HashMap::new()),
            wait_store: Arc::new(crate::workflow::wait::WaitStore::new()),
            webhooks: Arc::new(webhooks::WebhookRegistry::new()),
            pollers: Arc::new(pollers::PollerRegistry::new()),
            crons: Arc::new(crons::CronRegistry::new()),
            whiteboards: Arc::new(whiteboards::WhiteboardRegistry::new()),
            workflow_registry: Arc::new(RwLock::new(HashMap::new())),
            bind_is_loopback: true,
            signal_log: RwLock::new(std::collections::VecDeque::with_capacity(SIGNAL_LOG_CAP)),
            webhook_delivery_log: RwLock::new(std::collections::VecDeque::with_capacity(
                WEBHOOK_LOG_CAP,
            )),
            arc_cancel_tokens: RwLock::new(HashMap::new()),
            councils: Arc::new(council::CouncilRegistry::new()),
            resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        });
        BlackboxServer::new(state)
    }

    fn save_test_brofile(tmp: &tempfile::TempDir, name: &str) {
        orchestration::brofile::save_brofile(
            &orchestration::brofile::Brofile {
                name: name.to_string(),
                provider: Provider::Gemini,
                account: None,
                lens: None,
                model: None,
                effort: None,
                filters: None,
            },
            "global",
            &tmp.path().join("bro"),
            None,
        );
    }

    #[tokio::test]
    async fn artifact_install_wires_f3_workflow_and_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/schema-migration-arc.json"
        ))
        .unwrap();
        let packet_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/workflow-policy/arc-budget.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/schema-migration-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/workflow-policy/arc-budget.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            packet_value,
        )
        .await
        .unwrap();

        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("schema-migration-arc"));
        assert!(server
            .state
            .packets
            .read()
            .load("domain:workflow-policy/arc-budget")
            .is_ok());
        let rows = server
            .state
            .artifacts
            .read()
            .list(&ArtifactListParams {
                kind: None,
                name: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn artifact_install_wires_project_bootstrap_arc() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/project-bootstrap-arc.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/project-bootstrap-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();

        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("project-bootstrap-arc"));
        let rows = server
            .state
            .artifacts
            .read()
            .list(&ArtifactListParams {
                kind: Some(artifacts::ArtifactKind::Workflow),
                name: Some("project-bootstrap-arc".into()),
            })
            .unwrap();
        assert_eq!(rows.len(), 1);

        let compiled = {
            let workflow = server
                .state
                .workflow_registry
                .read()
                .get("project-bootstrap-arc")
                .cloned()
                .unwrap();
            workflow::compile(workflow).unwrap()
        };
        let mut vars = serde_json::Map::new();
        vars.insert("project_id".into(), Value::String("proj1234".into()));
        vars.insert(
            "project_path".into(),
            Value::String(tmp.path().to_string_lossy().into_owned()),
        );
        let result = workflow::run_workflow_with_initial_vars(
            &server,
            &compiled,
            Some(tmp.path().to_string_lossy().into_owned()),
            Some(50),
            vars,
        )
        .await;
        assert_eq!(result.status, "completed");
        assert_eq!(result.vars.get("published"), Some(&Value::Bool(true)));
        let arc_id = result.arc_thread_id.as_deref().unwrap_or_default();
        let snapshot = server
            .state
            .running_arcs
            .read()
            .get(arc_id)
            .cloned()
            .unwrap();
        assert_eq!(snapshot.status, "completed");
        assert!(snapshot
            .completed_nodes
            .iter()
            .any(|node| node == "Publish"));
    }

    #[tokio::test]
    async fn artifact_install_wires_m2_compaction_arc_and_packets() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/embed-compaction-arc.json"
        ))
        .unwrap();
        let policy_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/embed/compaction-policy.json"
        ))
        .unwrap();
        let cron_routing_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/cron-routing/embed-compaction.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/embed-compaction-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/embed/compaction-policy.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            policy_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/cron-routing/embed-compaction.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            cron_routing_value,
        )
        .await
        .unwrap();

        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("embed-compaction-arc"));
        assert!(server
            .state
            .packets
            .read()
            .load("domain:embed/compaction-policy")
            .is_ok());
        assert!(server
            .state
            .packets
            .read()
            .load("domain:cron-routing/embed-compaction")
            .is_ok());
    }

    #[tokio::test]
    async fn embed_compaction_arc_gates_against_vector_status_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let vector_store =
            Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
        let _guard = vectors::install_test_global(vector_store.clone());
        let route = "test-compaction-route";
        for idx in 0..10 {
            let theta = idx as f32 * 0.01;
            vector_store
                .upsert(
                    route,
                    &format!("entity-{idx}"),
                    &format!("hash-{idx}"),
                    vec![theta.cos(), theta.sin(), 0.0, 0.0],
                )
                .unwrap();
        }
        for idx in 0..4 {
            vector_store
                .delete(route, &format!("entity-{idx}"))
                .unwrap();
        }
        let before = vector_store.metrics().remove(route).unwrap();
        assert_eq!(before.active_count, 6);
        assert_eq!(before.deleted_count, 4);
        assert!(before.deleted_ratio > 0.3);

        let server = test_server(&tmp);
        let packet_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/embed/compaction-policy.json"
        ))
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/embed/compaction-policy.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            packet_value,
        )
        .await
        .unwrap();

        let workflow_spec: workflow::Workflow = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/embed-compaction-arc.json"
        ))
        .unwrap();
        let compiled = workflow::compile(workflow_spec).unwrap();
        let result = workflow::run_workflow_with_initial_vars(
            &server,
            &compiled,
            Some(tmp.path().to_string_lossy().into_owned()),
            Some(20),
            serde_json::Map::new(),
        )
        .await;

        assert_eq!(result.status, "completed");
        assert_eq!(result.vars.get("rebuild_started"), Some(&Value::Bool(true)));
        assert_eq!(result.vars.get("swapped"), Some(&Value::Bool(true)));
        assert!(result
            .events
            .iter()
            .any(
                |event| event.get("kind").and_then(Value::as_str) == Some("gate_applied")
                    && event
                        .get("data")
                        .and_then(|data| data.get("verdict"))
                        .and_then(Value::as_str)
                        == Some("compact")
            ));
        let after = vector_store.metrics().remove(route).unwrap();
        assert_eq!(after.active_count, 6);
        assert_eq!(after.deleted_count, 0);
    }

    #[tokio::test]
    async fn artifact_install_wires_m3_auto_digest_artifacts_and_audit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let brofile_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/brofiles/digest-extractor.json"
        ))
        .unwrap();
        assert_eq!(
            brofile_value["disallow_tools"],
            serde_json::json!(["Edit", "Write", "Bash"])
        );
        let trust_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/bro-trust/per-brofile.json"
        ))
        .unwrap();
        let quality_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/auto-digest/entry-quality.json"
        ))
        .unwrap();
        let routing_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/auto-digest/task-completed-routing.json"
        ))
        .unwrap();
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/auto-digest-arc.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: "examples/agentic-corpus/brofiles/digest-extractor.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            brofile_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/bro-trust/per-brofile.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            trust_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/auto-digest/entry-quality.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            quality_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/auto-digest/task-completed-routing.json"
                    .into(),
                name: None,
                version: None,
                supersedes: None,
            },
            routing_value,
        )
        .await
        .unwrap();
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/auto-digest-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();

        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("auto-digest-arc"));
        assert!(server
            .state
            .packets
            .read()
            .load("domain:auto-digest/entry-quality")
            .is_ok());
        assert!(server
            .state
            .packets
            .read()
            .load("domain:auto-digest/task-completed-routing")
            .is_ok());
        assert!(orchestration::brofile::resolve_brofile(
            "digest-extractor",
            &server.state.store_dir,
            None
        )
        .is_some());

        let cases: Value =
            serde_json::from_str(include_str!("../eval/audit/auto-digest/cases.json")).unwrap();
        let cases = cases.as_array().unwrap();
        let packet_store = server.state.packets.read();
        let packet = packet_store
            .load("domain:auto-digest/entry-quality")
            .unwrap();
        let mut matched = 0usize;
        for case in cases {
            let entity = serde_json::json!({
                "vars": {
                    "candidate": case["proposal"].clone()
                }
            });
            let prediction = packets::apply_with(&packet, &entity, &*packet_store)
                .unwrap_or_else(|| panic!("case {} produced no verdict", case["id"]));
            if prediction.classification == case["expected_verdict"].as_str().unwrap() {
                matched += 1;
            }
        }
        assert!(
            matched >= 18,
            "auto-digest audit fidelity {matched}/{} below gate",
            cases.len()
        );
        assert_eq!(matched, cases.len());
    }

    #[tokio::test]
    async fn artifact_install_wires_m4_contradiction_review_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/contradiction-review-arc.json"
        ))
        .unwrap();
        let packet_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/contradiction/review-synthesis.json"
        ))
        .unwrap();
        let brofiles: [(&str, Value); 4] = [
            (
                "contradiction-provenance",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/contradiction-provenance.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-lifecycle",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/contradiction-lifecycle.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-coherence",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/contradiction-coherence.json"
                ))
                .unwrap(),
            ),
            (
                "contradiction-facilitator",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/contradiction-facilitator.json"
                ))
                .unwrap(),
            ),
        ];

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/contradiction/review-synthesis.json"
                    .into(),
                name: None,
                version: None,
                supersedes: None,
            },
            packet_value,
        )
        .await
        .unwrap();
        for (name, value) in brofiles {
            install_artifact_value(
                &server.state,
                ArtifactInstallParams {
                    kind: artifacts::ArtifactKind::Brofile,
                    source: format!("examples/agentic-corpus/brofiles/{name}.json"),
                    name: None,
                    version: None,
                    supersedes: None,
                },
                value,
            )
            .await
            .unwrap();
        }
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/contradiction-review-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();

        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("contradiction-review-arc"));
        let packet_store = server.state.packets.read();
        let packet = packet_store
            .load("domain:contradiction/review-synthesis")
            .unwrap();
        let prediction = packets::apply_with(
            &packet,
            &json!({"vars": {"verdict": {"verdict": "contradicts"}}}),
            &*packet_store,
        )
        .unwrap();
        assert_eq!(prediction.classification, "contradicts");
        assert!(orchestration::brofile::resolve_brofile(
            "contradiction-facilitator",
            &server.state.store_dir,
            None
        )
        .is_some());
    }

    #[tokio::test]
    async fn artifact_install_wires_m5_auto_edge_artifacts_and_audit() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let packet_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/packets/auto-edge/vote-aggregate.json"
        ))
        .unwrap();
        let workflow_value: Value = serde_json::from_str(include_str!(
            "../examples/agentic-corpus/workflows/auto-edge-arc.json"
        ))
        .unwrap();
        let brofiles: [(&str, Value); 6] = [
            (
                "describe-prose-signal",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/describe-prose-signal.json"
                ))
                .unwrap(),
            ),
            (
                "describe-symbol-fit",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/describe-symbol-fit.json"
                ))
                .unwrap(),
            ),
            (
                "describe-narrative-cohesion",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/describe-narrative-cohesion.json"
                ))
                .unwrap(),
            ),
            (
                "reference-citation-precision",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/reference-citation-precision.json"
                ))
                .unwrap(),
            ),
            (
                "reference-target-existence",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/reference-target-existence.json"
                ))
                .unwrap(),
            ),
            (
                "reference-context-fit",
                serde_json::from_str(include_str!(
                    "../examples/agentic-corpus/brofiles/reference-context-fit.json"
                ))
                .unwrap(),
            ),
        ];

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Packet,
                source: "examples/agentic-corpus/packets/auto-edge/vote-aggregate.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            packet_value,
        )
        .await
        .unwrap();
        for (name, value) in brofiles {
            install_artifact_value(
                &server.state,
                ArtifactInstallParams {
                    kind: artifacts::ArtifactKind::Brofile,
                    source: format!("examples/agentic-corpus/brofiles/{name}.json"),
                    name: None,
                    version: None,
                    supersedes: None,
                },
                value,
            )
            .await
            .unwrap();
            assert!(orchestration::brofile::resolve_brofile(
                name,
                &server.state.store_dir,
                None
            )
            .is_some());
        }
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "examples/agentic-corpus/workflows/auto-edge-arc.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_value,
        )
        .await
        .unwrap();
        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("auto-edge-arc"));

        let packet_store = server.state.packets.read();
        let packet = packet_store
            .load("domain:auto-edge/vote-aggregate")
            .unwrap();
        for cases in [
            serde_json::from_str::<Value>(include_str!("../eval/audit/auto-edge/describes.json"))
                .unwrap(),
            serde_json::from_str::<Value>(include_str!(
                "../eval/audit/auto-edge/references.json"
            ))
            .unwrap(),
        ] {
            let rows = cases.as_array().unwrap();
            let mut matched = 0usize;
            for case in rows {
                let prediction = packets::apply_with(&packet, &case["entity"], &*packet_store)
                    .unwrap_or_else(|| panic!("case {} produced no verdict", case["id"]));
                if prediction.classification == case["expected"].as_str().unwrap() {
                    matched += 1;
                }
            }
            assert!(
                matched >= 12,
                "auto-edge audit fidelity {matched}/{} below gate",
                rows.len()
            );
            assert_eq!(matched, rows.len());
        }
    }

    #[tokio::test]
    async fn write_semantic_edge_projects_describes_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let edges_dir = tmp.path().join("edges");
        let source = "project_file:proj1234:relhash:chunkhash:0";
        let target = "symbol:proj1234:EntityRef:defnhash";
        let ctx = workflow::context::ArcContext::new(workflow::context::ArcMeta {
            arc_id: "arc-test".into(),
            workflow_name: "auto-edge-arc".into(),
            workflow_version: 1,
            project_dir: Some(tmp.path().to_string_lossy().into_owned()),
            ..Default::default()
        });
        let hook = workflow::ops::HookOp {
            op: workflow::ops::OpKind::WriteSemanticEdge,
            args: json!({
                "source": source,
                "target": target,
                "kind": "DESCRIBES",
                "edges_dir": edges_dir,
                "note": "synthetic doc-section describes EntityRef"
            }),
            when: None,
            on_failure: workflow::ops::OnFailure::Halt,
            into_var: Some("semantic_edge".into()),
        };
        workflow::ops::execute_op(&hook, &ctx, None).await.unwrap();
        let edge_index = edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
            index: &server.state.idx.read(),
            knowledge: &server.state.kb.read(),
            threads: &server.state.threads.read(),
            notes: &server.state.notes.read(),
            task_store: &server.state.task_store.read(),
            edges_dir,
        });
        let source_ref = entity_ref::EntityRef::parse(source).unwrap();
        let target_ref = entity_ref::EntityRef::parse(target).unwrap();
        assert!(edge_index
            .forward_edges(&source_ref)
            .iter()
            .any(|edge| edge.kind == "DESCRIBES" && edge.target == target_ref));
    }

    #[tokio::test]
    async fn shipped_packet_audit_examples_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let packets = [
            "examples/agentic-corpus/packets/workflow-policy/arc-budget.json",
            "examples/agentic-corpus/packets/embed/compaction-policy.json",
            "examples/agentic-corpus/packets/cron-routing/embed-compaction.json",
            "examples/agentic-corpus/packets/bro-trust/per-brofile.json",
            "examples/agentic-corpus/packets/auto-digest/task-completed-routing.json",
            "examples/agentic-corpus/packets/auto-digest/entry-quality.json",
            "examples/agentic-corpus/packets/contradiction/review-synthesis.json",
            "examples/agentic-corpus/packets/auto-edge/vote-aggregate.json",
            "examples/agentic-corpus/packets/eval/drift-policy.json",
        ];
        for rel in packets {
            let path = root.join(rel);
            let value: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            install_artifact_value(
                &server.state,
                ArtifactInstallParams {
                    kind: artifacts::ArtifactKind::Packet,
                    source: rel.into(),
                    name: None,
                    version: None,
                    supersedes: None,
                },
                value,
            )
            .await
            .unwrap();
        }

        let audits = [
            "examples/agentic-corpus/packets/workflow-policy/arc-budget.audit_examples.json",
            "examples/agentic-corpus/packets/embed/compaction-policy.audit_examples.json",
            "examples/agentic-corpus/packets/cron-routing/embed-compaction.audit_examples.json",
            "examples/agentic-corpus/packets/bro-trust/per-brofile.audit_examples.json",
            "examples/agentic-corpus/packets/auto-digest/task-completed-routing.audit_examples.json",
            "examples/agentic-corpus/packets/auto-digest/entry-quality.audit_examples.json",
            "examples/agentic-corpus/packets/contradiction/review-synthesis.audit_examples.json",
            "examples/agentic-corpus/packets/auto-edge/vote-aggregate.audit_examples.json",
            "examples/agentic-corpus/packets/eval/drift-policy.audit_examples.json",
        ];
        let packet_store = server.state.packets.read();
        for rel in audits {
            let spec: Value =
                serde_json::from_str(&std::fs::read_to_string(root.join(rel)).unwrap()).unwrap();
            let rendered = packet_store
                .audit_tool(&packets::AuditParams {
                    packet_id: spec["packet_id"].as_str().unwrap().into(),
                    dataset: spec["dataset"].clone(),
                    mode: None,
                })
                .unwrap();
            let report: Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(
                report["fidelity"].as_f64().unwrap(),
                1.0,
                "audit examples failed for {rel}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn tier0_contradiction_without_arc_surfaces_surprise_note() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        embed_queue::install_contradiction_threshold(0.85);
        embed_queue::install_contradiction_state(server.state.clone());
        let vector_store = Arc::new(vectors::VectorStore::open(tmp.path().join("vectors")).unwrap());
        let _guard = vectors::install_test_global(vector_store.clone());
        let now = "2026-01-01T00:00:00Z".to_string();
        for (id, content) in [
            ("aaaabbbb", "use provider A for embeddings"),
            ("ccccdddd", "never use provider A for embeddings"),
        ] {
            server
                .state
                .kb
                .write()
                .upsert_generated(knowledge::KnowledgeEntry {
                    id: id.into(),
                    title: id.into(),
                    content: content.into(),
                    cluster: None,
                    variants: Default::default(),
                    category: knowledge::Category::Memory,
                    scope: knowledge::Scope::Global,
                    project: None,
                    providers: Vec::new(),
                    priority: knowledge::Priority::Standard,
                    weight: 100,
                    status: knowledge::Status::Active,
                    approval: knowledge::Approval::UserConfirmed,
                    render: false,
                    decay: true,
                    review_at: None,
                    supersedes: None,
                    links: Vec::new(),
                    rationale: None,
                    expires_at: None,
                    source: "test".into(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    recall_count: 0,
                    last_recalled: None,
                })
                .unwrap();
        }
        vector_store
            .upsert("knowledge-test", "knowledge:ccccdddd", "h-old", vec![1.0, 0.0])
            .unwrap();
        vector_store
            .upsert("knowledge-test", "knowledge:aaaabbbb", "h-new", vec![0.99, 0.01])
            .unwrap();
        let request = embed::queue::EmbedRequest {
            bucket: embed::Bucket::Knowledge,
            project_id: None,
            entity_id: "knowledge:aaaabbbb".into(),
            chunk_hash: "h-new".into(),
            text: "use provider A for embeddings".into(),
        };
        embed_queue::maybe_detect_knowledge_contradiction(
            &request,
            "knowledge-test",
            &[0.99, 0.01],
        );

        assert!(server.state.notes.read().all().iter().any(|note| {
            note.body.contains("Tier-0 contradiction detected")
                && note.body.contains("knowledge:aaaabbbb")
                && note.body.contains("knowledge:ccccdddd")
        }));

        embed_queue::install_contradiction_threshold(1.0);
        let note_count = server.state.notes.read().all().len();
        embed_queue::maybe_detect_knowledge_contradiction(
            &request,
            "knowledge-test",
            &[0.99, 0.01],
        );
        assert_eq!(server.state.notes.read().all().len(), note_count);
        embed_queue::install_contradiction_threshold(0.85);
    }

    #[tokio::test]
    async fn artifact_supersession_deactivates_workflow_registry_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_a = serde_json::json!({
            "name": "workflow-a",
            "version": 1,
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });
        let workflow_a2 = serde_json::json!({
            "name": "workflow-a2",
            "version": 2,
            "supersedes": "workflow-a",
            "actors": {},
            "start": "Done",
            "nodes": {"Done": {"actor": "", "next": {"type": "terminal"}}}
        });

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "workflow-a.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_a,
        )
        .await
        .unwrap();
        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a"));

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Workflow,
                source: "workflow-a2.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_a2,
        )
        .await
        .unwrap();

        assert!(!server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a"));
        assert!(server
            .state
            .workflow_registry
            .read()
            .contains_key("workflow-a2"));
        assert!(!server
            .state
            .store_dir
            .join("workflows")
            .join("workflow-a.json")
            .exists());
    }

    #[tokio::test]
    async fn read_artifact_source_rejects_oversized_http_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: application/json\r\n",
                "Content-Length: 1048577\r\n",
                "\r\n",
                "{}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let err = read_artifact_source(&format!("http://{addr}/artifact.json"))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("too large"), "got: {err}");
    }

    #[test]
    fn bbox_project_list_round_trips_through_tool_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let server = test_server(&tmp);

        let register = server.bbox_project_register(Parameters(ProjectRegisterParams {
            path: project.to_string_lossy().into_owned(),
        }));
        assert_ne!(register.is_error, Some(true));

        let listed = server.bbox_project_list();
        assert_ne!(listed.is_error, Some(true));
        let wire = serde_json::to_value(&listed).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        let response: ProjectListResponse = serde_json::from_str(text).unwrap();

        assert_eq!(response.projects.len(), 1);
        assert_eq!(
            response.projects[0].project_id,
            entity_ref::project_id_for_path(&project).unwrap()
        );
    }

    #[test]
    fn resolve_resume_target_rejects_ambiguous_bro_names_across_live_teams() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        save_test_brofile(&tmp, "reviewer");

        for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
            orchestration::team::save_team(
                &orchestration::team::Team {
                    name: team_name.to_string(),
                    teamplate: "review".into(),
                    members: vec![orchestration::team::TeamMember {
                        name: "reviewer".into(),
                        brofile: "reviewer".into(),
                        session_id: Some(session_id.into()),
                        task_history: vec![],
                    }],
                    advisor: None,
                    project_dir: None,
                    created_at: 0,
                },
                &tmp.path().join("bro"),
            );
        }

        let err = server
            .resolve_resume_target(Some("reviewer"), None, None, None)
            .unwrap_err();
        assert!(err.contains("Ambiguous bro name: reviewer"));
        assert!(err.contains("red"));
        assert!(err.contains("blue"));
    }

    #[test]
    fn resolve_resume_target_accepts_scoped_team_bro_selector() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        save_test_brofile(&tmp, "reviewer");

        for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
            orchestration::team::save_team(
                &orchestration::team::Team {
                    name: team_name.to_string(),
                    teamplate: "review".into(),
                    members: vec![orchestration::team::TeamMember {
                        name: "reviewer".into(),
                        brofile: "reviewer".into(),
                        session_id: Some(session_id.into()),
                        task_history: vec![],
                    }],
                    advisor: None,
                    project_dir: Some(format!("/tmp/{team_name}")),
                    created_at: 0,
                },
                &tmp.path().join("bro"),
            );
        }

        let (provider, session_id, _lens, _opts, _env, cwd, _filters) = server
            .resolve_resume_target(Some("blue::reviewer"), None, None, None)
            .unwrap();
        assert_eq!(provider, Provider::Gemini);
        assert_eq!(session_id, "sid-blue");
        assert_eq!(cwd.as_deref(), Some("/tmp/blue"));
    }

    #[test]
    fn build_advisor_checkpoint_flattens_note_counts_for_packets() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        {
            let mut notes = server.state.notes.write();
            notes
                .create(&NoteParams {
                    kind: "blocked".into(),
                    body: "worker is blocked".into(),
                    task_id: Some("task-1".into()),
                    session_id: None,
                    project: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("worker".into()),
                })
                .unwrap();
            notes
                .create(&NoteParams {
                    kind: "dispute".into(),
                    body: "worker disputes premise".into(),
                    task_id: Some("task-1".into()),
                    session_id: None,
                    project: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("worker".into()),
                })
                .unwrap();
        }
        let team = orchestration::team::Team {
            name: "demo".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: Some(orchestration::team::TeamAdvisor {
                name: "advisor".into(),
                config: orchestration::team::TeamAdvisorConfig {
                    brofile: "advisor".into(),
                    alias: Some("advisor".into()),
                    charter: "demo".into(),
                    context: None,
                    halt_conditions: vec![],
                    exit_conditions: vec![],
                    packet_id: Some("packet-demo".into()),
                    timeout_seconds: None,
                    mode: orchestration::team::AdvisorMode::Blocking,
                },
                session_id: None,
                task_history: vec![],
            }),
            project_dir: None,
            created_at: 0,
        };
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "when_all",
            &[json!({
                "taskId": "task-1",
                "status": "running",
                "timed_out": true
            })],
        );
        assert_eq!(checkpoint.blocked_count, 1);
        assert_eq!(checkpoint.dispute_count, 1);
        assert_eq!(checkpoint.notes.blocked_count, 1);
        assert_eq!(checkpoint.notes.dispute_count, 1);
    }

    #[test]
    fn mcp_response_cap_limits_large_text() {
        let huge = "x".repeat(BlackboxServer::MCP_RESPONSE_CAP_BYTES + 1024);
        let capped = BlackboxServer::cap_response_text(&huge);
        assert!(capped.len() <= BlackboxServer::MCP_RESPONSE_CAP_BYTES);
        assert!(capped.contains("response truncated"));
    }

    #[tokio::test]
    async fn run_workflow_at_depth_rejects_past_ceiling() {
        // A direct smoke test for the fix driven by the self-audit
        // live validation: the subworkflow depth counter used to live
        // in a per-runner HashMap, so nested runners silently reset
        // it. Now it's threaded through run_workflow_at_depth so the
        // ceiling is enforced globally across the composition chain.
        use crate::workflow::{compile, engine, load_workflow};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        // Minimal valid workflow — doesn't actually matter since the
        // depth check short-circuits before any dispatch.
        let json = r#"{
            "name": "depth-test",
            "version": 1,
            "actors": {"a": {"kind": "executor", "brofile": "b"}},
            "nodes": {"N": {"actor": "a", "next": {"type": "terminal"}}},
            "start": "N"
        }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();

        // At exactly MAX_COMPOSITION_DEPTH: should proceed (no error
        // from depth check). We don't actually dispatch because there's
        // no brofile "b" on this test server — but we confirm the
        // depth check isn't the thing that errors it out.
        let at_ceiling = engine::run_workflow_at_depth(
            &server,
            &compiled,
            None,
            Some(1),
            engine::MAX_COMPOSITION_DEPTH,
            std::collections::HashMap::new(),
            serde_json::Map::new(),
            None,
        )
        .await;
        assert!(
            !at_ceiling
                .status
                .starts_with("error: subworkflow composition depth"),
            "at-ceiling depth should not be rejected by the depth guard; got: {}",
            at_ceiling.status
        );

        // Past ceiling: short-circuit with a depth-error status.
        let past_ceiling = engine::run_workflow_at_depth(
            &server,
            &compiled,
            None,
            Some(1),
            engine::MAX_COMPOSITION_DEPTH + 1,
            std::collections::HashMap::new(),
            serde_json::Map::new(),
            None,
        )
        .await;
        assert!(
            past_ceiling
                .status
                .starts_with("error: subworkflow composition depth"),
            "past-ceiling should error on depth; got: {}",
            past_ceiling.status
        );
        assert!(past_ceiling.events.is_empty());
        assert!(past_ceiling.arc_thread_id.is_none());
    }

    #[tokio::test]
    async fn bro_arc_cancel_trips_a_parked_wait_arc() {
        // End-to-end cancel: spawn an arc that immediately parks on a
        // long-timeout Wait, cancel it via the SharedState, observe
        // that run() returns with status=cancelled. No LLM dispatch
        // needed — the arc is hook-only and immediately blocks on the
        // wait.
        use crate::workflow::{compile, engine, load_workflow};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let json = r#"{
            "name": "cancel-smoke",
            "version": 1,
            "actors": {},
            "nodes": {
                "WaitFor": {
                    "actor": "",
                    "wait": {
                        "any_of": [{"signal": "never-arrives"}],
                        "timeout": "30s"
                    },
                    "next": {"type": "terminal"}
                }
            },
            "start": "WaitFor"
        }"#;
        let compiled = compile(load_workflow(json).unwrap()).unwrap();

        // Spawn the arc on a background task — it'll park inside the
        // Wait until either the timeout fires or our cancel trips.
        let server_state = server.state.clone();
        let run_handle = tokio::spawn(async move {
            let server2 = BlackboxServer::new(server_state);
            engine::run_workflow_with_initial_vars(
                &server2,
                &compiled,
                None,
                Some(50),
                serde_json::Map::new(),
            )
            .await
        });

        // Give the runner a moment to register the wait + cancel
        // token, then observe the registered token and trip it. Yield
        // a few times to let the task progress past wait registration
        // without hard-coding a timing assumption.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let token_count = server.state.arc_cancel_tokens.read().len();
            if token_count > 0 {
                break;
            }
        }

        // Cancel every registered arc (test fixture only spawns one).
        let arc_ids: Vec<String> = server
            .state
            .arc_cancel_tokens
            .read()
            .keys()
            .cloned()
            .collect();
        assert!(
            !arc_ids.is_empty(),
            "expected an arc cancel token to be registered after dispatch"
        );
        for arc_id in &arc_ids {
            let cancelled = server.state.cancel_arc(arc_id);
            assert!(cancelled, "cancel_arc returned false for live arc {arc_id}");
        }

        // The runner should release the wait and return with
        // status=cancelled.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle)
            .await
            .expect("runner did not exit within 5s of cancel")
            .expect("runner panicked");
        assert_eq!(result.status, "cancelled", "got: {}", result.status);

        // Token should have been unregistered at terminus.
        assert!(
            server.state.arc_cancel_tokens.read().is_empty(),
            "cancel token still registered after arc terminated"
        );
    }

    #[test]
    fn build_team_advisor_init_prompt_includes_charter_halt_exit_and_status_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let team = orchestration::team::Team {
            name: "migration-team".into(),
            teamplate: "tp".into(),
            members: vec![
                orchestration::team::TeamMember {
                    name: "executor".into(),
                    brofile: "codex-exec".into(),
                    session_id: None,
                    task_history: vec![],
                },
                orchestration::team::TeamMember {
                    name: "reviewer".into(),
                    brofile: "claude-review".into(),
                    session_id: None,
                    task_history: vec![],
                },
            ],
            advisor: None,
            project_dir: None,
            created_at: 0,
        };
        let advisor = orchestration::team::TeamAdvisor {
            name: "lead-advisor".into(),
            config: orchestration::team::TeamAdvisorConfig {
                brofile: "advisor-brofile".into(),
                alias: Some("lead-advisor".into()),
                charter: "keep the migration honest; reject fake phase boundaries".into(),
                context: Some("phase 2 of 3".into()),
                halt_conditions: vec![
                    "executor invents a phase boundary that masks coupling".into(),
                    "reviewer rubber-stamps a phase without adversarial read".into(),
                ],
                exit_conditions: vec!["all three phases land and are reviewed".into()],
                packet_id: Some("packet-abcdef12".into()),
                timeout_seconds: None,
                mode: orchestration::team::AdvisorMode::Blocking,
            },
            session_id: None,
            task_history: vec![],
        };

        let prompt = server.build_team_advisor_init_prompt(&team, &advisor);

        // Status schema — load-bearing for orchestrator parsing of advisor output.
        assert!(
            prompt.contains("Status: CONTINUE | ESCALATE | CHARTER_DRIFT | EXIT_MET | REPLACE_BRO"),
            "advisor init prompt missing canonical status schema: {prompt}"
        );
        assert!(prompt.contains("Rationale:"), "missing Rationale line");
        assert!(prompt.contains("Next step:"), "missing Next step line");

        // Charter, context, packet_id round-tripped verbatim.
        assert!(prompt.contains("keep the migration honest"));
        assert!(prompt.contains("phase 2 of 3"));
        assert!(prompt.contains("packet-abcdef12"));

        // Every halt and exit condition must survive as its own bullet.
        assert!(prompt.contains("- executor invents a phase boundary that masks coupling"));
        assert!(prompt.contains("- reviewer rubber-stamps a phase without adversarial read"));
        assert!(prompt.contains("- all three phases land and are reviewed"));

        // Team roster surfaces so the advisor knows who it is steering.
        assert!(prompt.contains("executor (codex-exec)"));
        assert!(prompt.contains("reviewer (claude-review)"));
        assert!(prompt.contains("migration-team"));
    }

    #[test]
    fn advisor_checkpoint_serializes_with_packet_entity_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let team = orchestration::team::Team {
            name: "demo".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: None,
            project_dir: None,
            created_at: 0,
        };
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "wait",
            &[
                json!({
                    "taskId": "task-a",
                    "status": "completed",
                    "bro": "exec",
                    "result": "ok"
                }),
                json!({
                    "taskId": "task-b",
                    "status": "running",
                    "bro": "reviewer",
                    "timed_out": true
                }),
            ],
        );
        let serialized = serde_json::to_value(&checkpoint).unwrap();

        // Fields the packet evaluator uses as predicate operands. If any of
        // these drift, every advisor packet in the wild breaks silently.
        for key in [
            "wait_kind",
            "team_name",
            "total_count",
            "completed_count",
            "failed_count",
            "running_count",
            "timed_out_count",
            "blocked_count",
            "dispute_count",
            "done_count",
            "members",
            "notes",
        ] {
            assert!(
                serialized.get(key).is_some(),
                "advisor checkpoint missing packet-facing field '{key}': {serialized}"
            );
        }

        assert_eq!(serialized["total_count"], 2);
        assert_eq!(serialized["completed_count"], 1);
        assert_eq!(serialized["running_count"], 1);
        assert_eq!(serialized["timed_out_count"], 1);
    }

    #[test]
    fn apply_advisor_packet_returns_rule_hit_for_checkpoint_entity() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        let packet_id = {
            let store = server.state.packets.read();
            let result = store
                .compile(&CompileParams {
                    domain: "advisor/demo-escalate".into(),
                    classification_lattice: Some(vec!["escalate".into(), "continue".into()]),
                    prefix_inference: Some(
                        [
                            ("escalate_".into(), "escalate".into()),
                            ("continue_".into(), "continue".into()),
                        ]
                        .into(),
                    ),
                    rules: json!([
                        {
                            "id": "escalate_any_blocked",
                            "antecedent": {"op": "Gt", "field": "blocked_count", "value": 0},
                            "consequent": "ESCALATE"
                        },
                        {
                            "id": "continue_default",
                            "classification": "continue",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "CONTINUE"
                        }
                    ]),
                    scope: Some("global".into()),
                    project: None,
                    source_ids: None,
                    rank_table: None,
                    rank_lookup_key: None,
                    threshold_table: None,
                    threshold_lookup_key: None,
                })
                .unwrap();
            // compile() returns "Packet packet-<id> compiled (...)" — extract id.
            result
                .split_whitespace()
                .find(|tok| tok.starts_with("packet-"))
                .unwrap()
                .to_string()
        };

        let team = orchestration::team::Team {
            name: "t".into(),
            teamplate: "tp".into(),
            members: vec![],
            advisor: None,
            project_dir: None,
            created_at: 0,
        };
        {
            let mut notes = server.state.notes.write();
            notes
                .create(&NoteParams {
                    kind: "blocked".into(),
                    body: "exec is stuck".into(),
                    task_id: Some("task-x".into()),
                    session_id: None,
                    project: None,
                    thread_id: None,
                    provider: None,
                    bro: Some("exec".into()),
                })
                .unwrap();
        }
        let checkpoint = server.build_advisor_checkpoint(
            &team,
            "wait",
            &[json!({"taskId": "task-x", "status": "running"})],
        );

        let verdict = server
            .apply_advisor_packet(&packet_id, &checkpoint)
            .unwrap();
        assert_eq!(verdict["match"], true);
        assert_eq!(verdict["ruleId"], "escalate_any_blocked");
        assert_eq!(verdict["classification"], "escalate");
    }

    #[test]
    fn arc_bound_warning_fires_on_residue_and_skips_system_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);

        {
            let store = server.state.packets.read();
            store
                .compile(&CompileParams {
                    domain: "content-classification/arc-bound".into(),
                    classification_lattice: Some(vec!["arc_bound".into(), "standing".into()]),
                    prefix_inference: Some(
                        [
                            ("arc_".into(), "arc_bound".into()),
                            ("standing_".into(), "standing".into()),
                        ]
                        .into(),
                    ),
                    rules: json!([
                        {
                            "id": "arc_named_migration",
                            "antecedent": {
                                "op": "StringContains",
                                "field": "content",
                                "needle": "3-tier migration",
                                "case_insensitive": true
                            },
                            "consequent": "ARC_BOUND"
                        },
                        {
                            "id": "standing_catchall",
                            "classification": "standing",
                            "emit": "fallback",
                            "antecedent": {"op": "True"},
                            "consequent": "STANDING"
                        }
                    ]),
                    scope: Some("global".into()),
                    project: None,
                    source_ids: None,
                    rank_table: None,
                    rank_lookup_key: None,
                    threshold_table: None,
                    threshold_lookup_key: None,
                })
                .unwrap();
        }

        let nag_arc = server.arc_bound_warning(None, "For the 3-tier migration, avoid touching X");
        assert!(
            nag_arc
                .as_deref()
                .is_some_and(|s| s.contains("arc-bound") && s.contains("bbox_pin")),
            "arc-bound content should produce a pin-steering nag: {nag_arc:?}"
        );

        let nag_standing = server.arc_bound_warning(None, "Prefer rustls over openssl");
        assert!(
            nag_standing.is_none(),
            "standing content should not trigger a nag: {nag_standing:?}"
        );

        let nag_system = server.arc_bound_warning(
            Some("bb-tool-reference"),
            "For the 3-tier migration, avoid touching X",
        );
        assert!(
            nag_system.is_none(),
            "system-generated entries must be exempt from the nag: {nag_system:?}"
        );
    }
}
