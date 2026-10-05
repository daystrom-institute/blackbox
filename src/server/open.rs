use super::startup::{configure_dispatch_mcp_env, discover_transcript_roots, resolve_codex_root};
use super::{SharedState, is_loopback_bind};
use anyhow::Context;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::checkout_mutations::CheckoutMutations;
use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::orchestration::TaskStore;
use crate::orchestration::tail::TailEvent;
use crate::producer_claims::ProducerClaims;
use crate::projects::ProjectRegistry;
use crate::store_persister::StorePersister;
use crate::threads::Threads;
use crate::{
    artifacts, config, index, orchestration, slack_channel_bindings, slack_proposal_links,
    system_memory, tool_docs, vectors,
};

pub(super) struct OpenedServer {
    pub(super) cfg: config::Config,
    pub(super) shared: Arc<SharedState>,
    pub(super) store_dir: PathBuf,
    pub(super) bind_host: String,
    pub(super) bind_is_loopback: bool,
}

/// Open the accepted-publication authority and verify every catalog
/// project's pointer before the read view is built (Phase 5 plan section
/// 5.4, P5-A). Bridge mode returns `None`: its published reads keep the
/// legacy publisher authority untouched.
///
/// Only a global-store failure propagates. Per-project damage is reported
/// and leaves that project's published capability unavailable.
fn open_accepted_publications(
    catalog_store: &Option<Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>>,
    projects_path: &Path,
) -> anyhow::Result<
    Option<Arc<bbox_indexing::accepted_publication_runtime::AcceptedPublicationRuntime>>,
> {
    use bbox_corpus_core::project_catalog::ProjectScope;

    let Some(store) = catalog_store else {
        return Ok(None);
    };
    let runtime = Arc::new(
        bbox_indexing::accepted_publication_runtime::AcceptedPublicationRuntime::open_global(
            projects_path,
        )
        .map_err(|error| anyhow::anyhow!("accepted-publication store open: {error}"))?,
    );
    let snapshot = store
        .snapshot()
        .map_err(|error| anyhow::anyhow!("catalog snapshot for the accepted scan: {error}"))?;
    let targets: Vec<_> = snapshot
        .catalog()
        .projects
        .iter()
        .map(|(project_id, project)| {
            let scope = match &project.scope {
                ProjectScope::Published(scope) => Some(scope.clone()),
                ProjectScope::LegacyLocal | ProjectScope::Connector(_) => None,
            };
            (project_id.clone(), scope)
        })
        .collect();
    let scan = runtime
        .startup_scan(targets)
        .map_err(|error| anyhow::anyhow!("accepted-publication startup scan: {error}"))?;
    tracing::info!(
        scanned = scan.scanned(),
        current = scan.current(),
        prior = scan.prior(),
        missing = scan.missing(),
        corrupt = scan.corrupt(),
        "Accepted publication: pre-bind scan"
    );
    for (project_id, failure) in scan.failures() {
        tracing::warn!(
            project_id = %project_id,
            code = failure.code(),
            "published capability unavailable for this project"
        );
    }
    if scan.dropped_failures() > 0 {
        tracing::warn!(
            dropped = scan.dropped_failures(),
            "further accepted-publication failures were not reported individually"
        );
    }
    Ok(Some(runtime))
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

/// Open every durable store this daemon serves.
///
/// `cfg` is the config `run` already loaded and claimed roots from, not a
/// fresh load: reloading here would open stores the held locks may not cover
/// (R32F2). `instance_locks` is proof of that claim — R31F1 requires every
/// root to be locked BEFORE anything below it is opened, read, or repaired,
/// because the listener bind, the only other exclusivity this process has,
/// happens much later: after the corpus index opens, after local-activation
/// recovery, and after the coordinator-held pin clear that unlinks every
/// writer temporary it walks past. A duplicate or leaked daemon that got that
/// far reclaimed a LIVE peer's in-flight publication and failed the peer's
/// reindex with an ENOENT.
pub(super) fn open_shared_state(
    home: &Path,
    cfg: config::Config,
    instance_locks: &super::instance_lock::InstanceLockSet,
) -> anyhow::Result<OpenedServer> {
    debug_assert!(
        instance_locks.covers(&cfg.paths.state_dir),
        "open_shared_state ran without a claim on the state root"
    );
    debug_assert!(
        instance_locks.covers(&cfg.paths.vectors_path),
        "open_shared_state ran without a claim on the vector root"
    );
    debug_assert!(
        instance_locks.covers(&cfg.paths.global_common_md),
        "open_shared_state ran without a claim on the global common render target"
    );
    debug_assert!(
        instance_locks.covers(&cfg.paths.global_claude_md),
        "open_shared_state ran without a claim on the global Claude render target"
    );
    debug_assert!(
        instance_locks.covers(&cfg.paths.global_codex_md),
        "open_shared_state ran without a claim on the global Codex render target"
    );
    debug_assert!(
        instance_locks.covers(&cfg.paths.global_gemini_md),
        "open_shared_state ran without a claim on the global Gemini render target"
    );
    // R33F1: install the ONE resolved vector root before anything can reach
    // `vectors::global()`. The warmup thread, the embed queue, and the
    // shutdown flush all open the store through it; without this they would
    // open the platform default while the migration inventory and the
    // retirement discharge read the configured root.
    vectors::install_global_root(cfg.paths.vectors_path.clone());
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
    let knowledge_transport_observations =
        bbox_indexing::knowledge_transport_observations::KnowledgeTransportObservationsV1::open(
            store_dir.join("knowledge-transport-observations.json"),
        )?;
    // The retired cutovers leave their markers, receipts, proofs, and
    // evidence behind; archive them once, before anything else reads the
    // state root.
    archive_retired_cutover_state_at_startup(&cfg.paths.state_dir, &store_dir);
    let render_issuances = bbox_indexing::render_issuances::RenderIssuancesV1::new();
    let render_operations = Arc::new(super::render_operations::RenderOperationRuntime::open(
        &store_dir.join("render-operations"),
    )?);
    // Bridge-only handles stay `Option` so catalog mode never constructs a
    // version-1 registry or its persister.
    let mut projects_store: Option<Arc<RwLock<ProjectRegistry>>> = None;
    let mut catalog_store: Option<Arc<bbox_indexing::project_catalog_store::ProjectCatalogStore>> =
        None;
    let mut catalog_records_provider: Option<
        Arc<bbox_indexing::catalog_records::CatalogProjectRecordsProvider>,
    > = None;
    let mut projects_needs_persist = false;
    let knowledge_transport_cutover = Arc::new(
        if matches!(
            store_probe,
            bbox_indexing::project_catalog_store::ProjectStoreProbe::CatalogV2
        ) {
            bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1::open(
                &cfg.paths.state_dir,
            )
            .map_err(|error| anyhow::anyhow!("knowledge transport cutover startup gate: {error}"))?
        } else {
            bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1::default(
            )
        },
    );
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
            let provider = Arc::new(
                bbox_indexing::catalog_records::CatalogProjectRecordsProvider::new(store.clone()),
            );
            catalog_records_provider = Some(provider.clone());
            (
                Arc::new(
                    bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(store),
                ),
                provider,
            )
        }
    };
    let checkout_access = {
        let broker =
            bbox_indexing::checkout_access::CheckoutAccessBroker::new_with_lifecycle_writer_wait(
                access_authority,
                checkout_access_observations.clone(),
                std::time::Duration::from_millis(cfg.daemon.checkout_lifecycle_writer_wait_ms),
            );
        if cfg.daemon.no_checkout_authority {
            tracing::info!(
                "Checkout authority: none (daemon.no_checkout_authority); checkout access is refused before resolution"
            );
            Arc::new(broker.without_checkout_authority())
        } else {
            Arc::new(broker)
        }
    };
    let checkout_policy = bbox_indexing::checkout_access::CheckoutAccessPolicyChain::new()
        .with_policy(
            super::knowledge_source::KnowledgeTransportCheckoutPolicy::new(
                knowledge_transport_cutover.clone(),
            ),
        );
    checkout_access
        .install_policy(Arc::new(checkout_policy))
        .map_err(anyhow::Error::new)?;
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
                cfg.paths.vectors_path.clone(),
            ),
            None => bbox_indexing::index::schema_rebuild::bridge_schema_replacement_guard(
                records_provider.clone(),
            ),
        };
    // DAEMON STARTUP REMAINS `SchemaMismatch`-ONLY (adjudication Q-F): `force`
    // is false here and only the offline `path-free-rebuild --apply` ever
    // passes true. What this call DOES carry forward is the recovery
    // classification above, which the boundary honors: a Prepared or Committed
    // manifest that survived the destructive drop means the marker-less index
    // on disk is a replacement in flight, not a pre-marker index to drop.
    // Without that, a daemon restart into crash state (3) or (4) would re-enter
    // the guard and mint a second manifest over generations the first already
    // pins - and in state (4) would drop a finished replacement.
    let replacement_intent =
        crate::project_catalog_rebuild_admin::replacement_intent_for(&rebuild_resume, false);
    let mut idx = TranscriptIndex::open_or_create_at_replacement_boundary(
        &index_path,
        roots,
        codex_root,
        projects_path.clone(),
        cfg.paths.state_dir.join("code-sources"),
        kb_path.clone(),
        th_path.clone(),
        records_provider.clone(),
        Some(schema_replacement_guard),
        replacement_intent,
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
    idx.set_native_sources(
        cfg.paths.state_dir.join("transcript-sources"),
        if cfg.source_connectors.enabled {
            cfg.source_connectors
                .producers
                .iter()
                .flat_map(|producer| producer.scopes.iter())
                .filter(|grant| grant.profile == config::ConnectorProfile::Transcript)
                .map(|grant| grant.scope())
                .collect()
        } else {
            Vec::new()
        },
    );
    // Read enrollment is explicit operator authority: live conversation
    // grants or validated retained read-only scopes, never store discovery.
    let conversation_root = cfg.paths.state_dir.join("conversation-sources");
    let conversation_catalog = catalog_store
        .as_ref()
        .map(|store| store.snapshot())
        .transpose()?;
    let conversation_enrollments = super::conversation_enrollment::resolve(
        &cfg.source_connectors,
        conversation_catalog
            .as_ref()
            .map(|snapshot| snapshot.catalog().as_ref()),
        &conversation_root,
    )?;
    idx.set_conversation_sources(conversation_root, conversation_enrollments);
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
    let mut kb = Knowledge::open(&kb_path)?;
    kb.set_path_fallback_cut(path_fallback_cut);
    tracing::info!("Knowledge store: {}", kb_path.display());
    // Load each registered repo's committed .bbox/knowledge/ into the query
    // surface BEFORE any save. This must precede sync_tool_docs (which writes
    // the tool-reference entry and triggers save()): a save with the repo's
    // entries not yet loaded would treat the in-memory set as authoritative and
    // purge the committed .bbox/knowledge files for a repo-owned project.
    // Catalog-mode base carriers name their attachment natively, read from
    // the same catalog epoch as the records they describe (F4); the bridge
    // keeps `Selected`, whose encoding must stay byte-identical. Startup has
    // no last-good carrier set to preserve, so an unreadable catalog refuses
    // here rather than installing a moving-ladder carrier.
    // Mode-independent record view: the bridge derives from the registry,
    // catalog mode from the compatibility projection (attached rows only).
    let carrier_inputs = super::repo_io::CatalogBaseTargets::read_consistent(
        &records_provider,
        catalog_store.as_deref(),
    )?;
    let registered_projects = carrier_inputs.records.clone();
    let catalog_base_targets = carrier_inputs.targets;
    let local_repo_projects = registered_projects
        .iter()
        .filter(|project| !knowledge_transport_cutover.covers_project_str(&project.project_id))
        .cloned()
        .collect::<Vec<_>>();
    if let Err(e) = kb.configure_repo_io(
        repo_io.clone(),
        repo_io.clone(),
        super::repo_io::RepoIoAuthority::knowledge_base_carriers(
            &local_repo_projects,
            catalog_base_targets.as_ref(),
        )?,
    ) {
        tracing::warn!("kb repository-carrier load at startup: {e:#}");
    }
    let tool_docs_synced = sync_tool_docs(&mut kb);

    // Gap store mirrors the kb repo-owned model. Load every registered repo's
    // committed `.bbox/gaps/` into the query surface BEFORE any producer
    // (gap-spool import) can save; a save with the repo's
    // gaps not yet loaded would treat the in-memory set as authoritative and
    // purge committed `.bbox/gaps/` files for a repo-owned project.
    let gaps_path = cfg.paths.gaps_path.clone();
    let mut gaps_store = crate::gaps::GapStore::open(&gaps_path)?;
    gaps_store.set_path_fallback_cut(path_fallback_cut);
    tracing::info!("Gap store: {}", gaps_path.display());
    if let Err(e) = gaps_store.configure_repo_io(
        repo_io.clone(),
        repo_io,
        super::repo_io::RepoIoAuthority::gap_base_carriers(
            &local_repo_projects,
            catalog_base_targets.as_ref(),
        )?,
    ) {
        tracing::warn!("gaps repository-carrier load at startup: {e:#}");
    }
    load_system_memory_catalog(&cfg)?;
    configure_dispatch_mcp_env(&cfg)?;

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
    crate::index::writer_actor::register_embed_bootstrap(
        crate::embed_queue::register_index_embed_hooks,
    );
    crate::providers::register_extra_providers(crate::providers_ext::extra_providers());
    tracing::info!("Thread store: {}", th_path.display());
    // Queued on the writer actor: boot no longer races the reindex thread
    // (or a winding-down previous daemon) for tantivy's single-writer lock.
    index_writer.enqueue(index::IndexWriteOp::UpsertThreadsStore(th.all().to_vec()));
    let threads_store = Arc::new(RwLock::new(th));
    let threads_persister =
        StorePersister::spawn("threads", threads_store.clone(), th_path.clone());

    let checkout_mutations_path = cfg.paths.checkout_mutations_path.clone();
    let checkout_mutations_store = Arc::new(RwLock::new(CheckoutMutations::open(
        &checkout_mutations_path,
    )?));
    let checkout_mutations_persister = StorePersister::spawn(
        "checkout-mutations",
        checkout_mutations_store.clone(),
        checkout_mutations_path.clone(),
    );
    tracing::info!(
        "Checkout mutations store: {}",
        checkout_mutations_path.display()
    );

    let producer_claims_path = cfg.paths.producer_claims_path.clone();
    let producer_claims_store = Arc::new(RwLock::new(ProducerClaims::open(&producer_claims_path)?));
    let producer_claims_persister = StorePersister::spawn(
        "producer-claims",
        producer_claims_store.clone(),
        producer_claims_path.clone(),
    );
    tracing::info!("Producer claims store: {}", producer_claims_path.display());

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

    let artifacts_dir = cfg.paths.artifacts_dir.clone();
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
    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);
    let (roster_tx, _) = broadcast::channel::<bro_protocol::RosterDelta>(1024);
    let code_sources = Arc::new(super::code_source::CodeSourceRuntime::open(
        &cfg,
        &records_provider.records_snapshot().records,
        catalog_store.clone(),
        checkout_access.clone(),
        &producer_claims_store.read().records_snapshot(),
    )?);
    // Opened beside the code lane and unconditionally: the store is a
    // directory tree with no producer state, so opening it when no connector
    // is configured costs two `mkdir -p` calls and keeps `SharedState`
    // non-optional for every reader.
    let file_sources = Arc::new(super::file_source::FileSourceRuntime::open(&cfg)?);
    // Same reasoning as the file lane above: a directory tree with no
    // producer state, so opening it unconditionally keeps every reader's
    // access non-optional.
    let conversation_sources = Arc::new(
        super::conversation_source::ConversationSourceRuntime::open(&cfg)?,
    );
    let git_sources = Arc::new(super::git_source::GitSourceRuntime::open(&cfg)?);
    let knowledge_sources = Arc::new(super::knowledge_source::KnowledgeSourceRuntime::open(&cfg)?);

    // R21F1: unconditional pre-bind transaction recovery. Runs before
    // selector refresh, read-view construction, and edge-index loading.
    // The Tantivy commit payload carries the cryptographic commitments
    // for all transactions that were committed atomically with the index
    // commit. Recovery uses these to distinguish committed-but-unfinalized
    // transactions (resume finalization) from uncommitted ones (discard
    // staging). R20F5: crash-idempotent finalization handles mid-rename
    // restarts.
    // R21F1: load_metas errors ABORT recovery with a typed error, never
    // silently become None (which would treat all pending journals as
    // uncommitted and discard committed transactions).
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&projects_path);
    let commit_payload: String = idx
        .index_handle()
        .load_metas()
        .context("recovery: load_metas failed, cannot determine committed transactions")?
        .payload
        .unwrap_or_default();
    bbox_edge_sidecar::snapshot::recover_pending_transactions_prebind(
        &edges_dir,
        Some(&commit_payload),
    )
    .context("pre-bind transaction recovery failed")?;

    // Edge families no reader consumes (the transcript-edge split lanes, the
    // managed project lane directory and the split-lane migration records)
    // leave the store once. The store-level marker makes every later start a
    // single stat.
    retire_legacy_edge_lanes_at_startup(&edges_dir)?;

    // Retired transcript file-touch rows leave the remaining legacy lanes
    // once. The store-level marker makes every later start a single stat.
    purge_file_touch_edges_at_startup(&edges_dir)?;

    // Pre-bind catalog-mode recovery (P4-F section 10.1 steps 5-8):
    // once-only classification, relationship chain validation,
    // retirement-journal detection, and startup reducer sweep. All run
    // BEFORE the schema rebuild, reindex, and CodeReadView construction
    // so that a broken relationship chain fails closed before the daemon
    // builds any read view from corrupt state. Bridge mode is a no-op.
    super::code_source::pre_bind_catalog_recovery(
        &project_authority,
        &code_sources,
        &checkout_access,
        &store_dir,
        &projects_path,
    )?;
    idx.refresh_active_code_selectors()
        .context("refreshing active code selectors after pre-bind catalog recovery")?;

    // Pre-bind accepted-publication authority (Phase 5 plan section 5.4).
    // The ordering relation is load-bearing: catalog recovery first, the
    // global accepted-store open second, the per-project scan third,
    // `CodeReadView` construction fourth, listener bind last.
    //
    // The two failure policies differ deliberately. Losing the global store
    // means this process cannot act as the accepted-publication authority
    // at all, so it must not bind. A single project whose pointer is
    // missing or whose generations do not verify loses only its own
    // published capability; code search and every other project keep
    // serving. Bridge mode never constructs the runtime.
    let accepted_publications = open_accepted_publications(&catalog_store, &projects_path)?;

    // The grant table is built after the writer actor spawns, so the
    // planner's assignment view is installed here rather than passed to
    // `spawn` (same shape as the post-commit searcher hook).
    index_writer.set_producer_assignment_source(code_sources.clone());
    // THE shared replacement driver (P6-B task 5, adjudication Q-D). Daemon
    // startup and the offline `path-free-rebuild --apply` call this same
    // function; its ordering, its resume arm, and its reasons all live in
    // `project_catalog_rebuild_admin` rather than being restated here.
    crate::project_catalog_rebuild_admin::drive_catalog_schema_replacement(
        &idx,
        &index_writer,
        &rebuild_resume,
    )?;

    // THE P6-C STARTUP VALIDATION GATE, before any v2 route binds.
    //
    // It runs AFTER the driver on purpose. On the boot that performs the
    // replacement, the committed manifest does not exist until the drive
    // above finishes, so a gate placed before it would refuse the very boot
    // that produces the evidence it wants. On every later boot the drive is a
    // no-op and the ordering is immaterial.
    //
    // Bridge mode has no catalog store and therefore no origin to scope the
    // gate to; the gate is a catalog-mode contract and does not apply.
    if let Some(store) = &catalog_store {
        let coverage = crate::project_catalog_rebuild_admin::validate_rebuild_coverage_before_bind(
            store,
            &index_path,
        )
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: {} (rebuilt history cannot be verified, so this daemon must not \
                     serve it)",
                error.code,
                error.message
            )
        })?;
        tracing::info!(?coverage, "rebuild coverage gate");
    }

    // GH-C startup recovery: producer-backed Git overlays are not allowed
    // into the first read view merely because their selector survived a
    // crash. Re-prove the exact Tantivy generation and each durable snapshot
    // receipt, clearing only invalid producer arms for background repair.
    super::history_activation::recover_prebind(
        &project_authority,
        &code_sources,
        &git_sources,
        &idx,
        &index_path,
    )?;

    // Shared reindex trigger. Initialized `true` so the first background pass
    // runs once after startup and indexes repo-owned `.bbox/knowledge` that may
    // have changed while the daemon was down (no watcher event fires for those,
    // and `needs_reindex` does not track them). The `.bbox/knowledge` watcher
    // sets it on live changes; the same `Arc` is stored in `SharedState`.
    let reindex_dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
    // The journal-currency rule decides which repositories producer
    // transport owns, so the reindex pass takes no checkout Git-history lease
    // for them. Installed before the first pass.
    if let (Some(provider), Some(store)) = (&catalog_records_provider, &catalog_store) {
        provider.install_history_transport_owner(
            super::history_activation::history_transport_owner(
                store.clone(),
                code_sources.clone(),
                git_sources.clone(),
                bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
                    &idx.reindex_config().projects_path,
                ),
            ),
        );
    }
    spawn_reindex_thread(&cfg, &idx, index_writer.clone(), reindex_dirty.clone());

    let bind_host = cfg.daemon.bind.clone();
    let bind_is_loopback = is_loopback_bind(&bind_host);
    let code_read_view = super::CodeReadView {
        active_selectors: idx.active_code_selectors(),
        searcher: idx.searcher(),
        // Seeded from the boot catalog snapshot, so the startup view and the
        // first runtime republish agree.
        catalog_epoch: records_provider.records_snapshot().authority_epoch,
        // Same boot snapshot, same rule: the startup view pins the durable
        // overlay selection the manifest names, which is also the Git GC
        // root set the reference manifest below is built from.
        git_overlays: super::state::read_git_overlays_for_view(
            &project_authority,
            &bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
                &idx.reindex_config().projects_path,
            ),
        ),
    };
    // Phase 3 plan section 10 item 4: the derived repo-history reference
    // manifest is rebuilt and checksummed from durable inputs at startup,
    // BEFORE anything can sweep. A mismatch leaves history GC disabled and
    // surfaces in doctor; it never blocks the open, because a stale
    // acceleration index must not cost history reads.
    refresh_history_reference_manifest(&cfg, &project_authority, &store_dir, &code_read_view);
    let (code_view_refresh_nudge_tx, code_view_refresh_nudge_rx) = std::sync::mpsc::sync_channel(1);
    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        index_writer,
        kb: kb_store,
        kb_persister,
        gaps: RwLock::new(gaps_store),
        threads: threads_store,
        threads_persister,
        checkout_mutations: checkout_mutations_store,
        checkout_mutations_persister,
        producer_claims: producer_claims_store,
        producer_claims_persister,
        producer_claim_lock: tokio::sync::Mutex::new(()),
        producer_commands: Arc::new(super::producer_commands::ProducerCommandRuntime::new()),
        producer_currency: Arc::new(super::producer_currency::ProducerCurrencyRuntime::new()),
        render_operations,
        project_authority,
        accepted_publications,
        records_provider,
        mcp_modern_lifecycle: cfg.daemon.mcp_modern_lifecycle.into(),
        project_selector_cache: Default::default(),
        checkout_registry,
        checkout_access_observations,
        checkout_access,
        knowledge_transport_observations,
        render_issuances,
        // Publisher refs define authority and cannot be reconstructed from
        // checkout discovery without silently moving published truth. Keep
        // corrupt pins fail-closed even though the checkout census below is a
        // recoverable discovery index.
        publisher_refs: RwLock::new(bbox_indexing::publisher::PublisherRefStore::open(
            store_dir.join("publisher-refs.json"),
        )?),
        path_fallback_cut: std::sync::atomic::AtomicBool::new(path_fallback_cut),
        knowledge_published_cache: RwLock::new(Default::default()),
        gap_published_cache: RwLock::new(Default::default()),
        catalog_knowledge_published_cache: RwLock::new(Default::default()),
        catalog_gap_published_cache: RwLock::new(Default::default()),
        project_graph_views: RwLock::new(Default::default()),
        publisher_authorization_cache: RwLock::new(Default::default()),
        artifacts: RwLock::new(artifacts_store),
        bbox_watcher: std::sync::Mutex::new(None),
        reindex_dirty,
        code_read_view: RwLock::new(Arc::new(code_read_view)),
        code_sources,
        file_sources,
        conversation_sources,
        git_sources,
        knowledge_sources,
        knowledge_transport_cutover,
        reconciler_shutdown: parking_lot::RwLock::new(Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        )),
        code_view_refresh_nudge_tx,
        code_view_refresh_nudge_rx: std::sync::Mutex::new(Some(code_view_refresh_nudge_rx)),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx,
        roster_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        roster_tx,
        roster_view: Arc::new(orchestration::RosterView::new()),
        store_dir: store_dir.clone(),

        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        drain: super::drain::DrainState::open(&store_dir),
        long_polls: Arc::new(super::drain::LongPollRegistry::new()),
        slack_channel_bindings: Arc::new(
            slack_channel_bindings::SlackChannelBindings::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack channel bindings at {store_dir:?}: {e}")),
        ),
        slack_proposal_links: Arc::new(
            slack_proposal_links::SlackProposalLinks::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack proposal links at {store_dir:?}: {e}")),
        ),
        config: cfg_arc,

        vector_store: Arc::new(
            vectors::VectorStore::open_unloaded(cfg.paths.vectors_path.clone())
                .expect("default vector store placeholder should open"),
        ),
    });
    shared.install_code_read_view_commit_hook();

    // M9a boot reconciliation: `project_graph_views` is in-memory while the
    // graph word-lane documents are durable, so a graph disabled or removed
    // while the daemon was down would stay searchable until the next install
    // (and a schema-replacement rebuild leaves every lane empty until then).
    // One published-view install per catalog project converges both cases.
    if let Some(store) = &catalog_store {
        let published_projects: Vec<bbox_corpus_core::project_catalog::ProjectId> =
            match store.snapshot() {
                Ok(snapshot) => snapshot
                    .catalog()
                    .projects
                    .iter()
                    .filter(|(_, project)| {
                        matches!(
                            project.scope,
                            bbox_corpus_core::project_catalog::ProjectScope::Published(_)
                        )
                    })
                    .map(|(project_id, _)| project_id.clone())
                    .collect(),
                Err(error) => {
                    // Degrade-and-warn, matching the reconcile's own
                    // per-project policy: a snapshot failure skips this boot
                    // pass, and the next install (or a later boot) converges
                    // the lanes rather than taking the whole daemon down.
                    tracing::warn!(
                        error = %error,
                        "catalog snapshot failed; skipping boot graph view reconcile"
                    );
                    Vec::new()
                }
            };
        super::knowledge_view::reconcile_published_graph_word_lanes_at_boot(
            &shared,
            published_projects,
        );
    }

    // Reconcile every granted connector scope against the derived manifest.
    // The activation record, the manifest, and the state flip are three
    // writes across two stores that share no transaction, so a crash between
    // any pair leaves an observable tear; the store's classifier is total
    // over those shapes and this is the one call that consumes it. Runs after
    // the read view exists because a forward republish swaps it.
    super::file_source_activation::recover_connector_activations(&shared);

    // Re-enqueue any connector selector retirement a prior process deferred
    // on writer readiness (gap-7e44ee3b): without this, a deferral that
    // outlives the daemon strands its corpus permanently instead of being
    // redriven to completion on the next boot.
    super::file_source_activation::recover_connector_retirements(&shared);

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

/// Pre-bind archive of every retired cutover's markers, receipts, proofs,
/// and evidence. Nothing reads those files, so a failure to move them is
/// logged and never holds the daemon down; the next start retries whatever
/// is left.
fn archive_retired_cutover_state_at_startup(
    state_dir: &Path,
    bro_home: &Path,
) -> Vec<bbox_indexing::cutover_retirement::CutoverRetirement> {
    let label = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut retirements = Vec::new();
    for cutover in bbox_indexing::cutover_retirement::RETIRED_CUTOVERS {
        match bbox_indexing::cutover_retirement::archive_retired_cutover_state(
            cutover, state_dir, bro_home, &label,
        ) {
            Ok(retirement) => {
                if let Some(archive_dir) = &retirement.archive_dir {
                    tracing::info!(
                        cutover = cutover.name,
                        archive_dir = %archive_dir.display(),
                        files = ?retirement.archived,
                        "archived retired cutover state"
                    );
                }
                retirements.push(retirement);
            }
            Err(error) => {
                tracing::warn!(
                    cutover = cutover.name,
                    error = %format!("{error:#}"),
                    "archiving retired cutover state failed; the files stay inert in place"
                );
            }
        }
    }
    retirements
}

/// Pre-bind removal of edge families no reader consumes.
fn retire_legacy_edge_lanes_at_startup(
    edges_dir: &Path,
) -> anyhow::Result<bbox_edge_sidecar::legacy_lane_retirement::LegacyLaneRetirementStats> {
    let stats = bbox_edge_sidecar::legacy_lane_retirement::retire_legacy_edge_lanes(edges_dir)
        .context("pre-bind legacy edge lane retirement failed")?;
    if stats.families_removed > 0 {
        tracing::info!(
            families_removed = stats.families_removed,
            files_removed = stats.files_removed,
            bytes_removed = stats.bytes_removed,
            "retired edge families removed"
        );
    }
    Ok(stats)
}

/// Pre-bind purge of retired file-touch rows from the durable edge lanes.
fn purge_file_touch_edges_at_startup(
    edges_dir: &Path,
) -> anyhow::Result<bbox_edge_sidecar::file_touch_purge::FileTouchPurgeStats> {
    let stats = bbox_edge_sidecar::file_touch_purge::purge_retired_file_touch_edges(edges_dir)
        .context("pre-bind file-touch edge purge failed")?;
    if stats.lanes_scanned > 0 {
        tracing::info!(
            lanes_scanned = stats.lanes_scanned,
            lanes_rewritten = stats.lanes_rewritten,
            rows_removed = stats.rows_removed,
            bytes_removed = stats.bytes_removed,
            "retired file-touch edge rows purged"
        );
    }
    Ok(stats)
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

    /// Startup archives a legacy-shaped cutover state root once: the
    /// locality markers (including one that no longer parses), a kept
    /// receipt, both locality evidence stores, and the Git transport marker,
    /// receipt, and checkout parity proof leave their live paths for one
    /// archive directory per cutover. The knowledge transport marker stays,
    /// and the next start finds nothing.
    #[test]
    fn startup_archives_retired_cutover_state_once() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().canonicalize().unwrap().join("state");
        let bro_home = state_dir.join("bro");
        std::fs::create_dir_all(&bro_home).unwrap();
        let locality = [
            "render-locality-cutover-marker.json",
            "code-source-locality-cutover-marker.json",
            "code-source-locality-cutover-receipt.json",
            "blame-locality-cutover-marker.json",
            "code-source-locality-observations.json",
        ];
        let git = [
            "git-transport-checkout-parity-proof.json",
            "git-transport-cutover-marker.json",
            "git-transport-cutover-receipt.json",
        ];
        for name in locality.iter().chain(&git) {
            std::fs::write(state_dir.join(name), b"{ not json").unwrap();
        }
        std::fs::write(bro_home.join("render-locality-observations.json"), b"{}").unwrap();
        std::fs::write(
            state_dir.join("knowledge-transport-cutover-marker.json"),
            b"{}",
        )
        .unwrap();

        let first = archive_retired_cutover_state_at_startup(&state_dir, &bro_home);
        assert_eq!(first.len(), 2);
        let locality_archive = first[0].archive_dir.clone().unwrap();
        let git_archive = first[1].archive_dir.clone().unwrap();
        assert!(locality_archive.starts_with(state_dir.join("cutover-artifacts")));
        assert!(
            git_archive
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("retired-git-transport-")
        );
        assert_eq!(first[0].archived.len(), locality.len() + 1);
        assert_eq!(first[1].archived, git);
        for name in locality {
            assert!(!state_dir.join(name).exists(), "{name}");
            assert!(locality_archive.join(name).is_file(), "{name}");
        }
        for name in git {
            assert!(!state_dir.join(name).exists(), "{name}");
            assert!(git_archive.join(name).is_file(), "{name}");
        }
        assert!(
            locality_archive
                .join("render-locality-observations.json")
                .is_file()
        );
        assert!(
            state_dir
                .join("knowledge-transport-cutover-marker.json")
                .is_file()
        );

        let second = archive_retired_cutover_state_at_startup(&state_dir, &bro_home);
        assert!(
            second
                .iter()
                .all(|retirement| retirement.archive_dir.is_none())
        );
        assert_eq!(
            std::fs::read_dir(state_dir.join("cutover-artifacts"))
                .unwrap()
                .count(),
            2
        );
    }

    /// A never-provisioned state root has nothing to archive and gains no
    /// archive directory.
    #[test]
    fn startup_archive_creates_nothing_on_a_never_provisioned_root() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().canonicalize().unwrap().join("state");
        let result = archive_retired_cutover_state_at_startup(&state_dir, &state_dir.join("bro"));
        assert!(
            result
                .iter()
                .all(|retirement| retirement.archive_dir.is_none())
        );
        assert!(!state_dir.exists());
    }

    /// The startup retirement is the daemon's pass over a real edge root: a
    /// store carrying the transcript-edge split lanes, the managed project
    /// lane directory and split-lane migration records loses them, snapshots
    /// and Git source lanes stay, and the next start inspects nothing. The
    /// file-touch purge that follows finds no split lane left to rewrite.
    #[test]
    fn startup_retires_legacy_edge_families_once() {
        let temp = tempfile::tempdir().unwrap();
        let edges_dir = temp.path().canonicalize().unwrap().join("edges");
        let project = "p_00000000000000000000000000000001";
        let write = |relative: String, contents: &str| {
            let path = edges_dir.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        let bash = r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact"}"#;
        write(format!("observed/{project}.jsonl"), &format!("{bash}\n"));
        write(format!("explicit/{project}.jsonl"), "");
        write(format!("derived/project/{project}.jsonl"), "{}\n");
        write("migrations/m1/manifest.json".into(), "{}");
        write(format!("derived/git/{project}.jsonl"), "{}\n");
        write("materialized/manifest-index.json".into(), "{}");

        let first = retire_legacy_edge_lanes_at_startup(&edges_dir).unwrap();
        assert_eq!(first.families_removed, 4);
        for gone in ["observed", "explicit", "derived/project", "migrations"] {
            assert!(!edges_dir.join(gone).exists(), "{gone} must be removed");
        }
        assert!(
            edges_dir
                .join(format!("derived/git/{project}.jsonl"))
                .is_file()
        );
        assert!(edges_dir.join("materialized/manifest-index.json").is_file());

        let purge = purge_file_touch_edges_at_startup(&edges_dir).unwrap();
        assert_eq!(purge.lanes_scanned, 0);

        let second = retire_legacy_edge_lanes_at_startup(&edges_dir).unwrap();
        assert!(second.already_complete);
    }

    /// The startup purge is the daemon's pass over a real edge root: a
    /// store carrying file-touch rows loses them and is marked, and the
    /// next start opens no lane.
    #[test]
    fn startup_purges_file_touch_rows_once() {
        let temp = tempfile::tempdir().unwrap();
        let edges_dir = temp.path().canonicalize().unwrap().join("edges");
        let lane = edges_dir
            .join("observed")
            .join("p_00000000000000000000000000000001.jsonl");
        std::fs::create_dir_all(lane.parent().unwrap()).unwrap();
        let bash = r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"exact"}"#;
        let read = r#"{"source":{"type":"task","task_id":"one"},"kind":"READ_FILE","target":{"type":"task","task_id":"two"},"provenance":"explicit","confidence":"heuristic"}"#;
        std::fs::write(&lane, format!("{read}\n{bash}\n")).unwrap();

        let first = purge_file_touch_edges_at_startup(&edges_dir).unwrap();
        assert_eq!(first.rows_removed, 1);
        assert_eq!(std::fs::read_to_string(&lane).unwrap(), format!("{bash}\n"));

        let second = purge_file_touch_edges_at_startup(&edges_dir).unwrap();
        assert!(second.already_complete);
        assert_eq!(second.lanes_scanned, 0);
    }

    #[test]
    fn bridge_mode_never_opens_the_accepted_publication_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(
            open_accepted_publications(&None, &root.join("projects.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn catalog_mode_opens_and_scans_before_the_read_view() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let store = Arc::new(
            bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
                &projects_path,
            )
            .unwrap(),
        );

        // A catalog with no published project scans clean: an absent
        // accepted store is the state of a catalog that has not published.
        assert!(
            open_accepted_publications(&Some(store), &projects_path)
                .unwrap()
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_global_accepted_store_blocks_the_bind() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let projects_path = root.join("projects.json");
        let store = Arc::new(
            bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
                &projects_path,
            )
            .unwrap(),
        );
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(root.join("accepted-publications")).unwrap();
        symlink(&elsewhere, root.join("accepted-publications/pointers")).unwrap();

        let error = open_accepted_publications(&Some(store), &projects_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("error.accepted_publication_global_store_unavailable"),
            "{error:#}"
        );
    }

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

    /// A state directory holding a packet tree (records, their lock files,
    /// the event log) and packet artifacts opens and restores cleanly. No
    /// store opens the tree, boot restore skips the retired artifact kind,
    /// and both project-catalog owner captures still parse the legacy rows.
    #[test]
    fn a_state_dir_holding_a_legacy_packet_store_opens() {
        use bbox_corpus_core::project_catalog_snapshot::{
            OwnerSnapshotLimitsV1, OwnerSnapshotStateV1,
        };

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let packets = root.join("packets");
        std::fs::create_dir_all(packets.join("global")).unwrap();
        std::fs::create_dir_all(packets.join("project")).unwrap();
        std::fs::write(
            packets.join("global/packet-0000000a.json"),
            br#"{"id":"packet-0000000a","domain":"legacy/rubric","scope":"global","rules":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(packets.join("global/packet-0000000a.json.lock"), b"").unwrap();
        std::fs::write(
            packets.join("project/packet-0000000b.json"),
            br#"{"id":"packet-0000000b","domain":"legacy/triage","scope":"project","project":"/legacy/repo","rules":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            packets.join("events.jsonl"),
            b"{\"op\":\"compile\",\"outcome\":\"ok\"}\n",
        )
        .unwrap();
        let tree_before = std::fs::read(packets.join("project/packet-0000000b.json")).unwrap();
        crate::artifacts::ArtifactCatalog::open(&root)
            .unwrap()
            .install_value(
                crate::artifacts::ArtifactKind::Packet,
                "legacy".into(),
                &serde_json::json!({"domain":"legacy/rubric","version":1,"scope":"global","rules":[]}),
                None,
                None,
                None,
            )
            .unwrap();

        let state = Arc::new(SharedState::for_test(&root));
        assert_eq!(
            crate::server::routes::restore_runtime_artifacts_from_catalog(&state).unwrap(),
            0
        );

        let limits = OwnerSnapshotLimitsV1::default();
        let artifacts =
            crate::artifacts::capture_project_catalog_owner_snapshot(&root, limits).unwrap();
        assert!(
            matches!(artifacts.state, OwnerSnapshotStateV1::Present { .. }),
            "{:?}",
            artifacts.state
        );
        let tree =
            bbox_indexing::project_catalog_packet_tree::capture_project_catalog_owner_snapshot(
                &packets, limits,
            )
            .unwrap();
        assert!(
            matches!(tree.state, OwnerSnapshotStateV1::Present { .. }),
            "{:?}",
            tree.state
        );
        assert!(
            tree.rows
                .iter()
                .any(|row| row.stable_row_id == "packet-0000000b"),
            "{:?}",
            tree.rows
        );
        assert_eq!(
            std::fs::read(packets.join("project/packet-0000000b.json")).unwrap(),
            tree_before
        );
        assert!(packets.join("events.jsonl").is_file());
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
