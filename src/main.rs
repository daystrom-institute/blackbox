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
mod dispatch_mcp;
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
mod manifest;
mod mcp_client;
mod mcp_tools;
mod migration;
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
mod snapshot;
mod storage_health;
mod system_events;
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
                + tools::atoms::router()
                + tools::orchestrate::router()
                + tools::councils::router()
                + tools::roster::router()
                + tools::config::router()
                + tools::dispatch::router()
                + tools::mcp_surface::router()
                + tools::storage_health::router()
                + tools::storage_gc::router()
                + tools::storage_migration::router()
                + tools::workspace::router()
                + tools::system_events::router(),
            surface: std::sync::OnceLock::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bbox tools (search, knowledge, threads)
// ---------------------------------------------------------------------------

use artifacts::{
    ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
pub(crate) use dispatch_mcp::dispatch_mcp_url;
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

#[tool_router(router = bro_tools)]
impl BlackboxServer {}

// ---------------------------------------------------------------------------
// Helper methods on BlackboxServer
// ---------------------------------------------------------------------------

impl BlackboxServer {}

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

    let bbox_url = dispatch_mcp_url(&cfg.daemon.bind, cfg.daemon.port);
    let bbox_mcp_name = cfg.daemon.mcp_name.clone();
    // Export for provider arg-builders so they can inject `--mcp-config`
    // etc. at dispatch time. Provider-owned MCP config files are never
    // rewritten on daemon startup; persistent registration is user-owned
    // or happens only through explicit `bro_mcp` calls.
    unsafe {
        std::env::set_var("BLACKBOX_MCP_URL", &bbox_url);
    }
    unsafe {
        std::env::set_var("BLACKBOX_MCP_NAME", &bbox_mcp_name);
    }
    tracing::info!(
        "blackbox MCP dispatch injection configured (name={}, url={})",
        bbox_mcp_name,
        bbox_url
    );

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
            registered_project_ids: Some(
                projects_store
                    .list()
                    .into_iter()
                    .map(|project| project.project_id)
                    .collect(),
            ),
            include_tantivy_projection: false,
            include_observed: true,
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
        atom_invocation_store: Arc::new(RwLock::new(
            orchestration::atoms::invocation::InvocationStore::new(
                store_dir.join("atom-invocations.json"),
            ),
        )),
        vector_store: Arc::new(
            vectors::VectorStore::open_unloaded(vectors::default_vectors_dir())
                .expect("default vector store placeholder should open"),
        ),
        system_events: Arc::new(system_events::EventHub::new(
            system_events::EventStore::new(&store_dir),
            system_events::OutboxStore::new(store_dir.join("events").join("outbox"))
                .unwrap_or_else(|e| panic!("opening outbox store at {store_dir:?}: {e}")),
            store_dir.join("reactions"),
            store_dir.join("identities"),
        )),
    });
    shared
        .agent_adapter_registry
        .write()
        .register(Arc::new(BadgeyAgentAdapter {
            state: shared.clone(),
        }));
    restore_badgey_registry_from_notes(&shared);
    recover_badgey_non_terminal_state(&shared);
    embed_queue::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    embed_queue::install_contradiction_state(shared.clone());
    embed_queue::install(embed::queue::EmbedQueueHandle::start_default_without_store());

    std::thread::Builder::new()
        .name("blackbox-vectors-warmup".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let store = vectors::global();
            embed_queue::install(embed::queue::EmbedQueueHandle::start_default_with_store(
                store.clone(),
            ));
            tracing::info!(
                partitions = store.partition_count(),
                elapsed_ms = started.elapsed().as_millis(),
                "vector store warmed"
            );
        })
        .map_err(|e| anyhow::anyhow!("spawning vector store warmup thread: {e}"))?;

    // Watch the tantivy corpus and rebuild the EdgeIndex whenever new docs
    // land via the auto-reindex thread (60s poll interval is sufficient
    // since the reindex tick is 120s by default).
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));
    let storage_gc_interval = storage_gc_interval_from_env();
    tracing::info!(
        interval_secs = storage_gc_interval.as_secs(),
        "storage GC maintenance thread enabled"
    );
    spawn_storage_gc_thread(shared.clone(), storage_gc_interval);

    // Task-completed router: subscribe to tail events and forward each
    // TaskCompleted as a `task-completed` signal through the installed
    // routing packet (domain:auto-digest/task-completed-routing). When
    // the packet is not installed the dispatch is a fast no_match
    // dead-letter — no performance impact on normal operation.
    {
        let shared_for_router = shared.clone();
        let mut tail_rx = tail_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match tail_rx.recv().await {
                    Ok(orchestration::tail::TailEvent::TaskCompleted {
                        task_id,
                        source_session,
                        task_kind,
                        ..
                    }) => {
                        let entity = serde_json::json!({
                            "signal": "task-completed",
                            "event_type": "task-completed",
                            "kind": "task-completed",
                            "task_id": task_id,
                            "session_id": source_session,
                            "task_kind": task_kind,
                        });
                        if let Err(e) = dispatch_routed_event(
                            shared_for_router.clone(),
                            "task-completed",
                            "domain:auto-digest/task-completed-routing",
                            entity,
                            None,
                        )
                        .await
                        {
                            tracing::debug!("task-completed router: {e:#}");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("task-completed router: lagged {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // System events are also workflow signals: a Wait on
    // `bro.identity.provisioned` should resume when the durable event is
    // emitted by a reaction. Only dispatch when a matching wait already exists
    // so ordinary event traffic does not fill the signal log with idle entries.
    {
        let shared_for_system_event_signals = shared.clone();
        let mut system_event_rx = shared.system_events.subscribe();
        tokio::spawn(async move {
            loop {
                match system_event_rx.recv().await {
                    Ok(event) => {
                        let signal = event.kind.to_wire().to_string();
                        let has_wait = shared_for_system_event_signals
                            .wait_store
                            .snapshot()
                            .into_iter()
                            .any(|w| w.signal == signal);
                        if !has_wait {
                            continue;
                        }
                        let correlation = event.correlation.clone();
                        let payload = serde_json::to_value(&event).unwrap_or_else(|e| {
                            json!({
                                "event_id": event.id,
                                "kind": signal,
                                "serialization_error": e.to_string(),
                            })
                        });
                        let resolved = crate::server::routes::signal_arc_dispatch(
                            &shared_for_system_event_signals,
                            &signal,
                            correlation,
                            payload,
                        )
                        .await;
                        tracing::debug!(
                            signal,
                            result = %resolved,
                            "system event signal bridge dispatched"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("system event signal bridge lagged {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

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

    // Reactions — restore installed reaction specs from disk so they
    // survive daemon restart. Bad specs are logged and skipped; warnings
    // are available via reaction_list.
    let reaction_warnings = shared.system_events.restore_reactions_from_disk().await;
    if !reaction_warnings.is_empty() {
        tracing::warn!("reaction restore: {} warning(s)", reaction_warnings.len());
    }

    // Outbox crash recovery — requeue stale claimed records with
    // idempotency keys, dead-letter non-idempotent stale claims.
    let recovery = shared.system_events.outbox_store().recover_stale_claims();
    if recovery.requeued > 0 || recovery.dead_lettered > 0 {
        tracing::info!(
            "outbox recovery: {} requeued, {} dead-lettered",
            recovery.requeued,
            recovery.dead_lettered
        );
    }

    // Startup compaction — drop events older than 7 days / cap at 10k,
    // and drop succeeded outbox records older than 7 days. Failures
    // log and continue; the worker still starts.
    {
        let now = crate::util::now_iso();
        match shared.system_events.compact_with_now(&now) {
            Ok(report)
                if report.event_journal.dropped_by_age > 0
                    || report.event_journal.dropped_by_count > 0
                    || report.outbox.dropped_succeeded > 0 =>
            {
                tracing::info!(
                    "event journal compaction: kept {} (dropped {} by age, {} by count)",
                    report.event_journal.after,
                    report.event_journal.dropped_by_age,
                    report.event_journal.dropped_by_count
                );
                tracing::info!(
                    "outbox compaction: kept {} (dropped {} succeeded)",
                    report.outbox.after,
                    report.outbox.dropped_succeeded
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("system event compaction failed: {e:#}"),
        }
    }

    // Outbox worker — background task that claims due records, evaluates
    // gates, executes supported actions, and marks succeeded/retry/dead-lettered.
    {
        let worker_state = shared.clone();
        tokio::spawn(async move {
            crate::system_events::worker::run_worker(worker_state).await;
        });
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
            "/admin/artifact/remove",
            axum::routing::post(admin_artifact_remove),
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
