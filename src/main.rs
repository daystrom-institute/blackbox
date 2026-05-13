#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return
)]

#[cfg(test)]
#[path = "../eval/agents/check.rs"]
mod agent_eval_check;
mod artifacts;
#[cfg(test)]
#[path = "../eval/badgey/check.rs"]
mod badgey_eval_check;
mod chunker;
pub mod code_nav;
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
mod gap_closeout;
mod gap_spool;
mod git;
mod inbox;
mod index;
mod json_store;
mod knowledge;
mod lsp;
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
mod refactor;
mod render;
mod roadmap;
mod routing;
mod search;
mod server;
mod slack_channel_bindings;
mod slack_proposal_links;
mod slack_thread_store;
mod system_memory;
mod template;
#[cfg(test)]
mod tests;
mod threads;
mod tool_docs;
mod tools;
mod transcripts;
mod vectors;
mod watcher;
mod webhooks;
mod whiteboards;
mod workflow;

use blackbox::config;

mod util {
    pub use blackbox::util::*;
}

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use futures::{StreamExt, stream::Stream};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, InitializeRequestParams, InitializeResult,
    IntoContents, ListToolsResult, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ErrorData, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
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
use projects::{
    ProjectListResponse, ProjectRecord, ProjectRegisterParams, ProjectRegistry,
    ProjectRenameParams, ProjectUnregisterParams,
};
use providers::ProviderContext;
use roadmap::Roadmap;
use threads::Threads;

static AGENT_QUERY_EMBED_CACHE: OnceLock<RwLock<BTreeMap<String, Vec<f32>>>> = OnceLock::new();

impl BlackboxServer {
    const MCP_RESPONSE_CAP_BYTES: usize = 80 * 1024;

    fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: Self::bbox_tools()
                + Self::bro_tools()
                + tools::projects::router()
                + tools::notes::router()
                + tools::threads::router()
                + tools::refactor::router()
                + tools::code_nav::router()
                + tools::artifacts::router()
                + tools::packets::router()
                + tools::attention::router()
                + tools::graph::router()
                + tools::transcripts::router()
                + tools::sessions::router()
                + tools::knowledge::router()
                + tools::render::router()
                + tools::roadmap::router()
                + tools::whiteboards::router()
                + tools::badgey::router()
                + tools::agents::router()
                + tools::orchestrate::router()
                + tools::councils::router()
                + tools::roster::router()
                + tools::config::router()
                + tools::dispatch::router()
                + tools::mcp_surface::router()
                + tools::workspace::router(),
            surface: std::sync::OnceLock::new(),
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
        // Brofile and agent vertices live in the artifact catalog. They
        // don't naturally appear in the EdgeIndex's known_refs until a
        // DERIVED_FROM / SUPERSEDES edge points at them; until that
        // wire-up matures (design/agent-system.md §8.1), seed the
        // counts directly from the catalog so describe_schema reflects
        // installed artifacts.
        let catalog = self.state.artifacts.read();
        for (kind, key) in [
            (artifacts::ArtifactKind::Brofile, "brofile"),
            (artifacts::ArtifactKind::Agent, "agent"),
        ] {
            let params = artifacts::ArtifactListParams {
                kind: Some(kind),
                name: None,
                include_superseded: false,
            };
            if let Ok(entries) = catalog.list(&params) {
                let active = entries.iter().filter(|e| e.active).count();
                counts.insert(key.into(), active);
            }
        }
        counts
    }

    fn build_agent_schema_entries(&self) -> Vec<mcp_tools::describe_schema::AgentSchemaEntry> {
        use orchestration::agents::registry::AgentRegistry;
        let catalog = self.state.artifacts.read();
        let registry = AgentRegistry::new(&catalog);
        let filter = orchestration::agents::registry::ListFilter::default();
        let Ok(summaries) = registry.list(&filter) else {
            return Vec::new();
        };
        summaries
            .into_iter()
            .filter(|s| s.active)
            .filter_map(|s| {
                let (manifest, _) = registry.load_manifest_degraded(&s.name);
                let manifest = manifest?;
                let cost_str = match manifest.cost_class {
                    orchestration::agents::types::AgentCostClass::Cheap => "cheap",
                    orchestration::agents::types::AgentCostClass::Normal => "normal",
                    orchestration::agents::types::AgentCostClass::Expensive => "expensive",
                };
                let example = format!("bro_agent_dispatch(agent=\"{}\", args={{...}})", s.name);
                Some(mcp_tools::describe_schema::AgentSchemaEntry {
                    name: s.name,
                    version: s.version,
                    description: manifest.description,
                    when_to_use: manifest.when_to_use,
                    anti_patterns: manifest.anti_patterns,
                    cost_class: cost_str.to_string(),
                    dispatch_adapter: manifest.dispatch_adapter,
                    example_invocation: example,
                })
            })
            .collect()
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
        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters, _coerce_workspace) =
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
        let resume_lease = if is_resume {
            match try_acquire_resume_lease(
                &self.state.task_store,
                self.state.resume_leases.as_ref(),
                provider,
                &session_id,
            ) {
                Ok(lease) => Some(lease),
                Err(err) => return Err(err),
            }
        } else {
            None
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
            coerce_workspace: false,
        };
        let ambient_prompt = orch::apply_ambient(prompt, &ambient_ctx);
        let mut args = if is_resume {
            provider.build_resume_args(&session_id, &ambient_prompt, exec_opts.as_ref())
        } else {
            let final_prompt = orch::apply_brofile_lens(&ambient_prompt, lens.as_deref());
            provider.build_exec_args(
                &final_prompt,
                &session_id,
                cwd.as_deref(),
                exec_opts.as_ref(),
            )
        };

        let dispatch_filters = match resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            &task_id,
            brofile_filters.as_ref(),
            None,
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Err(e),
        };
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
            None,
            None,
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        if let Some(lease) = resume_lease {
            release_resume_lease_when_done(task.clone(), lease);
        }
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

    /// Register an arc cancel token chained to a parent arc/group
    /// token. Used by nested workflows so cancellation propagates
    /// down the composition tree.
    pub fn register_arc_cancel_token_child(
        &self,
        arc_id: &str,
        parent: &CancellationToken,
    ) -> CancellationToken {
        self.state.register_arc_cancel_token_child(arc_id, parent)
    }

    /// Drop the arc's cancel token. Called by the runner at terminus.
    pub fn unregister_arc_cancel_token(&self, arc_id: &str) {
        self.state.unregister_arc_cancel_token(arc_id);
    }

    fn rebuild_edge_index_from_stores(&self) {
        // Store mutations only affect structured edges. Re-projecting all
        // Tantivy docs here is a multi-GB path and can stack under concurrent
        // thread updates.
        rebuild_edge_index_from_shared(&self.state, false);
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

    /// Parse a tool-supplied spec field that nominally takes a JSON object
    /// but may arrive as a stringified JSON document (some MCP clients
    /// stringify nested objects when the schema doesn't pin `type: object`
    /// tightly). Accepts either form.
    fn parse_spec<T: serde::de::DeserializeOwned>(
        spec: Value,
        kind: &str,
    ) -> Result<T, CallToolResult> {
        let resolved = match spec {
            Value::String(s) => match serde_json::from_str::<Value>(&s) {
                Ok(v) => v,
                Err(e) => {
                    return Err(Self::err_text(&format!(
                        "{kind} spec parse failed: passed as string but not valid JSON: {e}"
                    )));
                }
            },
            other => other,
        };
        serde_json::from_value(resolved)
            .map_err(|e| Self::err_text(&format!("{kind} spec parse failed: {e}")))
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

impl orchestration::agents::adapter::AgentDispatchAdapter for BadgeyAgentAdapter {
    fn name(&self) -> &'static str {
        "badgey"
    }

    fn dispatch(
        &self,
        _manifest: &orchestration::agents::types::AgentManifest,
        args: Value,
        ctx: orchestration::agents::adapter::DispatchContext,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        orchestration::agents::adapter::AgentDispatchResult,
                        orchestration::agents::adapter::AgentDispatchError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let state = self.state.clone();
        Box::pin(async move {
            use orchestration::agents::adapter::{
                AgentDispatchError, AgentDispatchResult, DispatchDegraded,
            };
            use orchestration::agents::types::{AgentRef, AgentSession, MergedFilters};

            let server = BlackboxServer::new(state);
            let project_dir = args
                .get("project_dir")
                .and_then(Value::as_str)
                .map(String::from)
                .or(ctx.project_dir);
            let result = if let Some(badgey_id) = args.get("badgey_id").and_then(Value::as_str) {
                let prompt = args
                    .get("prompt")
                    .or_else(|| args.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if prompt.trim().is_empty() {
                    return Err(AgentDispatchError::BadInput {
                        message: "badgey adapter resume requires args.prompt or args.question"
                            .to_string(),
                    });
                }
                server
                    .badgey_resume_internal(badgey_id, prompt, None)
                    .await
                    .map_err(|message| AgentDispatchError::AdapterFailed { message })?
            } else {
                let brief = args
                    .get("brief")
                    .or_else(|| args.get("prompt"))
                    .or_else(|| args.get("question"))
                    .and_then(Value::as_str)
                    .map(String::from);
                server
                    .badgey_exec_internal(project_dir.clone(), brief, ctx.bro_label_prefix.clone())
                    .await
                    .map_err(|message| AgentDispatchError::AdapterFailed { message })?
            };
            let session_id = result
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let provider = result
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let task_id = result
                .get("task_id")
                .and_then(Value::as_str)
                .map(String::from);
            let degraded = result.get("degraded").map(|_| DispatchDegraded {
                reasons: vec!["badgey reported degraded status".to_string()],
            });
            let merged_filters = result
                .get("merged_filters")
                .and_then(|value| serde_json::from_value::<MergedFilters>(value.clone()).ok())
                .unwrap_or_default();
            Ok(AgentDispatchResult {
                session: AgentSession {
                    session_id,
                    provider,
                    project_dir,
                    agent: AgentRef {
                        name: "badgey".to_string(),
                        version: 1,
                    },
                    task_id,
                },
                resolved_brofile: Some("badgey-persona".to_string()),
                merged_filters,
                degraded,
            })
        })
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
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams, apply_with as apply_packet_with, packet_matches_query, packet_summary,
};
use refactor::{
    RefactorApplyParams, RefactorPlanParams, RefactorProjectRefsParams, RefactorRunParams,
    RefactorStatusParams,
};
pub(crate) use server::*;
use threads::{ThreadListParams, ThreadParams};
pub(crate) use tools::badgey_adapter::*;
pub(crate) use tools::bro_helpers::*;
pub(crate) use tools::bro_params::*;
pub(crate) use tools::bro_runtime_params::*;

#[tool_router(router = bbox_tools)]
impl BlackboxServer {}

/// Inbound `proposal-approved` / `proposal-clarify` signal hook for
/// the Slack daily brief. Fires when a reaction (approve) or thread
/// reply (clarify) lands on a posted triage proposal AND no workflow
/// was waiting for the signal. Resolves the message back to its
/// SlackProposalLink and posts a threaded acknowledgement in Slack.
/// The actual apply work and the bro_resume refinement loop drop in
/// here once the foreach-driven Badgey workflow stack is wired —
/// then this hook becomes the call site for
/// `badgey_apply_proposal_internal` (approve) and `bro_resume`
/// (clarify). Errors are logged, never bubbled — best-effort
/// observability path.
async fn try_slack_proposal_signal_hook(
    signal: &str,
    state: &Arc<SharedState>,
    correlate: &serde_json::Map<String, Value>,
    entity: &Value,
) {
    let thread_ts = correlate
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thread_ts.is_empty() {
        return;
    }
    let team_id = entity.get("team_id").and_then(|v| v.as_str()).unwrap_or("");
    let channel_id = entity.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    if team_id.is_empty() || channel_id.is_empty() {
        return;
    }
    let link = match state
        .slack_proposal_links
        .lookup_by_msg(team_id, channel_id, thread_ts)
    {
        Some(l) => l,
        None => return,
    };
    let user = entity
        .get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("someone");
    let bbox_user = entity
        .get("bbox_user")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let acknowledger = if bbox_user.is_empty() {
        format!("<@{user}>")
    } else {
        format!("<@{user}> ({bbox_user})")
    };
    let text = match signal {
        "proposal-approved" => format!(
            ":white_check_mark: Approved by {acknowledger}. \
             Apply path lands with the foreach-driven Badgey workflow — \
             logged for follow-up. (proposal `{}`)",
            link.proposal_id
        ),
        "proposal-clarify" => {
            let reply_text = entity.get("text").and_then(|v| v.as_str()).unwrap_or("");
            // Char-aware truncation — naive byte slicing can panic on
            // non-ASCII at codepoint boundaries.
            let snippet = if reply_text.chars().count() > 120 {
                let truncated: String = reply_text.chars().take(120).collect();
                format!("{truncated}…")
            } else {
                reply_text.to_string()
            };
            format!(
                ":speech_balloon: Heard your follow-up from {acknowledger}{}. \
                 Refinement loop lands with the foreach-driven Badgey workflow — \
                 the proposal author isn't a live agent yet. \
                 (proposal `{}`)",
                if snippet.is_empty() {
                    String::new()
                } else {
                    format!(": _{snippet}_")
                },
                link.proposal_id,
            )
        }
        _ => return,
    };
    if signal == "proposal-approved" {
        if let Err(e) = state
            .slack_proposal_links
            .bump_version(team_id, channel_id, thread_ts)
        {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                "bump_version on slack proposal link failed: {e}"
            );
        }
    }
    let token = match std::env::var("SLACK_BOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            tracing::info!(
                proposal_id = %link.proposal_id,
                signal = %signal,
                "proposal hook fired but SLACK_BOT_TOKEN unset; skipping ack post"
            );
            return;
        }
    };
    let req_body = json!({
        "channel": channel_id,
        "thread_ts": thread_ts,
        "text": text,
        "mrkdwn": true,
    });
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                "building reqwest client for Slack ack failed: {e}"
            );
            return;
        }
    };
    match client
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(&token)
        .json(&req_body)
        .send()
        .await
    {
        Ok(resp) => {
            let parsed: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %link.proposal_id,
                        "parsing Slack ack response failed: {e}"
                    );
                    return;
                }
            };
            if !parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                tracing::warn!(
                    proposal_id = %link.proposal_id,
                    signal = %signal,
                    "Slack ack post returned ok=false: {parsed}"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                signal = %signal,
                "Slack ack post failed: {e}"
            );
        }
    }
}

#[tool_router(router = bro_tools)]
impl BlackboxServer {}

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

// ---------------------------------------------------------------------------
// Helper methods on BlackboxServer
// ---------------------------------------------------------------------------

impl BlackboxServer {}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Blackbox: unified transcript search, knowledge management, and multi-provider agent orchestration")
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let surface_str = if let Some(parts) = context.extensions.get::<http::request::Parts>() {
            server::surface::extract_surface_from_uri(parts.uri.query())
        } else {
            "default"
        };
        let entity = server::surface::build_surface_entity(surface_str, None);
        let decision = {
            let packets = self.state.packets.read();
            server::surface::evaluate_tool_surface(&packets, entity, None::<&str>)
        };
        if matches!(
            decision.verdict,
            server::surface::ToolSurfaceVerdict::Deny { .. }
        ) {
            let reason = match &decision.verdict {
                server::surface::ToolSurfaceVerdict::Deny { reason } => {
                    reason.as_deref().unwrap_or("surface denied")
                }
                _ => unreachable!(),
            };
            return Err(ErrorData::internal_error(
                format!("tool surface denied: {}", reason),
                None,
            ));
        }
        let _ = self.surface.set(Arc::from(surface_str));
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        // Canonical setter is `initialize`; fallback for paths that bypass it.
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = server::surface::build_surface_entity(surface, None);
        let packets = self.state.packets.read();
        let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
        drop(packets);
        let universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        if !server::surface::tool_visible(name, &decision, &universe) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Canonical setter is `initialize`; fallback for paths that bypass it.
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = server::surface::build_surface_entity(surface, None);
        let packets = self.state.packets.read();
        let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
        drop(packets);
        let universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let all = self.tool_router.list_all();
        let filtered = server::surface::filter_tools(&all, &decision, &universe);
        Ok(ListToolsResult {
            tools: filtered,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let (surface, decision, universe) = {
            // Canonical setter is `initialize`; fallback for paths that bypass it.
            let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
            let entity = server::surface::build_surface_entity(surface, None);
            let packets = self.state.packets.read();
            let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
            let universe: Vec<String> = self
                .tool_router
                .list_all()
                .iter()
                .map(|t| t.name.to_string())
                .collect();
            (surface.to_string(), decision, universe)
        };
        if !server::surface::tool_visible(&request.name, &decision, &universe) {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!(
                    "tool not available on surface '{}': {}",
                    surface, request.name
                ),
                None,
            ));
        }
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }
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

    // Load configuration
    let cfg = config::load()?;
    let cfg_arc = Arc::new(RwLock::new(cfg.clone()));

    // Transcript index roots - from config or env
    let roots: Vec<(String, PathBuf)> = if let Some(ref roots_str) = cfg.transcripts.roots {
        roots_str
            .split(',')
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
    } else if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
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

    let codex_root = cfg
        .transcripts
        .codex_root
        .map(|p| {
            if p.to_string_lossy().starts_with('~') {
                home.join(&p.to_string_lossy()[2..])
            } else {
                p
            }
        })
        .or_else(|| {
            std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
                .ok()
                .map(PathBuf::from)
        })
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() {
                Some(default)
            } else {
                None
            }
        });

    let index_path = cfg.paths.index_path.clone();

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

    let projects_path = cfg.paths.projects_path.clone();
    let kb_path = cfg.paths.knowledge_path.clone();
    let th_path = cfg.paths.threads_path.clone();
    let rm_path = cfg.paths.roadmap_path.clone();
    let mut idx = TranscriptIndex::open_or_create(
        &index_path,
        roots,
        codex_root,
        projects_path.clone(),
        kb_path.clone(),
        th_path.clone(),
        rm_path.clone(),
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
    let bbox_url = format!("http://{}:{}/mcp", cfg.daemon.bind, cfg.daemon.port);
    let bbox_mcp_name = cfg.daemon.mcp_name.clone();
    // Export for provider arg-builders so they can inject `--mcp-config`
    // etc. at dispatch time — ensures dispatched subprocesses see
    // blackbox regardless of which config file their CLI inherits.
    unsafe {
        std::env::set_var("BLACKBOX_MCP_URL", &bbox_url);
    }
    unsafe {
        std::env::set_var("BLACKBOX_MCP_NAME", &bbox_mcp_name);
    }
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

    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());
    if let Err(err) = idx.index_threads_store(&th) {
        tracing::warn!(error = %err, "thread index sync failed; will retry on next reindex cycle");
    }

    let roadmap_store = Roadmap::open(&rm_path)?;
    tracing::info!("Roadmap store: {}", rm_path.display());

    let notes_path = cfg.paths.notes_path.clone();
    let notes_store = Notes::open(&notes_path)?;
    tracing::info!("Notes store: {}", notes_path.display());

    let pins_path = cfg.paths.pins_path.clone();
    let pins_store = Pins::open(&pins_path)?;
    tracing::info!("Pins store: {}", pins_path.display());

    let packets_dir = cfg.paths.packets_dir.clone();
    let packets_store = Packets::open(&packets_dir)?;
    tracing::info!("Packets store: {}", packets_dir.display());

    let artifacts_dir = cfg.paths.artifacts_dir.clone();
    let agent_adapter_registry = Arc::new(RwLock::new(
        orchestration::agents::adapter::AgentAdapterRegistry::new(),
    ));
    let artifacts_store = artifacts::ArtifactCatalog::open(&artifacts_dir)?;
    tracing::info!("Artifact catalog: {}", artifacts_store.root().display());
    match artifacts_store.backfill_content_hashes() {
        Ok(r) => {
            if r.active_updated > 0 || r.version_updated > 0 || r.missing_artifacts > 0 {
                tracing::info!(
                    "Artifact hash backfill: {} active updated, {} version updated, {} missing payloads",
                    r.active_updated,
                    r.version_updated,
                    r.missing_artifacts
                );
            }
        }
        Err(e) => tracing::warn!("Artifact hash backfill failed: {e:#}"),
    }

    // Orchestration state
    let store_dir = cfg.paths.bro_home.clone();
    let task_ttl = cfg.daemon.task_ttl_ms;
    let task_store = TaskStore::load(&store_dir, task_ttl);
    let badgey_proposals = Arc::new(orchestration::badgey::ProposalStore::new(
        store_dir.clone(),
    )?);
    let badgey_journal = Arc::new(orchestration::badgey::ActionJournal::new(
        store_dir.clone(),
    )?);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);

    // Spawn background reindex thread
    let reindex_interval = cfg.index.reindex_interval_secs;
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
    let bind_host = cfg.daemon.bind.clone();
    let bind_is_loopback = is_loopback_bind(&bind_host);

    let edge_index = if cfg.index.edge_index_boot_rebuild {
        edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
            index: &idx,
            knowledge: &kb,
            threads: &th,
            notes: &notes_store,
            task_store: &task_store,
            roadmap: &roadmap_store,
            edges_dir: edge_index::edges_dir_from_bro_store(&store_dir),
            include_tantivy_projection: false,
        })
    } else {
        tracing::info!(
            "startup EdgeIndex rebuild deferred (set BLACKBOX_EDGE_INDEX_BOOT_REBUILD=1 to restore eager rebuild)"
        );
        edge_index::EdgeIndex::default()
    };

    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        kb: RwLock::new(kb),
        roadmap: RwLock::new(roadmap_store),
        threads: RwLock::new(th),
        notes: RwLock::new(notes_store),
        pins: RwLock::new(pins_store),
        projects: RwLock::new(projects_store),
        packets: RwLock::new(packets_store),
        artifacts: RwLock::new(artifacts_store),
        bbox_watcher: std::sync::Mutex::new(None),
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
        agent_adapter_registry: agent_adapter_registry.clone(),
        badgey_registry: Arc::new(orchestration::badgey::BadgeyRegistry::new()),
        badgey_proposals,
        badgey_journal,
        slack_thread_store: Arc::new(
            slack_thread_store::SlackThreadStore::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack thread store at {store_dir:?}: {e}")),
        ),
        slack_channel_bindings: Arc::new(
            slack_channel_bindings::SlackChannelBindings::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack channel bindings at {store_dir:?}: {e}")),
        ),
        slack_proposal_links: Arc::new(
            slack_proposal_links::SlackProposalLinks::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack proposal links at {store_dir:?}: {e}")),
        ),
        lsp_sessions: lsp::LspSessionManager::with_lsp_config(&cfg.lsp),
        config: cfg_arc.clone(),
    });
    shared
        .agent_adapter_registry
        .write()
        .register(Arc::new(BadgeyAgentAdapter {
            state: shared.clone(),
        }));
    restore_badgey_registry_from_notes(&shared);
    recover_badgey_non_terminal_state(&shared);
    std::thread::Builder::new()
        .name("blackbox-vectors-warmup".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let store = vectors::global();
            tracing::info!(
                partitions = store.partition_count(),
                elapsed_ms = started.elapsed().as_millis(),
                "vector store warmed"
            );
        })
        .map_err(|e| anyhow::anyhow!("spawning vector store warmup thread: {e}"))?;
    embed_queue::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    embed_queue::install_contradiction_state(shared.clone());
    embed_queue::install(embed::queue::EmbedQueueHandle::start_default());

    // Watch the tantivy corpus and rebuild the EdgeIndex whenever new docs
    // land via the auto-reindex thread (60s poll interval is sufficient
    // since the reindex tick is 120s by default).
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));

    // Start .bbox/ filesystem watcher for all registered projects.
    {
        let project_roots: Vec<(String, std::path::PathBuf)> = shared
            .projects
            .read()
            .list()
            .into_iter()
            .map(|r| (r.project_id, std::path::PathBuf::from(r.canonical_path)))
            .collect();
        let catalog = Arc::new(shared.artifacts.read().clone());
        match watcher::BbxWatcher::start(project_roots, catalog) {
            Ok(w) => {
                *shared.bbox_watcher.lock().unwrap() = Some(w);
                tracing::info!(".bbox/ artifact watcher started");
            }
            Err(e) => tracing::warn!(".bbox/ artifact watcher failed to start: {e:#}"),
        }
    }

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
                if path.extension() != Some(OsStr::new("json")) {
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
    let port = cfg.daemon.port;

    let ct = CancellationToken::new();
    let server_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_stateful_mode(true);

    let shared_for_mcp = shared.clone();
    let session_keep_alive = cfg.daemon.mcp_session_keepalive_secs;
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive =
        Some(std::time::Duration::from_secs(session_keep_alive));
    let mcp_service: StreamableHttpService<BlackboxServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            session_manager.into(),
            server_config,
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

    let shutdown_grace = std::time::Duration::from_secs(cfg.daemon.shutdown_grace_secs);
    let signal_ct = ct.clone();
    #[cfg(unix)]
    {
        let shared_for_hup = shared.clone();
        tokio::spawn(async move {
            let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("install SIGHUP handler");
            loop {
                let _ = sighup.recv().await;
                match config::load() {
                    Ok(new_cfg) => {
                        let old_cfg = shared_for_hup.config.read();
                        if old_cfg.daemon.port != new_cfg.daemon.port
                            || old_cfg.daemon.bind != new_cfg.daemon.bind
                        {
                            tracing::warn!(
                                "SIGHUP reload changed bind/port; requires daemon restart"
                            );
                        }
                        drop(old_cfg);
                        *shared_for_hup.config.write() = new_cfg;
                    }
                    Err(e) => {
                        tracing::warn!("SIGHUP reload failed: {e}");
                    }
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async {});
    }

    tokio::spawn(async move {
        // Wait for either Ctrl-C (interactive) or SIGTERM (systemd
        // stop). Without the SIGTERM branch, `systemctl stop` would
        // not signal graceful shutdown and would rely on the
        // TimeoutStopSec SIGKILL.
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        signal_ct.cancel();
    });

    let graceful_ct = ct.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        graceful_ct.cancelled().await;
    });
    tokio::select! {
        result = server => result?,
        _ = async {
            ct.cancelled().await;
            tokio::time::sleep(shutdown_grace).await;
        } => {
            tracing::warn!(
                grace_secs = shutdown_grace.as_secs(),
                "HTTP graceful shutdown timed out; forcing daemon shutdown path"
            );
        }
    }

    // Persist tasks on shutdown
    embed_queue::shutdown();
    // Tear down long-lived LSP sessions before persistence so JDTLS
    // and friends get a chance to write their workspace caches and
    // exit cleanly. shutdown_all is best-effort and bounded.
    shared.lsp_sessions.shutdown_all();
    shared.task_store.read().persist(&store_dir);
    // Best-effort vector-partition force-flush with a short timeout.
    // The earlier unconditional `vectors::global().flush_all()` could
    // block here for tens of seconds if any embed worker was holding a
    // partition write lock for a mid-flight voyage batch — long enough
    // to push systemd past TimeoutStopSec=90 and trigger SIGKILL,
    // which is worse than just leaving the WAL to replay on next start.
    // Spawn it on a thread + join with a short cap; if it doesn't
    // finish in time, drop on the floor and exit cleanly. The next
    // daemon start runs `rebuild_from_wal` which is correct (the WAL
    // was sync'd per batch) just slow.
    let flush_handle = std::thread::spawn(|| vectors::global().flush_all());
    let flush_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < flush_deadline {
        if flush_handle.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if flush_handle.is_finished() {
        if let Err(err) = flush_handle.join().expect("flush thread panic") {
            tracing::warn!(error = %err, "vector partition force-flush on shutdown failed");
        }
    } else {
        tracing::warn!(
            "vector partition force-flush on shutdown timed out after 5s; \
             next start will rebuild derived files from WAL"
        );
        // Detach; the OS reaps it when the process exits.
    }
    tracing::info!("blackboxd shut down");
    Ok(())
}
