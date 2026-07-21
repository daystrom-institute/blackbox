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
use crate::store_persister::StorePersister;
use crate::threads::Threads;
use crate::{
    artifacts, config, crons, edge_index, index, orchestration, path_cache, pollers,
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
    // Push the config-resolved git-notes namespace into the corpus-core
    // foundation crate (dependency inversion: corpus-core must not reach up into
    // blackbox::config). Absent this, git::notes_namespace falls back to the
    // BBOX_GIT_NOTES_NAMESPACE env var, then "bbox".
    crate::git::set_notes_namespace(cfg.provenance.git_notes_namespace.clone());
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
    // Index harness session event logs (sidecar JSONL next to the resume
    // snapshots) so harness sessions are searchable like any other provider
    // transcript. Each child receives this same root as BRO_HOME.
    idx.set_harness_sessions_dir(cfg.paths.bro_home.join("harness-sessions"));
    // Interactive Gemini chats (claude/codex roots already travel inside
    // ReindexConfig; gemini's tmp root is resolved here, same explicit-only
    // contract — gap-5af6d773).
    if let Some(gemini_tmp) = std::env::var("GEMINI_TMP_ROOT")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".gemini").join("tmp")))
    {
        idx.set_gemini_tmp_root(gemini_tmp);
    }
    // The daemon's single tantivy writer: every index mutation and reindex
    // pass flows through this actor (concurrency-model §4.3). Spawned AFTER
    // all ReindexConfig mutation — the actor clones the config at spawn.
    let index_writer = crate::index::IndexWriterActor::spawn_for(&idx);
    let (mut projects_store, mut projects_needs_persist) =
        ProjectRegistry::open_with_backfill_status(&projects_path)?;
    tracing::info!("Project registry: {}", projects_path.display());
    // Materialize repo-declared aliases (`[project] aliases` in each repo's
    // committed `.bbox/config.toml`) into the registry. Boot cannot fail
    // closed the way registration does, so a conflicting or invalid claim is
    // skipped with a warning — the alias simply never materializes, and
    // resolution fails closed by absence. Records are sorted by
    // canonical_path, so first-claim-wins is deterministic across boots.
    for record in projects_store.list() {
        let declared: std::collections::BTreeSet<String> =
            match crate::config::load_project(std::path::Path::new(&record.canonical_path)) {
                Ok(cfg) => cfg.project.aliases.into_iter().collect(),
                Err(e) => {
                    tracing::warn!("project config for {}: {e:#}", record.project_id);
                    continue;
                }
            };
        match projects_store.sync_declared_aliases(&record.project_id, &declared) {
            Ok(dirty) => projects_needs_persist |= dirty,
            Err(e) => tracing::warn!("alias sync for {}: {e:#}", record.project_id),
        }
    }

    let path_fallback_cut = bbox_knowledge::inventory::path_fallback_was_cut(&cfg.paths.bro_home)?;
    let mut kb = Knowledge::open(&kb_path)?;
    kb.set_path_fallback_cut(path_fallback_cut);
    tracing::info!("Knowledge store: {}", kb_path.display());
    // Load each registered repo's committed .bbox/knowledge/ into the query
    // surface BEFORE any save. This must precede sync_tool_docs (which writes
    // the tool-reference entry and triggers save()): a save with the repo's
    // entries not yet loaded would treat the in-memory set as authoritative and
    // purge the committed .bbox/knowledge files for a repo-owned project.
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
    let tool_docs_synced = sync_tool_docs(&mut kb);

    // Gap store mirrors the kb repo-owned model. Load every registered repo's
    // committed `.bbox/gaps/` into the query surface BEFORE any producer
    // (bbox_packet_gap, gap-spool import) can save — a save with the repo's
    // gaps not yet loaded would treat the in-memory set as authoritative and
    // purge committed `.bbox/gaps/` files for a repo-owned project.
    let gaps_path = cfg.paths.gaps_path.clone();
    let mut gaps_store = crate::gaps::GapStore::open(&gaps_path)?;
    gaps_store.set_path_fallback_cut(path_fallback_cut);
    tracing::info!("Gap store: {}", gaps_path.display());
    {
        let gap_roots: Vec<std::path::PathBuf> = projects_store
            .list()
            .into_iter()
            .map(|r| std::path::PathBuf::from(r.canonical_path))
            .collect();
        if let Err(e) = gaps_store.set_project_roots(gap_roots) {
            tracing::warn!("gaps project-root load at startup: {e:#}");
        }
    }
    load_system_memory_catalog(&cfg)?;
    configure_dispatch_mcp_env(&cfg);

    let kb_store = Arc::new(RwLock::new(kb));
    let kb_persister = StorePersister::spawn("knowledge", kb_store.clone(), kb_path.clone());
    if tool_docs_synced {
        // Startup sync is synchronous setup; central knowledge persistence is write-behind here.
        kb_persister.request();
    }

    let th = Threads::open(&th_path)?;
    // Wire the store-side embed sinks to the embedding queue (dependency
    // inversion: the stores live below the embed pipeline in the crate DAG).
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
    tracing::info!("Thread store: {}", th_path.display());
    // Queued on the writer actor: boot no longer races the reindex thread
    // (or a winding-down previous daemon) for tantivy's single-writer lock.
    index_writer.enqueue(index::IndexWriteOp::UpsertThreadsStore(th.all().to_vec()));
    let threads_store = Arc::new(RwLock::new(th));
    let threads_persister =
        StorePersister::spawn("threads", threads_store.clone(), th_path.clone());

    let roadmap_store = Arc::new(RwLock::new(Roadmap::open(&rm_path)?));
    let roadmap_persister =
        StorePersister::spawn("roadmap", roadmap_store.clone(), rm_path.clone());
    tracing::info!("Roadmap store: {}", rm_path.display());

    let notes_path = cfg.paths.notes_path.clone();
    let notes_store = Arc::new(RwLock::new(Notes::open(&notes_path)?));
    let notes_persister = StorePersister::spawn("notes", notes_store.clone(), notes_path.clone());
    tracing::info!("Notes store: {}", notes_path.display());

    let pins_path = cfg.paths.pins_path.clone();
    let pins_store = Arc::new(RwLock::new(Pins::open(&pins_path)?));
    let pins_persister = StorePersister::spawn("pins", pins_store.clone(), pins_path.clone());
    tracing::info!("Pins store: {}", pins_path.display());

    let projects_store = Arc::new(RwLock::new(projects_store));
    let projects_persister =
        StorePersister::spawn("projects", projects_store.clone(), projects_path.clone());
    if projects_needs_persist {
        // Startup language backfill is synchronous setup; projects persistence is write-behind here.
        projects_persister.request();
    }

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
    // Keep daemon-side transcript discovery on the configured harness-session
    // root. Each standalone harness child also receives this path explicitly
    // in its own environment at spawn.
    if std::env::var("BRO_HOME").is_err() {
        unsafe {
            std::env::set_var("BRO_HOME", store_dir.to_string_lossy().as_ref());
        }
    }
    let task_ttl = cfg.daemon.task_ttl_ms;
    let task_store = TaskStore::load(&store_dir, task_ttl);
    let consultant_proposals = Arc::new(orchestration::consultant::ProposalStore::new(
        orchestration::badgey::descriptor().proposals_root(&store_dir),
    )?);
    let consultant_journal = Arc::new(orchestration::consultant::ActionJournal::new(
        orchestration::badgey::descriptor().action_journal_root(&store_dir),
    )?);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);
    let (roster_tx, _) = broadcast::channel::<bro_protocol::RosterDelta>(1024);
    // Shared reindex trigger. Initialized `true` so the first background pass
    // runs once after startup and indexes repo-owned `.bbox/knowledge` that may
    // have changed while the daemon was down (no watcher event fires for those,
    // and `needs_reindex` does not track them). The `.bbox/knowledge` watcher
    // sets it on live changes; the same `Arc` is stored in `SharedState`.
    let reindex_dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
    spawn_reindex_thread(&cfg, &idx, index_writer.clone(), reindex_dirty.clone());

    let bind_host = cfg.daemon.bind.clone();
    let bind_is_loopback = is_loopback_bind(&bind_host);
    let edge_index = build_startup_edge_index(
        &cfg,
        &idx,
        &kb_store.read(),
        &threads_store.read(),
        &notes_store.read(),
        &task_store,
        &roadmap_store.read(),
        &store_dir,
        &projects_store.read(),
    );

    let (edge_rebuild_nudge_tx, edge_rebuild_nudge_rx) = std::sync::mpsc::sync_channel(1);
    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        index_writer,
        kb: kb_store,
        kb_persister,
        gaps: RwLock::new(gaps_store),
        roadmap: roadmap_store,
        roadmap_persister,
        threads: threads_store,
        threads_persister,
        notes: notes_store,
        notes_persister,
        pins: pins_store,
        pins_persister,
        projects: projects_store,
        projects_persister,
        checkout_registry: RwLock::new(bbox_indexing::checkout_registry::CheckoutRegistry::open(
            &store_dir.join("checkout-registry.json"),
        )?),
        publisher_refs: RwLock::new(bbox_indexing::publisher::PublisherRefStore::open(
            store_dir.join("publisher-refs.json"),
        )?),
        knowledge_overlays: RwLock::new(bbox_knowledge::overlay::KnowledgeOverlayStore::default()),
        gap_overlays: RwLock::new(bbox_gaps::overlay::GapOverlayStore::default()),
        knowledge_overlay_refresh: parking_lot::Mutex::new(()),
        gap_overlay_refresh: parking_lot::Mutex::new(()),
        path_fallback_cut: std::sync::atomic::AtomicBool::new(path_fallback_cut),
        knowledge_published_cache: RwLock::new(Default::default()),
        packets: RwLock::new(packets_store),
        surface_decisions: crate::server::surface::SurfaceDecisionCache::default(),
        artifacts: RwLock::new(artifacts_store),
        bbox_watcher: std::sync::Mutex::new(None),
        reindex_dirty,
        edge_index: RwLock::new(edge_index),
        edge_rebuild_nudge_tx,
        edge_rebuild_nudge_rx: std::sync::Mutex::new(Some(edge_rebuild_nudge_rx)),
        path_cache: RwLock::new(path_cache::PathCache::default()),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx,
        roster_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        roster_tx,
        roster_view: Arc::new(orchestration::RosterView::new()),
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
        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        agent_adapter_registry,
        consultant_registry: Arc::new(orchestration::consultant::ConsultantRegistry::new()),
        consultant_proposals,
        consultant_journal,
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

    // Start the single-writer task-store persist actor (control-plane starvation
    // fix). Hot paths now signal `request_persist` instead of doing a blocking
    // `task_store.read().persist(dir)` on a tokio worker. Production only — tests
    // build SharedState via `for_test` and keep the synchronous fallback.
    orchestration::init_task_persister(shared.task_store.clone(), shared.store_dir.clone());

    // Cold-start roster rebuild: tasks restored from `tasks.json` need to
    // appear in `/control/roster` immediately, before any child-process delta
    // arrives. Walk the loaded store once and seed the view. Cheap O(N) over
    // the task store and runs before routes bind, so the first poll sees
    // the full set.
    shared
        .roster_view
        .rebuild_from_store(&shared.task_store.read());

    Ok(OpenedServer {
        cfg,
        shared,
        store_dir,
        bind_host,
        bind_is_loopback,
    })
}

fn sync_tool_docs(kb: &mut Knowledge) -> bool {
    match tool_docs::sync_into_knowledge(kb) {
        Ok(r) if r.wrote => {
            tracing::info!("Tool reference synced ({} bytes)", r.bytes);
            true
        }
        Ok(_) => {
            tracing::debug!("Tool reference already up to date");
            false
        }
        Err(e) => {
            tracing::warn!("Tool reference sync failed: {e:#}");
            false
        }
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

fn spawn_reindex_thread(
    cfg: &config::Config,
    idx: &TranscriptIndex,
    index_writer: index::IndexWriterActor,
    reindex_dirty: Arc<std::sync::atomic::AtomicBool>,
) {
    let reindex_interval = cfg.index.reindex_interval_secs;
    index::spawn_reindex_thread(
        index_writer,
        idx.reindex_config(),
        std::time::Duration::from_secs(reindex_interval),
        reindex_dirty,
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
            session_brofile_rows: task_store.session_brofile_rows(),
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
