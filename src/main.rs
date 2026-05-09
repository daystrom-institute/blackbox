#[cfg(test)]
#[path = "../eval/agents/check.rs"]
mod agent_eval_check;
mod artifacts;
#[cfg(test)]
#[path = "../eval/badgey/check.rs"]
mod badgey_eval_check;
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
#[cfg(test)]
mod tests;
mod threads;
mod tool_docs;
mod tools;
mod util;
mod vectors;
mod webhooks;
mod whiteboards;
mod workflow;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

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
use serde_json::{json, Map, Value};
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
    ProjectListResponse, ProjectRecord, ProjectRegisterParams, ProjectRegistry, ProjectRenameParams,
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
                + tools::agents::router(),
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

    fn inspect_extra_properties(
        &self,
        r: &crate::entity_ref::EntityRef,
    ) -> anyhow::Result<Option<BTreeMap<String, String>>> {
        use crate::entity_ref::EntityRef;
        match r {
            EntityRef::Knowledge { id } => Ok(self.state.kb.read().entry(id).map(|entry| {
                let mut properties = BTreeMap::new();
                properties.insert("id".into(), entry.id.clone());
                properties.insert("title".into(), entry.title.clone());
                properties.insert("content".into(), entry.content.clone());
                properties.insert("category".into(), format!("{:?}", entry.category));
                properties.insert("scope".into(), format!("{:?}", entry.scope));
                properties.insert("status".into(), format!("{:?}", entry.status));
                properties.insert("approval".into(), format!("{:?}", entry.approval));
                if let Some(project) = &entry.project {
                    properties.insert("project".into(), project.clone());
                }
                if let Some(supersedes) = &entry.supersedes {
                    properties.insert("supersedes".into(), supersedes.clone());
                }
                properties
            })),
            EntityRef::Thread { thread_id } => Ok(self
                .state
                .threads
                .read()
                .all()
                .iter()
                .find(|thread| thread.id == *thread_id)
                .map(|thread| {
                    let mut properties = BTreeMap::new();
                    properties.insert("thread_id".into(), thread.id.clone());
                    properties.insert("topic".into(), thread.topic.clone());
                    properties.insert("project".into(), thread.project.clone());
                    properties.insert("status".into(), format!("{:?}", thread.status));
                    if let Some(name) = &thread.name {
                        properties.insert("name".into(), name.clone());
                    }
                    properties
                })),
            EntityRef::Note { note_id } => Ok(self
                .state
                .notes
                .read()
                .all()
                .iter()
                .find(|note| note.id == *note_id)
                .map(|note| {
                    let mut properties = BTreeMap::new();
                    properties.insert("note_id".into(), note.id.clone());
                    properties.insert("kind".into(), format!("{:?}", note.kind));
                    properties.insert("body".into(), note.body.clone());
                    properties.insert("created_at".into(), note.created_at.clone());
                    if let Some(task_id) = &note.task_id {
                        properties.insert("task_id".into(), task_id.clone());
                    }
                    if let Some(thread_id) = &note.thread_id {
                        properties.insert("thread_id".into(), thread_id.clone());
                    }
                    properties
                })),
            EntityRef::Whiteboard { board_id } => {
                Ok(self.state.whiteboards.get(board_id).map(|board| {
                    let board = board.read();
                    let mut properties = BTreeMap::new();
                    properties.insert("board_id".into(), board.id.clone());
                    properties.insert("topic".into(), board.topic.clone());
                    properties.insert("project".into(), board.project.clone());
                    properties.insert("phase".into(), format!("{:?}", board.phase));
                    properties
                }))
            }
            EntityRef::Brofile { name } => {
                Ok(
                    orch::brofile::list_brofiles("global", &self.state.store_dir, None)
                        .into_iter()
                        .find(|brofile| brofile.name == *name)
                        .map(|brofile| {
                            let mut properties = BTreeMap::new();
                            properties.insert("name".into(), brofile.name);
                            properties.insert("provider".into(), brofile.provider.as_str().into());
                            if let Some(model) = brofile.model {
                                properties.insert("model".into(), model);
                            }
                            if let Some(effort) = brofile.effort {
                                properties.insert("effort".into(), effort);
                            }
                            properties
                        }),
                )
            }
            EntityRef::Agent { name, version } => {
                let catalog = self.state.artifacts.read();
                let meta = catalog
                    .metadata_for(artifacts::ArtifactKind::Agent, name)
                    .ok()
                    .flatten();
                let meta = match meta {
                    Some(m) if m.active => m,
                    _ => return Ok(None),
                };
                if meta.version != format!("{version}") {
                    return Ok(None);
                }
                let artifact_value = catalog
                    .load_artifact_value(artifacts::ArtifactKind::Agent, name)
                    .ok()
                    .flatten();
                let mut properties = BTreeMap::new();
                properties.insert("name".into(), name.clone());
                properties.insert("version".into(), version.to_string());
                if let Some(v) = &artifact_value {
                    let manifest = v.get("manifest").unwrap_or(v);
                    if let Some(desc) = manifest.get("description").and_then(|d| d.as_str()) {
                        properties.insert("description".into(), desc.to_string());
                    }
                    if let Some(bro) = manifest.get("brofile_ref").and_then(|b| b.as_str()) {
                        properties.insert("brofile_ref".into(), bro.to_string());
                    }
                    if let Some(wtu) = manifest.get("when_to_use").and_then(|w| w.as_array()) {
                        properties.insert(
                            "when_to_use".into(),
                            wtu.iter()
                                .filter_map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join("; "),
                        );
                    }
                }
                Ok(Some(properties))
            }
            _ => self.state.idx.read().entity_properties(&r.to_string()),
        }
    }

    fn inspect_entity_exists(
        &self,
        r: &crate::entity_ref::EntityRef,
        extra_properties: Option<&BTreeMap<String, String>>,
    ) -> bool {
        if extra_properties.is_some() {
            return true;
        }
        let edge_index = self.state.edge_index.read();
        !edge_index.forward_edges(r).is_empty() || !edge_index.reverse_edges(r).is_empty()
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

    /// Trigger cancellation for a running arc.
    pub fn cancel_arc(&self, arc_id: &str) -> bool {
        self.state.cancel_arc(arc_id)
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
    apply_with as apply_packet_with, packet_matches_query, packet_summary,
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams,
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
            None,
            None,
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
        description = "Continue an existing session with a follow-up. Single-flight per provider session."
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
        let resume_lease = match try_acquire_resume_lease(
            &self.state.task_store,
            self.state.resume_leases.as_ref(),
            provider,
            &session_id,
        ) {
            Ok(lease) => lease,
            Err(err) => return Self::err_text(&err),
        };

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
            None,
            None,
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        release_resume_lease_when_done(task.clone(), resume_lease);

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
                    let resume_lease = match try_acquire_resume_lease(
                        &self.state.task_store,
                        self.state.resume_leases.as_ref(),
                        brofile.provider,
                        sid,
                    ) {
                        Ok(lease) => lease,
                        Err(err) => {
                            launched.push(json!({
                                "bro": member.name,
                                "error": err,
                            }));
                            continue;
                        }
                    };
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
                        None,
                        None,
                    );
                    cleanup_policy_file_when_done(t.clone(), df.policy_file);
                    release_resume_lease_when_done(t.clone(), resume_lease);
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
                    None,
                    None,
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

        #[derive(Default)]
        struct AgentDashboardMetrics {
            dispatch_count: u64,
            success_count: u64,
            failure_count: u64,
            elapsed_ms_total: u64,
            elapsed_count: u64,
            cost_usd_total: f64,
        }

        let mut agent_metrics: BTreeMap<String, AgentDashboardMetrics> = BTreeMap::new();
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
                if let Some(label) = inner.agent_label.as_ref() {
                    let metrics = agent_metrics.entry(label.clone()).or_default();
                    metrics.dispatch_count += 1;
                    match inner.status {
                        orch::TaskStatus::Completed => metrics.success_count += 1,
                        orch::TaskStatus::Failed | orch::TaskStatus::Cancelled => {
                            metrics.failure_count += 1;
                        }
                        orch::TaskStatus::Running => {}
                    }
                    if let Some(done) = inner.completed_at {
                        metrics.elapsed_ms_total += done.saturating_sub(inner.started_at);
                        metrics.elapsed_count += 1;
                    }
                    if let Some(cost) = inner.cost_usd {
                        metrics.cost_usd_total += cost;
                    }
                }
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
                if let Some(ref label) = inner.bro_label {
                    entry["broLabel"] = Value::String(label.clone());
                }
                if let Some(ref label) = inner.agent_label {
                    entry["agentLabel"] = Value::String(label.clone());
                }
                (inner.started_at, entry)
            })
            .collect();
        with_ts.sort_by(|a, b| b.0.cmp(&a.0));
        let entries: Vec<Value> = with_ts.into_iter().take(limit).map(|(_, e)| e).collect();
        let agents: BTreeMap<String, Value> = agent_metrics
            .into_iter()
            .map(|(label, metrics)| {
                let avg_elapsed_ms = if metrics.elapsed_count == 0 {
                    None
                } else {
                    Some(metrics.elapsed_ms_total / metrics.elapsed_count)
                };
                (
                    label,
                    json!({
                        "dispatch_count": metrics.dispatch_count,
                        "success_count": metrics.success_count,
                        "failure_count": metrics.failure_count,
                        "avg_elapsed_ms": avg_elapsed_ms,
                        "cost_usd_total": (metrics.cost_usd_total * 10000.0).round() / 10000.0,
                    }),
                )
            })
            .collect();

        Self::ok_json(&json!({"count": entries.len(), "tasks": entries, "agents": agents}))
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
        {
            let inner = task.inner.lock();
            if inner.provider == Provider::Workflow {
                let _ = self.state.cancel_arc(&inner.session_id);
            }
        }
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
        name = "bro_slack_bind",
        description = "Bind a Slack channel to a bbox project. The binding scopes inbound Slack→badgey activity to a single project and gives the daily-triage cron a per-channel home for proposal posts. Channel id (C-prefix) is the stable lookup key; rename-safe. Actions: bind, unbind, list, lookup. Project accepts absolute path or 8-hex project_id from the registry."
    )]
    fn bro_slack_bind(&self, Parameters(p): Parameters<BroSlackBindParams>) -> CallToolResult {
        let store = &self.state.slack_channel_bindings;
        match p.action.as_str() {
            "bind" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t.to_string(),
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c.to_string(),
                    None => return Self::err_text("channel_id is required"),
                };
                let project_input = match p.project.as_deref().filter(|s| !s.is_empty()) {
                    Some(pr) => pr.to_string(),
                    None => return Self::err_text("project is required"),
                };
                let registry = self.state.projects.read();
                let records = registry.list();
                // Mirror ProjectRegistry's symlink-resolving behavior so
                // bind accepts aliases the registry already collapsed at
                // register time. Without this, a user who registered
                // `/repo/foo` (resolved target) but binds the symlink
                // `/home/me/foo` would see project_id=None and miss
                // rename/preflight scoping later. Refuse paths that
                // exist on disk but are non-directories (file, etc.)
                // — those would silently bind nonsense.
                let canonical_input = match entity_ref::canonical_input_path(&project_input) {
                    Ok(p) => Some(p.to_string_lossy().into_owned()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Self::err_text(&format!(
                            "project '{project_input}' is not a usable directory: {e}"
                        ));
                    }
                };
                let resolved = records.iter().find(|r| {
                    r.canonical_path == project_input
                        || r.project_id == project_input
                        || canonical_input
                            .as_deref()
                            .is_some_and(|c| c == r.canonical_path)
                });
                let (project_dir, project_id) = match resolved {
                    Some(rec) => (rec.canonical_path.clone(), Some(rec.project_id.clone())),
                    None => {
                        let path_buf = std::path::PathBuf::from(&project_input);
                        if !path_buf.is_absolute() {
                            return Self::err_text(&format!(
                                "project '{project_input}' is not registered and not an absolute path; register via bbox_project_register first"
                            ));
                        }
                        // Store the canonicalized form when fs resolution
                        // succeeded (symlink-stable storage); fall back
                        // to the operator's literal absolute path when
                        // it doesn't exist yet on disk.
                        let stored = canonical_input.unwrap_or_else(|| project_input.clone());
                        (stored, None)
                    }
                };
                drop(registry);
                // Preserve any badgey_id from a prior bind so re-binding
                // an already-active channel doesn't orphan its system
                // BadgeyInstance. The instance lifecycle is unbind-only;
                // rebind updates project/name/path in place.
                let prior_badgey_id = store
                    .lookup(&team_id, &channel_id)
                    .and_then(|b| b.badgey_id);
                let binding = slack_channel_bindings::ChannelBinding {
                    team_id: team_id.clone(),
                    channel_id: channel_id.clone(),
                    channel_name: p.channel_name.clone(),
                    project_dir: project_dir.clone(),
                    project_id: project_id.clone(),
                    registered_at: util::now_iso(),
                    registered_by: p.registered_by.clone(),
                    badgey_id: prior_badgey_id,
                };
                if let Err(e) = store.bind(binding.clone()) {
                    return Self::err_text(&format!("bind failed: {e}"));
                }
                Self::ok_json(&json!({
                    "bound": true,
                    "binding": binding,
                }))
            }
            "unbind" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t,
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c,
                    None => return Self::err_text("channel_id is required"),
                };
                match store.unbind(team_id, channel_id) {
                    Ok(Some(prior)) => {
                        // Dismiss the system Badgey instance that was
                        // serving this channel, if any. Best-effort:
                        // dismissal failure (e.g. instance already gone)
                        // is logged but doesn't fail the unbind.
                        let mut dismissed_badgey: Option<String> = None;
                        if let Some(ref bid) = prior.badgey_id {
                            if let Ok(parsed) =
                                bid.parse::<orchestration::badgey::types::BadgeyId>()
                            {
                                match self.state.badgey_registry.dismiss(&parsed) {
                                    Ok(_) => dismissed_badgey = Some(bid.clone()),
                                    Err(e) => tracing::warn!(
                                        badgey_id = %bid,
                                        "unbind: dismissing system Badgey instance failed: {e}"
                                    ),
                                }
                            }
                        }
                        Self::ok_json(&json!({
                            "unbound": true,
                            "prior": prior,
                            "dismissed_badgey": dismissed_badgey,
                        }))
                    }
                    Ok(None) => Self::ok_json(&json!({"unbound": false})),
                    Err(e) => Self::err_text(&format!("unbind failed: {e}")),
                }
            }
            "list" => {
                let bindings = store.list(p.team_filter.as_deref(), p.project_filter.as_deref());
                Self::ok_json(&json!({"bindings": bindings}))
            }
            "lookup" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t,
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c,
                    None => return Self::err_text("channel_id is required"),
                };
                match store.lookup(team_id, channel_id) {
                    Some(b) => Self::ok_json(&json!({"found": true, "binding": b})),
                    None => Self::ok_json(&json!({"found": false})),
                }
            }
            other => Self::err_text(&format!("Unknown bro_slack_bind action: {other}")),
        }
    }

    #[tool(
        name = "bro_slack_link_record",
        description = "Record a SlackProposalLink mapping a posted Slack message back to its BadgeyProposal. Called by the per-channel triage workflow's emit-proposal subworkflow after chat.postMessage so inbound reactions/replies can resolve back to (BadgeyId, proposal_id) and the apply/refine hooks fire."
    )]
    fn bro_slack_link_record(
        &self,
        Parameters(p): Parameters<SlackProposalLinkRecordParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty()
            || p.channel_id.trim().is_empty()
            || p.msg_ts.trim().is_empty()
            || p.proposal_id.trim().is_empty()
            || p.project_dir.trim().is_empty()
        {
            return Self::err_text(
                "team_id, channel_id, msg_ts, proposal_id, and project_dir are all required",
            );
        }
        let link = slack_proposal_links::SlackProposalLink {
            team_id: p.team_id,
            channel_id: p.channel_id,
            msg_ts: p.msg_ts.clone(),
            proposal_id: p.proposal_id.clone(),
            instance_id: p.instance_id,
            authoring_session_id: p.authoring_session_id,
            version: 1,
            project_dir: p.project_dir,
            posted_at: util::now_iso(),
        };
        match self.state.slack_proposal_links.record(link) {
            Ok(()) => Self::ok_json(&json!({
                "recorded": true,
                "msg_ts": p.msg_ts,
                "proposal_id": p.proposal_id,
            })),
            Err(e) => Self::err_text(&format!("recording slack proposal link failed: {e}")),
        }
    }

    #[tool(
        name = "bro_slack_link_lookup",
        description = "Resolve a Slack message ts back to its SlackProposalLink (proposal_id, instance_id, project_dir, version, posted_at). Used by the apply/refine workflows that fire on `:white_check_mark:` reactions and in-thread replies — they need the (BadgeyId, proposal_id) pair from the link to call badgey_apply_proposal or bro_resume. Returns {found: false} for messages that aren't a posted proposal (e.g. random check on an unrelated message) so workflows can no-op cleanly."
    )]
    fn bro_slack_link_lookup(
        &self,
        Parameters(p): Parameters<SlackProposalLinkLookupParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty()
            || p.channel_id.trim().is_empty()
            || p.msg_ts.trim().is_empty()
        {
            return Self::err_text("team_id, channel_id, and msg_ts are all required");
        }
        match self
            .state
            .slack_proposal_links
            .lookup_by_msg(&p.team_id, &p.channel_id, &p.msg_ts)
        {
            Some(link) => Self::ok_json(&json!({"found": true, "link": link})),
            None => Self::ok_json(&json!({"found": false})),
        }
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

    fn spawn_workflow_task(
        &self,
        compiled: workflow::CompiledWorkflow,
        project_dir: Option<String>,
        max_steps: Option<usize>,
        initial_vars: serde_json::Map<String, Value>,
    ) -> (Arc<orch::Task>, String) {
        let task_id = uuid::Uuid::new_v4().to_string();
        let arc_id = format!("arc-{}", uuid::Uuid::new_v4().simple());
        let workflow_name = compiled.spec.name.clone();
        let task = orch::spawn_in_process_task(
            task_id.clone(),
            Provider::Workflow,
            arc_id.clone(),
            project_dir.clone(),
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(format!("workflow::{workflow_name}")),
            None,
        );
        orch::push_in_process_event(
            &task,
            serde_json::json!({
                "kind": "workflow_task_started",
                "data": {
                    "workflow": workflow_name,
                    "arc_id": arc_id,
                },
                "timestamp": crate::util::now_iso(),
            }),
        );
        let state = self.state.clone();
        let task_for_run = task.clone();
        let arc_for_run = arc_id.clone();
        tokio::spawn(async move {
            let server = BlackboxServer::new(state.clone());
            let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
            let task_for_events = task_for_run.clone();
            let event_forwarder = tokio::spawn(async move {
                let mut count = 0usize;
                while let Some(event) = event_rx.recv().await {
                    count += 1;
                    orch::push_in_process_event(&task_for_events, event);
                }
                count
            });
            let result = workflow::run_workflow_streaming_with_vars_and_arc_id(
                &server,
                &compiled,
                project_dir,
                max_steps,
                initial_vars,
                event_tx,
                arc_for_run.clone(),
            )
            .await;
            let streamed_count = event_forwarder.await.unwrap_or(0);
            let status = if result.status == "completed" {
                orch::TaskStatus::Completed
            } else if result.status == "cancelled" {
                orch::TaskStatus::Cancelled
            } else {
                orch::TaskStatus::Failed
            };
            if streamed_count == 0 {
                for event in &result.events {
                    orch::push_in_process_event(&task_for_run, event.clone());
                }
            }
            let result_text = serde_json::to_string(&result).unwrap_or_else(|err| {
                serde_json::json!({
                    "status": "serialization_error",
                    "error": err.to_string()
                })
                .to_string()
            });
            let stderr = (status == orch::TaskStatus::Failed).then(|| result.status.clone());
            orch::finish_in_process_task(
                &task_for_run,
                status,
                Some(result_text),
                stderr,
                &state.task_store,
                &state.store_dir,
                &state.tail_tx,
            );
        });
        (task, arc_id)
    }

    #[tool(
        name = "bro_orchestrate_run",
        description = "Dispatch a workflow as a pollable task. Takes a full spec (actors, nodes with per-node `next` transitions: goto / branch / fork / terminal) and returns {taskId, arcId, status} immediately by default; poll with bro_status(task_id=...), await with bro_wait(task_id=...), or inspect arc state with bro_arc_status(arc_id=...). Pass await_completion=true only when the caller intentionally wants blocking behavior. Pass dry_run=true to validate + summarize without dispatching any bros."
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
        let (task, arc_id) =
            self.spawn_workflow_task(compiled, p.project_dir, p.max_steps, initial_vars);
        if p.await_completion.unwrap_or(false) {
            let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
            let mut out = if completed {
                orch::task_result_json(&task)
            } else {
                orch::timeout_snapshot_json(&task)
            };
            out["arcId"] = Value::String(arc_id);
            return Self::ok_json(&out);
        }
        let inner = task.inner.lock();
        Self::ok_json(&serde_json::json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "arcId": arc_id,
            "status": "running",
            "poll": {
                "status_tool": "bro_status",
                "wait_tool": "bro_wait",
                "arc_status_tool": "bro_arc_status"
            }
        }))
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
        let spec: webhooks::WebhookSpec = match Self::parse_spec(p.spec, "webhook") {
            Ok(s) => s,
            Err(r) => return r,
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
        let spec: pollers::PollerSpec = match Self::parse_spec(p.spec, "poller") {
            Ok(s) => s,
            Err(r) => return r,
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
        let spec: crons::CronSpec = match Self::parse_spec(p.spec, "cron") {
            Ok(s) => s,
            Err(r) => return r,
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

    #[tool(
        name = "bro_workflow_install",
        description = "Install a workflow spec by id so it can be referenced by name from webhook routing verdicts (`{route: start_arc, workflow: <id>}`) and other lookup paths. Compile-validated before install; capability tags enforced."
    )]
    async fn bro_workflow_install(
        &self,
        Parameters(p): Parameters<WorkflowInstallParams>,
    ) -> CallToolResult {
        let spec: workflow::Workflow = match Self::parse_spec(p.spec, "workflow") {
            Ok(s) => s,
            Err(r) => return r,
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
                let resume_lease = try_acquire_resume_lease(
                    &self.state.task_store,
                    self.state.resume_leases.as_ref(),
                    provider,
                    session_id,
                )?;
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
                    None,
                    None,
                );
                cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
                release_resume_lease_when_done(task.clone(), resume_lease);
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
                    None,
                    None,
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
    let th_path = util::blackbox_threads_path(&home);
    let rm_path = util::blackbox_roadmap_path(&home);
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

    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());
    if let Err(err) = idx.index_threads_store(&th) {
        tracing::warn!(error = %err, "thread index sync failed; will retry on next reindex cycle");
    }

    let roadmap_store = Roadmap::open(&rm_path)?;
    tracing::info!("Roadmap store: {}", rm_path.display());

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
    let agent_adapter_registry = Arc::new(RwLock::new(
        orchestration::agents::adapter::AgentAdapterRegistry::new(),
    ));
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
    let badgey_proposals = Arc::new(orchestration::badgey::ProposalStore::new(
        store_dir.clone(),
    )?);
    let badgey_journal = Arc::new(orchestration::badgey::ActionJournal::new(
        store_dir.clone(),
    )?);

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

    let edge_index = if std::env::var("BLACKBOX_EDGE_INDEX_BOOT_REBUILD")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
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

    let shutdown_grace = std::env::var("BLACKBOX_SHUTDOWN_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| std::time::Duration::from_secs(15));
    let signal_ct = ct.clone();
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
