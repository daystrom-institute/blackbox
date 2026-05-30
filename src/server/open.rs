use super::startup::{configure_dispatch_mcp_env, discover_transcript_roots, resolve_codex_root};
use super::{SIGNAL_LOG_CAP, SharedState, WEBHOOK_LOG_CAP, is_loopback_bind};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::notes::Notes;
use crate::orchestration::TaskStore;
use crate::orchestration::tail::TailEvent;
use crate::packets::Packets;
use crate::pins::Pins;
use crate::projects::ProjectRegistry;
use crate::roadmap::Roadmap;
use crate::threads::Threads;
use crate::{
    artifacts, config, council, crons, edge_index, index, lsp, orchestration, path_cache, pollers,
    slack_channel_bindings, slack_proposal_links, slack_thread_store, system_events, system_memory,
    tool_docs, vectors, webhooks, whiteboards,
};

pub(super) struct OpenedServer {
    pub(super) cfg: config::Config,
    pub(super) shared: Arc<SharedState>,
    pub(super) store_dir: PathBuf,
    pub(super) bind_host: String,
    pub(super) bind_is_loopback: bool,
}

pub(super) fn open_shared_state(home: &Path) -> anyhow::Result<OpenedServer> {
    let cfg = config::load()?;
    let cfg_arc = Arc::new(RwLock::new(cfg.clone()));

    let roots = discover_transcript_roots(&cfg, home);
    let codex_root = resolve_codex_root(&cfg, home);
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
    sync_tool_docs(&mut kb);
    // Load each registered repo's committed .bbox/knowledge/ into the query
    // surface (project durable knowledge is repo-owned; central holds globals).
    {
        let kb_roots: Vec<std::path::PathBuf> = projects_store
            .list()
            .into_iter()
            .map(|r| std::path::PathBuf::from(r.canonical_path))
            .collect();
        if let Err(e) = kb.set_project_roots(kb_roots) {
            tracing::warn!("kb project-root load at startup: {e:#}");
        }
    }
    load_system_memory_catalog(&cfg)?;
    configure_dispatch_mcp_env(&cfg);
    sweep_stale_gemini_policies();

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
    backfill_artifact_hashes(&artifacts_store);

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
    spawn_reindex_thread(&cfg, &idx);

    let bind_host = cfg.daemon.bind.clone();
    let bind_is_loopback = is_loopback_bind(&bind_host);
    let edge_index = build_startup_edge_index(
        &cfg,
        &idx,
        &kb,
        &th,
        &notes_store,
        &task_store,
        &roadmap_store,
        &store_dir,
        &projects_store,
    );

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
        tail_tx,
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
        agent_adapter_registry,
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
        config: cfg_arc,
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

    Ok(OpenedServer {
        cfg,
        shared,
        store_dir,
        bind_host,
        bind_is_loopback,
    })
}

fn sync_tool_docs(kb: &mut Knowledge) {
    match tool_docs::sync_into_knowledge(kb) {
        Ok(r) if r.wrote => tracing::info!("Tool reference synced ({} bytes)", r.bytes),
        Ok(_) => tracing::debug!("Tool reference already up to date"),
        Err(e) => tracing::warn!("Tool reference sync failed: {e:#}"),
    }
}

fn load_system_memory_catalog(cfg: &config::Config) -> anyhow::Result<()> {
    let memory_ctx = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "mcp_name": &cfg.daemon.mcp_name,
    });
    system_memory::init(
        &cfg.paths.defaults_memories_dir,
        cfg.paths.user_memories_dir.as_deref(),
        &memory_ctx,
    )?;
    tracing::info!(
        "System memory catalog loaded from {}",
        cfg.paths.defaults_memories_dir.display()
    );
    Ok(())
}

fn sweep_stale_gemini_policies() {
    match orchestration::mcp::sweep_stale_gemini_policies(24) {
        Ok(n) if n > 0 => tracing::info!("swept {n} stale gemini policy file(s)"),
        Ok(_) => {}
        Err(e) => tracing::debug!("gemini policy sweep: {e:#}"),
    }
}

fn backfill_artifact_hashes(artifacts_store: &artifacts::ArtifactCatalog) {
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
}

fn spawn_reindex_thread(cfg: &config::Config, idx: &TranscriptIndex) {
    let reindex_interval = cfg.index.reindex_interval_secs;
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );
}

fn build_startup_edge_index(
    cfg: &config::Config,
    idx: &TranscriptIndex,
    kb: &Knowledge,
    th: &Threads,
    notes_store: &Notes,
    task_store: &TaskStore,
    roadmap_store: &Roadmap,
    store_dir: &Path,
    projects_store: &ProjectRegistry,
) -> edge_index::EdgeIndex {
    if cfg.index.edge_index_boot_rebuild {
        edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
            index: idx,
            knowledge: kb,
            threads: th,
            notes: notes_store,
            task_store,
            roadmap: roadmap_store,
            edges_dir: edge_index::edges_dir_from_bro_store(store_dir),
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
    }
}
