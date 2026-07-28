use super::startup::{configure_dispatch_mcp_env, discover_transcript_roots, resolve_codex_root};
use super::{SIGNAL_LOG_CAP, SharedState, WEBHOOK_LOG_CAP, is_loopback_bind};
use anyhow::Context;
use parking_lot::RwLock;
use std::collections::{BTreeSet, HashMap};
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

fn sync_project_aliases_at_startup(
    projects: &Arc<RwLock<ProjectRegistry>>,
    load_aliases: impl Fn(
        &bbox_corpus_core::project_record::ProjectRecord,
    ) -> anyhow::Result<std::collections::BTreeSet<String>>,
) -> bool {
    let mut dirty = false;
    let records = projects.read().list();
    for record in records {
        let declared = match load_aliases(&record) {
            Ok(declared) => declared,
            Err(error) => {
                // The committed config is authoritative only when it was read
                // successfully. A transient read or parse failure must not be
                // interpreted as an authoritative empty alias declaration.
                tracing::warn!("project config for {}: {error:#}", record.project_id);
                continue;
            }
        };
        match projects
            .write()
            .sync_declared_aliases(&record.project_id, &declared)
        {
            Ok(changed) => dirty |= changed,
            Err(error) => tracing::warn!("alias sync for {}: {error:#}", record.project_id),
        }
    }
    dirty
}

fn open_checkout_registry(store_dir: &Path) -> bbox_indexing::checkout_registry::CheckoutRegistry {
    let path = store_dir.join("checkout-registry.json");
    let (registry, degraded) =
        bbox_indexing::checkout_registry::CheckoutRegistry::open_recoverable(&path);
    if let Some(error) = degraded {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "checkout registry unreadable; starting from an empty discovery index"
        );
    }
    registry
}

fn backfill_project_languages(
    projects: &Arc<RwLock<ProjectRegistry>>,
    checkout_access: &bbox_indexing::checkout_access::CheckoutAccessBroker,
) -> bool {
    use bbox_indexing::checkout_access::{
        CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
        CheckoutAttachmentSelector,
    };

    let records = projects.read().list();
    let mut changed = false;
    for project in records {
        if !project.languages.is_empty() {
            continue;
        }
        let scope_lease = match checkout_access.acquire(CheckoutAccessRequest {
            project_id: project.project_id.clone(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        }) {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(
                    project_id = %project.project_id,
                    error_code = %error.code.as_str(),
                    "language backfill skipped because scope discovery is unavailable"
                );
                continue;
            }
        };
        let expected_scope = scope_lease.published_scope().cloned();
        drop(scope_lease);
        let lease = match checkout_access.acquire(CheckoutAccessRequest {
            project_id: project.project_id.clone(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope,
            kind: CheckoutAccessKind::LocalProjectWalk,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        }) {
            Ok(lease) => lease,
            Err(error) => {
                tracing::warn!(
                    project_id = %project.project_id,
                    error_code = %error.code.as_str(),
                    "language backfill skipped because LocalProjectWalk is unavailable"
                );
                continue;
            }
        };
        let languages = bbox_indexing::projects::detect_languages(lease.project_root());
        if let Err(error) = checkout_access.revalidate(&lease) {
            tracing::warn!(
                project_id = %project.project_id,
                error_code = %error.code.as_str(),
                "language backfill discarded because checkout authority changed"
            );
            continue;
        }
        drop(lease);
        changed |= projects
            .write()
            .backfill_languages(&project.project_id, languages);
    }
    changed
}

pub(super) fn open_shared_state(home: &Path) -> anyhow::Result<OpenedServer> {
    let cfg = config::load()?;
    // Push the config-resolved git-notes namespace into the corpus-core
    // foundation crate (dependency inversion: corpus-core must not reach up into
    // blackbox::config). Absent this, git::notes_namespace falls back to the
    // BBOX_GIT_NOTES_NAMESPACE env var, then "bbox".
    crate::git::set_notes_namespace(cfg.provenance.git_notes_namespace.clone())?;
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
    let store_dir = cfg.paths.bro_home.clone();
    // Store-version mode selection (phase-2 §4.1): one strict probe decides
    // the runtime authority for the process lifetime, before any
    // project-scoped subsystem starts. The probe fails closed on corrupt or
    // half-pair state; nothing here repairs, migrates, or creates v2 state.
    let store_probe =
        bbox_indexing::project_catalog_store::probe_project_store_mode(&projects_path)
            .map_err(|error| anyhow::anyhow!("project store probe: {error}"))?;
    let checkout_registry = Arc::new(RwLock::new(open_checkout_registry(&store_dir)));
    let checkout_access_observations =
        bbox_indexing::checkout_access::CheckoutAccessObservations::open(
            store_dir.join("checkout-access-observations.json"),
        )?;
    // Bridge-only handles stay `Option` so catalog mode never constructs a
    // version-1 registry or its persister.
    let mut projects_store: Option<Arc<RwLock<ProjectRegistry>>> = None;
    let mut catalog_store: Option<Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>> =
        None;
    let mut projects_needs_persist = false;
    let (access_authority, records_provider): (
        Arc<dyn bbox_indexing::checkout_access::CheckoutAccessAuthority>,
        Arc<dyn bbox_corpus_core::project_record::ProjectRecordsProvider>,
    ) = match store_probe {
        bbox_indexing::project_catalog_store::ProjectStoreProbe::AbsentBridge
        | bbox_indexing::project_catalog_store::ProjectStoreProbe::LegacyV1 => {
            let (projects_registry, needs_persist) =
                ProjectRegistry::open_with_backfill_status(&projects_path)?;
            projects_needs_persist = needs_persist;
            let registry = Arc::new(RwLock::new(projects_registry));
            projects_store = Some(registry.clone());
            (
                Arc::new(
                    bbox_indexing::checkout_access_v1::V1CheckoutAccessAuthority::new(
                        registry.clone(),
                        checkout_registry.clone(),
                    ),
                ),
                Arc::new(bbox_indexing::projects::BridgeProjectRecordsProvider::new(
                    registry,
                )),
            )
        }
        bbox_indexing::project_catalog_store::ProjectStoreProbe::CatalogV2 => {
            // Strict pair open: validation, journal recovery, and the
            // origin/marker binding all happen here, before routes bind.
            let store = Arc::new(
                bbox_indexing::project_catalog_store::ProjectCatalogStore::open_existing(
                    &projects_path,
                )
                .map_err(|error| anyhow::anyhow!("catalog store open: {error}"))?,
            );
            tracing::info!("Project authority: durable catalog (v2)");
            catalog_store = Some(store.clone());
            (
                Arc::new(
                    bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(
                        store.clone(),
                    ),
                ),
                Arc::new(bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(store)),
            )
        }
    };
    let checkout_access = Arc::new(bbox_indexing::checkout_access::CheckoutAccessBroker::new(
        access_authority,
        checkout_access_observations.clone(),
    ));
    if let Some(registry) = &projects_store {
        projects_needs_persist |= backfill_project_languages(registry, &checkout_access);
    }

    // Rebuild-manifest recovery, classified BEFORE the index opens (P3-E, plan
    // section 9 item 2). Two reasons the ordering is not cosmetic: the
    // classifier locates itself relative to the destructive drop by reading the
    // index's schema marker, and opening the index rewrites that marker; and a
    // crash after the drop leaves no schema mismatch to detect, so without the
    // resume signal below the synchronous rebuild would never run and the
    // carried-over history would stay unmaterialized.
    let rebuild_resume =
        bbox_indexing::index::schema_rebuild::recover_rebuild_manifest_before_open(&index_path)
            .context("classifying repo-history rebuild recovery")?;
    // The injected pre-replacement guard. Catalog mode drives the P3-D
    // materializer; bridge mode writes the commit spill. There is deliberately
    // no third arm: an absent guard refuses the reset outright, so a future
    // authority mode must supply one rather than inheriting a silent drop.
    let schema_replacement_guard: bbox_corpus_index::index::schema_replacement::SchemaReplacementGuard =
        match &catalog_store {
            Some(store) => bbox_indexing::index::schema_rebuild::catalog_schema_replacement_guard(
                store.clone(),
                bbox_corpus_index::index::history_generations::HistoryScanLimitsV1::default(),
            ),
            None => bbox_indexing::index::schema_rebuild::bridge_schema_replacement_guard(
                records_provider.clone(),
            ),
        };
    let mut idx = TranscriptIndex::open_or_create_with_code_source_store_path(
        &index_path,
        roots,
        codex_root,
        projects_path.clone(),
        cfg.paths.state_dir.join("code-sources"),
        kb_path.clone(),
        th_path.clone(),
        rm_path.clone(),
        records_provider.clone(),
        Some(schema_replacement_guard),
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
    let index_writer = crate::index::IndexWriterActor::spawn_for_with_checkout_access(
        &idx,
        records_provider.clone(),
        checkout_access.clone(),
    );
    tracing::info!("Project registry: {}", projects_path.display());
    // Materialize repo-declared aliases (`[project] aliases` in each repo's
    // committed `.bbox/config.toml`) into the registry. Boot cannot fail
    // closed the way registration does, so a conflicting or invalid claim is
    // skipped with a warning — the alias simply never materializes, and
    // resolution fails closed by absence. Records are sorted by
    // canonical_path, so first-claim-wins is deterministic across boots.
    // Catalog mode never runs the materializing alias sync: committed
    // aliases are nominations there (D-005), reported and accepted through
    // the explicit catalog action, never rewritten at startup.
    if let Some(registry) = &projects_store {
        projects_needs_persist |= sync_project_aliases_at_startup(registry, |project| {
            use bbox_indexing::checkout_access::{
                CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
                CheckoutAccessSourceLane, CheckoutAttachmentSelector,
            };
            let lease = checkout_access.acquire(CheckoutAccessRequest {
                project_id: project.project_id.clone(),
                attachment: CheckoutAttachmentSelector::Selected,
                expected_scope: None,
                kind: CheckoutAccessKind::PublisherConfigTreeRead,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
            })?;
            let aliases = crate::config::load_project_at_ref(lease.project_root(), "HEAD")?
                .project
                .aliases
                .into_iter()
                .collect();
            checkout_access.revalidate(&lease)?;
            Ok(aliases)
        });
    }

    let path_fallback_cut = bbox_knowledge::inventory::path_fallback_was_cut(&cfg.paths.bro_home)?;
    let repo_io = Arc::new(super::repo_io::RepoIoAuthority::new(
        checkout_access.clone(),
    ));
    // Mode-independent record view: the bridge derives from the registry,
    // catalog mode from the compatibility projection (attached rows only).
    let registered_projects = records_provider.records_snapshot().records;
    let mut kb = Knowledge::open(&kb_path)?;
    kb.set_path_fallback_cut(path_fallback_cut);
    tracing::info!("Knowledge store: {}", kb_path.display());
    // Load each registered repo's committed .bbox/knowledge/ into the query
    // surface BEFORE any save. This must precede sync_tool_docs (which writes
    // the tool-reference entry and triggers save()): a save with the repo's
    // entries not yet loaded would treat the in-memory set as authoritative and
    // purge the committed .bbox/knowledge files for a repo-owned project.
    if let Err(e) = kb.configure_repo_io(
        repo_io.clone(),
        repo_io.clone(),
        super::repo_io::RepoIoAuthority::knowledge_base_carriers(&registered_projects)?,
    ) {
        tracing::warn!("kb repository-carrier load at startup: {e:#}");
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
    if let Err(e) = gaps_store.configure_repo_io(
        repo_io.clone(),
        repo_io,
        super::repo_io::RepoIoAuthority::gap_base_carriers(&registered_projects)?,
    ) {
        tracing::warn!("gaps repository-carrier load at startup: {e:#}");
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

    let project_authority = match (&projects_store, &catalog_store) {
        (Some(registry), None) => {
            let persister =
                StorePersister::spawn("projects", registry.clone(), projects_path.clone());
            if projects_needs_persist {
                // Startup language backfill is synchronous setup; projects
                // persistence is write-behind here.
                persister.request();
            }
            super::state::ProjectAuthority::Bridge {
                registry: registry.clone(),
                persister,
            }
        }
        (None, Some(store)) => super::state::ProjectAuthority::Catalog {
            store: store.clone(),
        },
        _ => unreachable!("the store probe selects exactly one project authority"),
    };

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
    let code_sources = Arc::new(super::code_source::CodeSourceRuntime::open(
        &cfg,
        &records_provider.records_snapshot().records,
        catalog_store.clone(),
        checkout_access.clone(),
    )?);

    // R19F1: unconditional pre-bind transaction recovery. Runs before
    // selector refresh, read-view construction, and edge-index loading.
    // Pending transaction journals are ambiguous (the paired Tantivy
    // commit status is unprovable without a commit token): recovery
    // discards their staging directories and journals, leaving the live
    // snapshot untouched in its pre-transaction state.
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&store_dir);
    bbox_edge_sidecar::snapshot::recover_pending_transactions_prebind(&edges_dir)
        .context("pre-bind transaction recovery failed")?;

    // Pre-bind catalog-mode recovery (P4-F section 10.1 steps 5-8):
    // once-only classification, relationship chain validation,
    // retirement-journal detection, and startup reducer sweep. All run
    // BEFORE the schema rebuild, reindex, and CodeReadView construction
    // so that a broken relationship chain fails closed before the daemon
    // builds any read view from corrupt state. Bridge mode is a no-op.
    let pending_first_republish = super::code_source::pre_bind_catalog_recovery(
        &project_authority,
        &code_sources,
        &checkout_access,
        &store_dir,
    )?;
    idx.refresh_active_code_selectors()
        .context("refreshing active code selectors after pre-bind catalog recovery")?;

    // The grant table is built after the writer actor spawns, so the
    // planner's assignment view is installed here rather than passed to
    // `spawn` (same shape as the post-commit searcher hook).
    index_writer.set_producer_assignment_source(code_sources.clone());
    // The resume arm forces the same synchronous rebuild a fresh mismatch does.
    // After a crash that already dropped the index there is no marker left to
    // mismatch against, so `schema_was_reset` is false and the prepared
    // manifest is the only surviving evidence that a replacement is half done.
    let resume_interrupted_rebuild = matches!(
        rebuild_resume,
        bbox_indexing::index::schema_rebuild::SchemaRebuildResume::Resume { .. }
    );
    if idx.schema_was_reset() || resume_interrupted_rebuild {
        tracing::info!(
            schema = crate::index::INDEX_SCHEMA_VERSION,
            resume_interrupted_rebuild,
            "running synchronous full rebuild after index schema migration"
        );
        // Explicit cause, not `run_reindex_pass(true, true)`: this pass runs
        // against the index the replacement guard just authorized emptying, so
        // the preservation gates must not verify against it. Re-staging from the
        // proved sources is the authority here (`FullRebuildCause`).
        index_writer
            .run_reindex_pass_for_schema_migration()
            .context("synchronous schema-migration rebuild failed")?;
        idx.reader_reload_for_test();
        // Re-read the selector map from the edge-sidecar manifest. The paired
        // INDEXER_VERSION bump changes every collected selector's
        // materialization suffix, so the rebuild above may have migrated one or
        // more projects onto a new selector and flipped the manifest; the map
        // seeded at open still names the outgoing one, and the read view built
        // below would filter out exactly the documents the rebuild just staged.
        idx.refresh_active_code_selectors()
            .context("refreshing active code selectors after the schema-migration rebuild")?;
        idx.complete_schema_migration()
            .context("committing schema-migration version marker failed")?;
    }

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
        &records_provider.records_snapshot(),
        &pending_first_republish,
    )?;
    let code_read_view = super::CodeReadView {
        active_selectors: idx.active_code_selectors(),
        searcher: idx.searcher(),
        edge_index: Arc::new(edge_index),
        // Seeded from the same boot snapshot that seeded the edge set above,
        // so the startup view and the first runtime republish agree.
        catalog_epoch: records_provider.records_snapshot().authority_epoch,
        // Same boot snapshot, same rule: the startup view pins the durable
        // overlay selection so the first request after open joins commit-file
        // edges through exactly the overlays the manifest names.
        git_overlays: super::state::read_git_overlays_for_view(&project_authority, &store_dir),
    };
    // Phase 3 plan section 10 item 4: the derived repo-history reference
    // manifest is rebuilt and checksummed from durable inputs at startup,
    // BEFORE anything can sweep. A mismatch leaves history GC disabled and
    // surfaces in doctor; it never blocks the open, because a stale
    // acceleration index must not cost history reads.
    refresh_history_reference_manifest(&cfg, &project_authority, &store_dir, &code_read_view);
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
        project_authority,
        records_provider,
        checkout_registry,
        checkout_access_observations,
        resolver_compat: crate::server::resolver_compat::ResolverCompatObservations::open(
            store_dir.join("resolver-compat-observations.json"),
        ),
        checkout_access,
        // Publisher refs define authority and cannot be reconstructed from
        // checkout discovery without silently moving published truth. Keep
        // corrupt pins fail-closed even though the checkout census below is a
        // recoverable discovery index.
        publisher_refs: RwLock::new(bbox_indexing::publisher::PublisherRefStore::open(
            store_dir.join("publisher-refs.json"),
        )?),
        knowledge_overlays: RwLock::new(bbox_knowledge::overlay::KnowledgeOverlayStore::default()),
        gap_overlays: RwLock::new(bbox_gaps::overlay::GapOverlayStore::default()),
        knowledge_overlay_refresh: parking_lot::Mutex::new(()),
        gap_overlay_refresh: parking_lot::Mutex::new(()),
        path_fallback_cut: std::sync::atomic::AtomicBool::new(path_fallback_cut),
        knowledge_published_cache: RwLock::new(Default::default()),
        gap_published_cache: RwLock::new(Default::default()),
        publisher_authorization_cache: RwLock::new(Default::default()),
        packets: RwLock::new(packets_store),
        surface_decisions: crate::server::surface::SurfaceDecisionCache::default(),
        artifacts: RwLock::new(artifacts_store),
        bbox_watcher: std::sync::Mutex::new(None),
        reindex_dirty,
        code_read_view: RwLock::new(Arc::new(code_read_view)),
        code_sources,
        reconciler_shutdown: parking_lot::RwLock::new(Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        )),
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
    shared.install_code_read_view_commit_hook();

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
    records: &bbox_corpus_core::project_record::ProjectRecordsSnapshot,
    pending_first_republish: &BTreeSet<String>,
) -> anyhow::Result<edge_index::EdgeIndex> {
    if cfg.index.edge_index_boot_rebuild {
        edge_index::EdgeIndex::rebuild_admitting_fully_absent(
            &edge_index::EdgeStoreRefs {
                index: idx,
                knowledge: kb,
                threads: th,
                notes: notes_store,
                session_brofile_rows: task_store.session_brofile_rows(),
                roadmap: roadmap_store,
                edges_dir: edge_index::edges_dir_from_bro_store(store_dir),
                registered_project_ids: Some(records.registered_project_ids()),
                include_tantivy_projection: false,
                include_observed: true,
            },
            pending_first_republish,
        )
    } else {
        tracing::info!(
            "startup EdgeIndex rebuild deferred (set BLACKBOX_EDGE_INDEX_BOOT_REBUILD=1 to restore eager rebuild)"
        );
        Ok(edge_index::EdgeIndex::default())
    }
}

/// Rebuild and checksum the derived repo-history reference manifest from its
/// durable inputs (Phase 3 plan section 10 item 4; governing section 11).
///
/// Best effort, and deliberately so. This index only ACCELERATES GC root
/// computation; the authority is the catalog plus the overlay selectors this
/// function reads. A failure here leaves GC disabled (doctor reports it) and
/// must never fail the daemon open, because history reads do not depend on it.
///
/// The startup rebuild passes empty process-local root sets: no read view is
/// pinned and no build is in flight yet, which is exactly the state that
/// makes the durable set complete.
fn refresh_history_reference_manifest(
    cfg: &crate::config::Config,
    authority: &super::state::ProjectAuthority,
    store_dir: &Path,
    code_read_view: &super::CodeReadView,
) {
    use bbox_indexing::index::history_gc::{build_reference_manifest, evaluate_history_gc};

    let super::state::ProjectAuthority::Catalog { store } = authority else {
        return;
    };
    let Ok(pinned) = store.snapshot() else {
        tracing::warn!(
            "the project catalog is unreadable at open; the repo-history reference \
             manifest was not rebuilt and history GC stays disabled"
        );
        return;
    };
    let Ok(generation_store) =
        bbox_indexing::index::history_generations::HistoryGenerationStore::open_for_index(
            &cfg.paths.index_path,
        )
    else {
        return;
    };
    let rebuild_manifests = generation_store
        .read_rebuild_manifest()
        .ok()
        .flatten()
        .into_iter()
        .collect::<Vec<_>>();
    let _ = store_dir;
    let rebuilt = build_reference_manifest(
        pinned.catalog(),
        &code_read_view.git_overlays,
        &rebuild_manifests,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
    );
    match evaluate_history_gc(&generation_store, &rebuilt) {
        bbox_indexing::index::history_gc::HistoryGcEnablementV1::Enabled { roots, divergence } => {
            // The divergence itself is already logged at warn inside the
            // evaluation, so this stays a plain info: at startup a stale
            // persisted index is the expected state after any history
            // operation in the previous run (D-038).
            tracing::info!(
                referenced_generations = roots.len(),
                accepted_stale_index = divergence.is_some(),
                "repo-history reference manifest rebuilt; history GC enabled"
            );
        }
        bbox_indexing::index::history_gc::HistoryGcEnablementV1::Disabled { diagnostic } => {
            tracing::warn!(%diagnostic, "history GC is disabled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn unreadable_committed_config_preserves_materialized_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let registry = Arc::new(RwLock::new(
            ProjectRegistry::open(&temp.path().join("projects.json")).unwrap(),
        ));
        let record = registry.write().register_path(&project).unwrap();
        registry
            .write()
            .sync_declared_aliases(
                &record.project_id,
                &BTreeSet::from(["durable-alias".to_string()]),
            )
            .unwrap();

        let changed = sync_project_aliases_at_startup(&registry, |_| {
            anyhow::bail!("committed config temporarily unreadable")
        });

        assert!(!changed);
        assert_eq!(
            registry
                .read()
                .resolve("durable-alias")
                .unwrap()
                .unwrap()
                .project_id,
            record.project_id
        );
    }

    #[test]
    fn language_backfill_does_not_walk_without_checkout_authority() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let projects = Arc::new(RwLock::new(
            ProjectRegistry::open(temp.path().join("projects.json")).unwrap(),
        ));
        projects.write().register_path(&project).unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let broker = bbox_indexing::checkout_access::CheckoutAccessBroker::new(
            Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
            bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
        );

        assert!(!backfill_project_languages(&projects, &broker));
        assert!(projects.read().list()[0].languages.is_empty());
    }
}
