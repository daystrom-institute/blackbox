use super::SharedState;
use crate::{config, embed_queue, vectors};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) async fn serve_until_shutdown(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shared: Arc<SharedState>,
    store_dir: PathBuf,
    ct: CancellationToken,
    shutdown_grace_secs: u64,
) -> anyhow::Result<()> {
    spawn_config_reload_handler(shared.clone());
    spawn_shutdown_signal_handler(ct.clone());
    serve_with_grace_period(listener, app, &ct, shutdown_grace_secs).await?;
    persist_shutdown_state(shared, store_dir);
    tracing::info!("blackboxd shut down");
    Ok(())
}

#[cfg(unix)]
fn spawn_config_reload_handler(shared: Arc<SharedState>) {
    tokio::spawn(async move {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("install SIGHUP handler");
        loop {
            let _ = sighup.recv().await;
            match config::load() {
                Ok(new_cfg) => {
                    if let Some(transitions) = install_config_reload(&shared, new_cfg) {
                        super::code_source::apply_source_transitions(shared.clone(), transitions);
                    }
                }
                Err(e) => {
                    tracing::warn!("SIGHUP reload failed: {e}");
                }
            }
        }
    });
}

/// Install one validated configuration replacement and republish the pinned
/// read view before any asynchronous source transition can begin. Producer
/// assignment removal is cutover authority, so readers must stop seeing a
/// covered producer overlay at this synchronous boundary rather than waiting
/// for the cutback reconciler.
#[cfg(any(unix, test))]
fn install_config_reload(
    shared: &Arc<SharedState>,
    new_cfg: crate::config::Config,
) -> Option<super::code_source::SourceTransitions> {
    if let Err(error) = super::git_source::GitSourceRuntime::validate_config(&new_cfg) {
        tracing::warn!(
            error = %error,
            "SIGHUP Git-source limit reload rejected"
        );
        return None;
    }
    if let Err(error) = super::knowledge_source::KnowledgeSourceRuntime::validate_config(&new_cfg) {
        tracing::warn!(
            error = %error,
            "SIGHUP knowledge-source limit reload rejected"
        );
        return None;
    }
    let projects = shared.records_provider.records_snapshot().records;
    let transitions = match shared.code_sources.reload(&new_cfg, &projects) {
        Ok(transitions) => transitions,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "SIGHUP code-collection reload rejected"
            );
            return None;
        }
    };
    if let Err(error) = shared.git_sources.update_limits(&new_cfg) {
        tracing::error!(
            error = %error,
            "SIGHUP Git-source limit reload failed after validated auth reload"
        );
        return None;
    }
    if let Err(error) = shared.knowledge_sources.update_limits(&new_cfg) {
        tracing::error!(
            error = %error,
            "SIGHUP knowledge-source limit reload failed after validated auth reload"
        );
        return None;
    }
    let old_cfg = shared.config.read();
    if old_cfg.daemon.port != new_cfg.daemon.port || old_cfg.daemon.bind != new_cfg.daemon.bind {
        tracing::warn!("SIGHUP reload changed bind/port; requires daemon restart");
    }
    drop(old_cfg);
    *shared.config.write() = new_cfg;
    if let Err(error) = super::code_source::republish_code_read_view(shared) {
        tracing::error!(
            %error,
            "SIGHUP cutover authority republish failed after auth reload"
        );
    }
    Some(transitions)
}

#[cfg(not(unix))]
fn spawn_config_reload_handler(_shared: Arc<SharedState>) {
    tokio::spawn(async {});
}

fn spawn_shutdown_signal_handler(ct: CancellationToken) {
    tokio::spawn(async move {
        // Wait for either Ctrl-C (interactive) or SIGTERM (systemd stop).
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
        ct.cancel();
    });
}

async fn serve_with_grace_period(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    ct: &CancellationToken,
    shutdown_grace_secs: u64,
) -> anyhow::Result<()> {
    let shutdown_grace = std::time::Duration::from_secs(shutdown_grace_secs);
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
    Ok(())
}

fn persist_shutdown_state(shared: Arc<SharedState>, store_dir: PathBuf) {
    // Signal the cutback reconciler background task to drain and exit
    // (P4-D section 8.1 item 1) before durable flushes.
    shared
        .reconciler_shutdown
        .read()
        .store(true, std::sync::atomic::Ordering::Release);
    embed_queue::shutdown();
    // Block until durable: shutdown must not race the persist actor's thread.
    crate::orchestration::flush_persist_blocking(&shared.task_store, &store_dir);
    if let Err(err) = shared.kb_persister.flush_blocking() {
        tracing::warn!(error = %err, "knowledge persister flush on shutdown failed");
    }
    if let Err(err) = shared.threads_persister.flush_blocking() {
        tracing::warn!(error = %err, "threads persister flush on shutdown failed");
    }
    if let Err(err) = shared.pins_persister.flush_blocking() {
        tracing::warn!(error = %err, "pins persister flush on shutdown failed");
    }
    if let crate::server::state::ProjectAuthority::Bridge { persister, .. } =
        &shared.project_authority
        && let Err(err) = persister.flush_blocking()
    {
        tracing::warn!(error = %err, "projects persister flush on shutdown failed");
    }
    if let Err(err) = shared.notes_persister.flush_blocking() {
        tracing::warn!(error = %err, "notes persister flush on shutdown failed");
    }
    if let Err(err) = shared.index_writer.flush_blocking() {
        tracing::warn!(error = %err, "index writer flush at shutdown failed");
    }
    flush_vectors_with_timeout();
}

fn flush_vectors_with_timeout() {
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
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    use bbox_config::config::CodeCollectionProducerConfig;
    use bbox_corpus_core::git_overlay::{GitOverlaySelector, GitOverlaySourceV1};
    use bbox_corpus_core::git_transport_cutover::{
        RepoTransportGrantState, derive_repo_transport_grants,
    };
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        CommitNamespace, CorpusProject, ProjectId, ProjectScope, RecordedRepoAuthority,
        RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization, RepoHistoryRecord,
    };
    use bbox_edge_sidecar::manifest::{ManifestIndex, WorkspaceIndexEntry};
    use bbox_indexing::git_transport_cutover::{
        GitTransportCutoverMarkerV1, GitTransportCutoverRuntimeV1, GitTransportRuntimeCoverageV1,
        PredictedGitTransportCutoverRowV1,
    };
    use bbox_indexing::project_catalog_inventory::Sha256ValueV1;

    use super::*;

    #[test]
    fn config_reload_revokes_covered_producer_overlay_before_transition_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        let catalog_path = root.join("catalog").join("projects.json");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();

        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);

        let project_id = ProjectId::parse("p_0000000000000000000000000000cf11").unwrap();
        let repo_history_id = RepoHistoryId::parse("rh_0000000000000000000000000000cf11").unwrap();
        let scope = PublishedScope::try_new("reload-cutover", ".").unwrap();
        let catalog_store =
            bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
                &catalog_path,
            )
            .unwrap();
        let epoch = catalog_store.snapshot().unwrap().epoch();
        catalog_store
            .transact(epoch, |catalog, _attachments| {
                catalog.repo_histories.insert(
                    repo_history_id.clone(),
                    RepoHistoryRecord {
                        repo_history_id: repo_history_id.clone(),
                        membership_generation: 0,
                        authority: RepoHistoryAuthority::Recorded(
                            RecordedRepoAuthority::parse("reload-cutover").unwrap(),
                        ),
                        primary_namespace: CommitNamespace::parse("reload-cutover").unwrap(),
                        compatibility_namespaces: Default::default(),
                        materialization: RepoHistoryMaterialization::NotBuilt,
                    },
                );
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id: project_id.clone(),
                        scope: ProjectScope::Published(scope.clone()),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "Reload cutover fixture".to_string(),
                        created_at: "unix:1".to_string(),
                        registered_at_compat: None,
                        repo_history: Some(repo_history_id.clone()),
                        languages: Default::default(),
                    },
                );
                Ok(())
            })
            .unwrap();

        let token_file = root.join("producer-token");
        fs::write(&token_file, "a".repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut initial_cfg = crate::config::load().unwrap();
        initial_cfg.code_collection.enabled = true;
        initial_cfg.code_collection.git_transport_enabled = true;
        initial_cfg.code_collection.producers = vec![CodeCollectionProducerConfig {
            producer_id: "producer-a".to_string(),
            token_file,
            token_files: Vec::new(),
            scopes: vec![scope.clone()],
        }];

        let mut state = SharedState::for_test_catalog(&state_dir, &catalog_path);
        let authority_store = state.project_authority.catalog_store().unwrap().clone();
        state.code_sources = Arc::new(
            super::super::code_source::CodeSourceRuntime::open(
                &initial_cfg,
                &[],
                Some(authority_store.clone()),
                state.checkout_access.clone(),
                Arc::new(
                    bbox_indexing::code_source_locality_cutover::CodeSourceLocalityCutoverRuntimeV1::default(),
                ),
            )
            .unwrap(),
        );
        *state.config.write() = initial_cfg.clone();

        let catalog = authority_store.snapshot().unwrap();
        let assignments = BTreeMap::from([(scope.clone(), "producer-a".to_string())]);
        let projection = derive_repo_transport_grants(catalog.catalog(), &assignments);
        let RepoTransportGrantState::Granted { grant } = &projection.grants[&repo_history_id]
        else {
            panic!("fixture grant must be complete")
        };
        let p3_generation_id = format!("rhg_{}", "b".repeat(64));
        let marker = GitTransportCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:2".to_string(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: catalog.epoch(),
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            aggregate_grant_hash: Sha256ValueV1::digest(b"grants"),
            zero_prepared_history_journals: true,
            zero_prepared_provenance_journals: true,
            rows: vec![PredictedGitTransportCutoverRowV1 {
                repo_history_id: repo_history_id.clone(),
                grant_commitment: grant.commitment.clone(),
                membership_generation: 1,
                source_generation_id: "source-one".to_string(),
                p3_generation_id: p3_generation_id.clone(),
                history_parity_commitment: Sha256ValueV1::digest(b"history"),
                provenance_import_generations: BTreeMap::from([(
                    project_id.clone(),
                    "import-one".to_string(),
                )]),
                provenance_export_generations: BTreeMap::from([(
                    project_id.clone(),
                    "export-one".to_string(),
                )]),
                provenance_parity_commitments: BTreeMap::from([(
                    project_id.clone(),
                    Sha256ValueV1::digest(b"provenance"),
                )]),
                capability_baselines: Vec::new(),
            }],
            checksum_sha256: Sha256ValueV1::digest(b"checksum"),
        };
        state.git_transport_cutover =
            Arc::new(GitTransportCutoverRuntimeV1::from_marker(Some(marker)));

        let overlay = GitOverlaySelector {
            project_id: project_id.as_str().to_string(),
            code_generation: "code-one".to_string(),
            repo_history_generation: p3_generation_id,
            source: GitOverlaySourceV1::ProducerTransport {
                producer_id: "producer-a".to_string(),
                source_generation_id: "source-one".to_string(),
            },
            repo_head: "c".repeat(40),
            commit_namespace: "reload-cutover".to_string(),
            overlay_generation: 1,
        };
        let mut manifest = ManifestIndex::new();
        manifest.upsert_workspace(
            project_id.as_str(),
            WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some("collected:code-one".to_string()),
                code_source_generation: Some("code-one".to_string()),
                git_overlay: Some(overlay),
                git_overlay_managed: true,
            },
        );
        manifest
            .write_atomic(&super::super::routes::edge_sidecar_dir(&state))
            .unwrap();
        let state = Arc::new(state);

        super::super::code_source::republish_code_read_view(&state).unwrap();
        assert!(
            state
                .code_read_view
                .read()
                .git_overlays
                .contains_key(project_id.as_str()),
            "the current covered producer overlay must be visible before reload"
        );

        let mut removed_cfg = initial_cfg;
        removed_cfg.code_collection.enabled = false;
        removed_cfg.code_collection.git_transport_enabled = false;
        removed_cfg.code_collection.producers.clear();
        let _pending_transitions = install_config_reload(&state, removed_cfg)
            .expect("the validated reload must install before transition dispatch");

        assert!(
            state
                .code_sources
                .producer_auth()
                .repo_assignment_producers()
                .is_empty()
        );
        assert_eq!(
            state
                .git_transport_coverage_for_project(project_id.as_str())
                .unwrap(),
            Some(GitTransportRuntimeCoverageV1::CoveredProducerRemoved)
        );
        assert!(
            !state
                .code_read_view
                .read()
                .git_overlays
                .contains_key(project_id.as_str()),
            "reload must revoke the covered producer overlay before transitions are dispatched"
        );
    }
}
