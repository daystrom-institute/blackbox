//! Native transcript source ingress. Authentication is shared with connector
//! producers; each operation additionally proves the native profile and scope.
use super::{SharedState, producer_auth::ConnectorGrant};
use anyhow::{Context, Result, anyhow};
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_transcript_source::*;
use bbox_transcript_source_store::TranscriptSourceStore;
use std::sync::Arc;

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route("/internal/transcript-source/v1/onboard", post(onboard))
        .route("/internal/transcript-source/v1/status", post(status))
        .route("/internal/transcript-source/v1/contact", post(contact))
        .route("/internal/transcript-source/v1/missing", post(missing))
        .route("/internal/transcript-source/v1/publish", post(publish))
        .route(
            "/internal/transcript-source/v1/chunks/{source}/{stream}/{hash}",
            put(chunk).layer(DefaultBodyLimit::max(CHUNK_BYTES)),
        )
        .layer(DefaultBodyLimit::max(2 * CHUNK_BYTES))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_transcript_source_request,
        ))
}
fn authorize(
    state: &SharedState,
    producer: &ConnectorGrant,
    scope: &ConnectorScope,
    pending: bool,
) -> Result<()> {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    anyhow::ensure!(
        connectors
            .grants()
            .iter()
            .any(|grant| grant.producer_id == producer.producer_id && &grant.scope == scope),
        "scope_forbidden"
    );
    anyhow::ensure!(
        connectors.profile_for(scope.connector_source_id())
            == Some(crate::config::ConnectorProfile::Transcript),
        "scope_wrong_lane"
    );
    anyhow::ensure!(
        pending || !connectors.is_pending_onboard(scope),
        "scope_pending_onboarding"
    );
    Ok(())
}
fn store_root(state: &SharedState) -> std::path::PathBuf {
    state
        .config
        .read()
        .paths
        .state_dir
        .join("transcript-sources")
}
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| anyhow!("transcript source worker failed: {error}"))?
}
fn reply<T: serde::Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "native transcript source request failed");
            let text = error.to_string();
            let (status, code) = if let Some(catalog_error) =
                error
                    .downcast_ref::<bbox_indexing::project_catalog_store::ProjectCatalogStoreError>(
                    ) {
                (StatusCode::CONFLICT, catalog_error.code())
            } else if text.contains("scope_forbidden") || text.contains("scope_wrong_lane") {
                (StatusCode::FORBIDDEN, text.as_str())
            } else if text.contains("scope_pending_onboarding") {
                (StatusCode::CONFLICT, "scope_pending_onboarding")
            } else if text.contains("transcript_scan_conflict") {
                (StatusCode::CONFLICT, "transcript_scan_conflict")
            } else if text.contains("transcript_generation_conflict") {
                (StatusCode::CONFLICT, "transcript_generation_conflict")
            } else if error.downcast_ref::<std::io::Error>().is_some()
                || error.downcast_ref::<serde_json::Error>().is_some()
            {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transcript_source_unavailable",
                )
            } else if [
                "invalid",
                "mismatch",
                "incomplete",
                "exceeds",
                "unsupported",
                "missing transcript chunk",
            ]
            .iter()
            .any(|marker| text.contains(marker))
            {
                (StatusCode::BAD_REQUEST, "invalid_transcript_snapshot")
            } else {
                tracing::warn!(error = %error, "native transcript source request failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transcript_source_unavailable",
                )
            };
            (status, Json(serde_json::json!({"code": code, "message": "The transcript source operation failed; use the code to check authority, current generation, or source health."}))).into_response()
        }
    }
}
async fn contact(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<ContactRequest>,
) -> Response {
    if let Err(error) = authorize(&state, &grant, &request.scope, false) {
        return reply::<()>(Err(error));
    }
    let root = store_root(&state);
    reply(
        blocking(move || {
            TranscriptSourceStore::open(root)?
                .record_contact(&request, &chrono::Utc::now().to_rfc3339())
        })
        .await,
    )
}
async fn status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<StreamQuery>,
) -> Response {
    if let Err(error) = authorize(&state, &grant, &request.scope, false) {
        return reply::<()>(Err(error));
    }
    let root = store_root(&state);
    reply(
        blocking(move || {
            TranscriptSourceStore::open(root)?.status(&request.scope, &request.stream_id)
        })
        .await,
    )
}
async fn missing(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(snapshot): Json<StreamSnapshot>,
) -> Response {
    if let Err(error) = authorize(&state, &grant, &snapshot.scope, false) {
        return reply::<()>(Err(error));
    }
    let root = store_root(&state);
    reply(blocking(move || TranscriptSourceStore::open(root)?.missing_chunks(&snapshot)).await)
}
async fn chunk(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path((source, stream, hash)): Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Response {
    let connectors = state.code_sources.producer_auth().connectors().clone();
    let scope = connectors
        .grants()
        .iter()
        .find(|entry| {
            entry.producer_id == grant.producer_id
                && entry.scope.connector_source_id().as_str() == source
        })
        .map(|entry| entry.scope.clone());
    let Some(scope) = scope else {
        return reply::<()>(Err(anyhow!("scope_forbidden")));
    };
    if let Err(error) = authorize(&state, &grant, &scope, false) {
        return reply::<()>(Err(error));
    }
    let root = store_root(&state);
    reply(
        blocking(move || {
            TranscriptSourceStore::open(root)?.install_chunk(&scope, &stream, &hash, &body)
        })
        .await,
    )
}
async fn publish(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<PublishRequest>,
) -> Response {
    if let Err(error) = authorize(&state, &grant, &request.snapshot.scope, false) {
        return reply::<()>(Err(error));
    }
    let root = store_root(&state);
    let result = blocking(move || {
        TranscriptSourceStore::open(root)?.publish(&request, &chrono::Utc::now().to_rfc3339())
    })
    .await;
    if result.is_ok() {
        let _ = state
            .index_writer
            .request_reindex_pass_accepting_empty(false, false, Vec::new());
    }
    reply(result)
}
async fn onboard(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<OnboardRequest>,
) -> Response {
    if let Err(error) = authorize(&state, &grant, &request.scope, true) {
        return reply::<()>(Err(error));
    }
    let connectors = state.code_sources.producer_auth().connectors().clone();
    let Some(catalog) = state.project_authority.catalog_store().cloned() else {
        return reply::<()>(Err(anyhow!("catalog unavailable")));
    };
    let grants = connectors.grants().to_vec();
    let mut result = blocking(move || {
        let onboard = bbox_indexing::project_catalog_admin::ConnectorOnboardRequest {
            producer_id: grant.producer_id, scope: request.scope.clone(),
            probed_connector_kind: request.scope.connector_kind().as_str().into(),
            probed_remote_authority: request.remote_authority.clone(),
            probed_remote_root_id: Some(request.remote_authority),
            probed_remote_display_name: Some(request.display_name.clone()),
            display_name: request.display_name, observed_at: chrono::Utc::now().to_rfc3339(),
        };
        let epoch = catalog.snapshot().context("reading native transcript onboarding catalog")?.epoch();
        let receipt = bbox_indexing::project_catalog_admin::connector_onboard(&catalog, epoch, &grants, &onboard).context("admitting native transcript connector scope")?;
        Ok(serde_json::json!({"project_id": receipt.project_id.as_str(), "created": receipt.created, "epoch": receipt.catalog_epoch, "catalog_admitted": true, "reload_pending": false}))
    }).await;
    if result.is_ok() {
        let config = state.config.read().clone();
        let records = state.records_provider.records_snapshot().records;
        if let Err(error) = state.code_sources.reload(&config, &records) {
            tracing::warn!(%error, "native transcript catalog admission succeeded but grant reload is pending");
            if let Ok(receipt) = result.as_mut() {
                receipt["catalog_admitted"] = serde_json::Value::Bool(true);
                receipt["reload_pending"] = serde_json::Value::Bool(true);
                receipt["next_step"] = serde_json::Value::String("Retry onboard idempotently to reload publication authority; the catalog project already exists.".into());
            }
            return reply(result);
        }
        state.nudge_edge_index_rebuild();
    }
    reply(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{ConnectorKind, ConnectorSourceId};

    fn scope() -> ConnectorScope {
        ConnectorScope::try_new("csrc_0123456789abcdef", "native_transcript").unwrap()
    }
    fn pending_state(root: &std::path::Path) -> (Arc<SharedState>, std::path::PathBuf) {
        let catalog_path = root.join("catalog/projects.json");
        std::fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(&catalog_path)
            .unwrap();
        let mut state = SharedState::for_test_catalog(root, &catalog_path);
        let token_path = root.join("producer.token");
        std::fs::write(&token_path, "a".repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = crate::config::SourceConnectorsConfig {
            retained_conversations: Vec::new(),
            enabled: true,
            producers: vec![crate::config::ConnectorProducerConfig {
                producer_id: "fixture-producer".into(),
                token_file: token_path.clone(),
                token_files: Vec::new(),
                scopes: vec![crate::config::ConnectorScopeGrant {
                    connector_source_id: ConnectorSourceId::parse("csrc_0123456789abcdef").unwrap(),
                    connector_kind: ConnectorKind::parse("native_transcript").unwrap(),
                    remote_authority: "fixture-installation".into(),
                    profile: crate::config::ConnectorProfile::Transcript,
                }],
            }],
        };
        state.config.write().source_connectors = config;
        state.config.write().paths.state_dir = root.join("native-runtime");
        let runtime_config = state.config.read().clone();
        state.code_sources = Arc::new(
            super::super::code_source::CodeSourceRuntime::open(
                &runtime_config,
                &[],
                state.project_authority.catalog_store().cloned(),
                state.checkout_access.clone(),
                state.code_source_locality_cutover.clone(),
            )
            .unwrap(),
        );
        (Arc::new(state), token_path)
    }

    #[tokio::test]
    async fn native_collector_http_publication_indexes_without_producer_files_and_refuses_revoked_scope()
     {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, token_file) = pending_state(&root);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = router(state.clone()).with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let source_root = root.join("producer");
        std::fs::create_dir_all(&source_root).unwrap();
        let source_file = source_root.join("session-fixture.jsonl");
        let original = serde_json::json!({"type":"user", "sessionId":"session-fixture", "message":{"content":"native landed marker"}}).to_string() + "\n";
        std::fs::write(&source_file, &original).unwrap();
        let config = bbox_transcript_collector::Config {
            corpus_url: format!("http://{address}"),
            token_file,
            scope: scope(),
            remote_authority: "fixture-installation".into(),
            display_name: "Native fixture".into(),
            roots: vec![bbox_transcript_collector::RootConfig {
                source: NativeSource::Claude,
                account: "fixture".into(),
                path: source_root,
            }],
        };
        let client = bbox_transcript_collector::Client::new(&config).unwrap();
        let pending: Result<StreamStatus> = client
            .post(
                "status",
                &StreamQuery {
                    scope: scope(),
                    stream_id: sha256(b"unknown"),
                },
            )
            .await;
        let pending = pending.unwrap_err();
        assert_eq!(
            pending
                .downcast_ref::<bbox_transcript_collector::TransportError>()
                .unwrap()
                .code,
            "scope_pending_onboarding"
        );
        let admission = client.onboard(&config).await.unwrap();
        assert_eq!(admission["catalog_admitted"], true);
        assert_eq!(admission["reload_pending"], false);
        let cycle = bbox_transcript_collector::publish_cycle(&config, &client)
            .await
            .unwrap();
        assert_eq!(cycle.published, 1);
        assert_eq!(cycle.failed, 0);
        assert_eq!(
            bbox_transcript_collector::publish_cycle(&config, &client)
                .await
                .unwrap()
                .unchanged,
            1
        );
        std::fs::remove_file(&source_file).unwrap();

        // Build a separate consumer index solely from the landed source. The
        // fixture daemon writer has its own index and never shares this writer.
        let mut index = crate::index::TranscriptIndex::open_or_create_with_records(
            &root.join("verify-index"),
            Vec::new(),
            None,
            root.join("verify-projects.json"),
            root.join("verify-kb.json"),
            root.join("verify-threads.json"),
            Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        index.set_native_sources(store_root(&state), vec![scope()]);
        index.build_index_with_project_access(false, &[]).unwrap();
        index.reader_reload_for_test();
        let params =
            serde_json::from_value(serde_json::json!({"session_id":"session-fixture"})).unwrap();
        let body = index.messages(&params).unwrap();
        assert!(body.contains("native landed marker"), "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body["source_freshness"]["streams"][0]["index_matches_published"],
            true
        );
        assert_eq!(
            body["source_freshness"]["producers"][0]["scan_in_progress"],
            false
        );
        assert_eq!(
            body["source_freshness"]["producers"][0]["last_completed_scan"]["failed"],
            0
        );
        assert!(body["source_freshness"]["producers"][0]["last_contact_at"].is_string());
        let locator = body["messages"][0]["locator"].as_str().unwrap();
        assert!(locator.starts_with("native:csrc_"));
        assert!(
            index
                .context(&crate::index::ContextParams {
                    file_path: locator.into(),
                    byte_offset: 0,
                    context_lines: None
                })
                .unwrap()
                .contains("native landed marker")
        );

        // A newer admitted source does not make the existing index current.
        let store = TranscriptSourceStore::open(store_root(&state)).unwrap();
        let current = store.snapshots(&scope()).unwrap().pop().unwrap();
        let bytes = b"{\"type\":\"user\",\"sessionId\":\"session-fixture\",\"message\":{\"content\":\"new generation\"}}\n";
        let mut snapshot = current.snapshot;
        snapshot.byte_length = bytes.len() as u64;
        snapshot.content_sha256 = sha256(bytes);
        snapshot.chunks = vec![ChunkRef {
            sha256: sha256(bytes),
            size: bytes.len() as u64,
        }];
        store
            .install_chunk(&scope(), &snapshot.stream_id, &sha256(bytes), bytes)
            .unwrap();
        store
            .publish(
                &PublishRequest {
                    snapshot,
                    expected_generation: Some(current.generation),
                },
                "later",
            )
            .unwrap();
        let stale: serde_json::Value =
            serde_json::from_str(&index.messages(&params).unwrap()).unwrap();
        assert_eq!(
            stale["source_freshness"]["streams"][0]["index_matches_published"],
            false
        );
        assert!(
            stale["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("native landed marker")
        );

        let index_config = index.reindex_config();
        let meta = crate::index::passes::load_meta(&index_config.meta_path).unwrap();
        assert!(
            crate::index::passes::native_purge_exempt_paths(&index_config, &meta).contains(locator)
        );
        index.build_index_with_project_access(false, &[]).unwrap();
        index.reader_reload_for_test();
        let replaced: serde_json::Value =
            serde_json::from_str(&index.messages(&params).unwrap()).unwrap();
        assert_eq!(replaced["total_matching_messages"], 1);
        assert_eq!(
            replaced["source_freshness"]["streams"][0]["index_matches_published"],
            true
        );
        assert_eq!(replaced["messages"][0]["content"], "new generation");
        let meta = crate::index::passes::load_meta(&index_config.meta_path).unwrap();
        assert!(!meta.contains_key(locator));

        // An unavailable enrolled landing store is an indexing error, not an
        // empty source set that authorizes dropping existing projections.
        let landed_root = store_root(&state);
        let saved_root = root.join("saved-native-sources");
        std::fs::rename(&landed_root, &saved_root).unwrap();
        std::fs::write(&landed_root, b"not a directory").unwrap();
        assert!(index.build_index_with_project_access(true, &[]).is_err());
        index.reader_reload_for_test();
        assert!(index.messages(&params).unwrap().contains("new generation"));
        std::fs::remove_file(&landed_root).unwrap();
        std::fs::rename(&saved_root, &landed_root).unwrap();

        // Revocation removes the source from both wire authority and the
        // index enrollment. Old source bytes cannot keep reappearing in scans.
        state.config.write().source_connectors.producers.clear();
        state.config.write().source_connectors.enabled = false;
        let config_state = state.config.read().clone();
        let records = state.records_provider.records_snapshot().records;
        state.code_sources.reload(&config_state, &records).unwrap();
        let refused: Result<StreamStatus> = client
            .post(
                "status",
                &StreamQuery {
                    scope: scope(),
                    stream_id: sha256(b"unknown"),
                },
            )
            .await;
        assert!(refused.is_err());
        index.set_native_sources(store_root(&state), Vec::new());
        index.build_index_with_project_access(false, &[]).unwrap();
        index.reader_reload_for_test();
        assert!(index.messages(&params).is_err());
        server.abort();
    }
}
