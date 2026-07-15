use crate::config;
use crate::server::routes::*;
use crate::server::tail::{roster_stream_handler, tail_handler};
use crate::server::{BlackboxServer, RuntimeRole, SharedState};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn build_http_app(
    shared: Arc<SharedState>,
    cfg: &config::Config,
    ct: &CancellationToken,
    role: RuntimeRole,
) -> axum::Router {
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
            move || {
                Ok(match role {
                    RuntimeRole::Corpus => BlackboxServer::new_corpus(shared_for_mcp.clone()),
                    RuntimeRole::Compatibility => BlackboxServer::new(shared_for_mcp.clone()),
                })
            },
            session_manager.into(),
            server_config,
        );

    let app = axum::Router::new()
        .route("/healthz", axum::routing::get(move || service_health(role)))
        .route("/readyz", axum::routing::get(move || service_health(role)))
        .route(
            "/internal/capability",
            axum::routing::post(internal_capability_handler),
        )
        .route(
            "/internal/records",
            axum::routing::post(internal_record_ingest_handler),
        );
    let app = if role == RuntimeRole::Compatibility {
        app.route("/tail", axum::routing::get(tail_handler))
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
            // Generic orchestration control plane. These are thin HTTP adapters over
            // the `bro_*` dispatch/control tools, shared by every external driver
            // (the fleet client, future bridges). The canonical namespace is
            // `/control/*`.
            .route("/control/exec", axum::routing::post(control_exec_handler))
            .route(
                "/control/resume",
                axum::routing::post(control_resume_handler),
            )
            .route(
                "/control/closeout",
                axum::routing::post(control_closeout_handler),
            )
            .route("/control/steer", axum::routing::post(control_steer_handler))
            .route(
                "/control/interrupt",
                axum::routing::post(control_interrupt_handler),
            )
            .route(
                "/control/broadcast",
                axum::routing::post(control_broadcast_handler),
            )
            .route(
                "/control/status/{task_id}",
                axum::routing::get(control_status_handler),
            )
            .route(
                "/control/roster",
                axum::routing::get(control_roster_handler),
            )
            .route(
                "/control/roster/{task_id}",
                axum::routing::delete(control_roster_forget_handler),
            )
            .route(
                "/control/roster/stream",
                axum::routing::get(roster_stream_handler),
            )
            .route(
                "/control/dashboard",
                axum::routing::get(control_dashboard_handler),
            )
            .route(
                "/control/cancel",
                axum::routing::post(control_cancel_handler),
            )
            .route(
                "/control/team/{team_name}",
                axum::routing::get(control_team_handler),
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
                "/admin/runtime-metrics",
                axum::routing::get(admin_runtime_metrics),
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
    } else {
        app
    };
    let auth_state = shared.clone();
    app.with_state(shared)
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            auth_state,
            require_service_auth,
        ))
}

async fn require_service_auth(
    axum::extract::State(shared): axum::extract::State<Arc<SharedState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/healthz" | "/readyz")
        || shared.service_token.authorizes(request.headers())
    {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            "bearer token required",
        )
            .into_response()
    }
}

async fn service_health(role: RuntimeRole) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "ready": true,
        "service": "blackboxd",
        "role": role.as_str(),
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": env!("BLACKBOX_BUILD_ID")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app_with_state() -> (axum::Router, Arc<SharedState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let shared = Arc::new(SharedState::for_test(&root));
        let cfg = shared.config.read().clone();
        let ct = CancellationToken::new();
        (
            build_http_app(shared.clone(), &cfg, &ct, RuntimeRole::Compatibility),
            shared,
            dir,
        )
    }

    fn test_app() -> (axum::Router, tempfile::TempDir) {
        let (app, _, directory) = test_app_with_state();
        (app, directory)
    }

    fn corpus_test_app_with_state() -> (axum::Router, Arc<SharedState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let shared = Arc::new(SharedState::for_test(&root));
        let cfg = shared.config.read().clone();
        let ct = CancellationToken::new();
        (
            build_http_app(shared.clone(), &cfg, &ct, RuntimeRole::Corpus),
            shared,
            dir,
        )
    }

    fn authorized(
        builder: axum::http::request::Builder,
        state: &SharedState,
    ) -> axum::http::request::Builder {
        builder.header(
            axum::http::header::AUTHORIZATION,
            state.service_token.authorization_header(),
        )
    }

    fn capability_authorization_value(
        worker_id: &str,
        session_id: &str,
        operation: &str,
    ) -> serde_json::Value {
        serde_json::to_value(bro_protocol::CapabilityAuthorization {
            worker_id: bro_core::WorkerId::new(worker_id),
            session_id: bro_core::SessionId::new(session_id),
            task_id: bro_core::TaskId::new("task-test"),
            attempt_id: bro_core::AttemptId::new("attempt-test"),
            session_attempt_generation: 1,
            policy: bro_protocol::PolicyIdentity {
                version: 1,
                digest: "sha256:test-policy".into(),
            },
            capability_policy: bro_protocol::SessionCapabilityPolicy {
                allowed_operations: std::collections::BTreeMap::from([(
                    "corpus".into(),
                    std::collections::BTreeSet::from([operation.into()]),
                )]),
                allowed_atom_refs: std::collections::BTreeSet::new(),
            },
        })
        .unwrap()
    }

    #[tokio::test]
    async fn health_exposes_build_identity() {
        let (app, _directory) = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["service"], "blackboxd");
        assert_eq!(value["role"], "compatibility");
        assert_eq!(value["build_id"], env!("BLACKBOX_BUILD_ID"));
    }

    /// The generic control plane is reachable at the neutral `/control/*`
    /// namespace. A typo in the path table would surface here as a 404.
    #[tokio::test]
    async fn control_dashboard_resolves_to_handler() {
        let path = "/control/dashboard";
        let (app, state, _directory) = test_app_with_state();
        let resp = app
            .oneshot(
                authorized(Request::builder().uri(path), &state)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} must be mounted (route table regression)"
        );
        assert_eq!(resp.status(), StatusCode::OK, "{path} dashboard should 200");
    }

    #[tokio::test]
    async fn corpus_role_removes_legacy_control_routes_and_tools() {
        let (app, state, _directory) = corpus_test_app_with_state();
        let response = app
            .oneshot(
                authorized(Request::builder().uri("/control/dashboard"), &state)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let server = BlackboxServer::new_corpus(state);
        let tools = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(tools.iter().any(|tool| tool == "bbox_search"));
        for operational in [
            "bro_exec",
            "bro_agent_dispatch",
            "atom_invoke",
            "bbox_artifact_install",
        ] {
            assert!(
                !tools.iter().any(|tool| tool == operational),
                "corpus role exposed operational tool {operational}"
            );
        }
    }

    #[tokio::test]
    async fn internal_corpus_capability_searches_ingested_records() {
        let (app, state, _directory) = test_app_with_state();
        state
            .record_ingest
            .ingest(bro_capabilities::RecordIngestRequest {
                records: vec![bro_capabilities::RecordEnvelope {
                    record_id: "record-capability-1".into(),
                    producer: "blackopsd".into(),
                    cursor: "1".into(),
                    kind: "operation.completed".into(),
                    occurred_at: None,
                    subject: Some("operation-1".into()),
                    attributes: Default::default(),
                    payload: serde_json::json!({"summary": "capability-needle"}),
                }],
            })
            .unwrap();
        let body = serde_json::json!({
            "worker_id": "worker-1",
            "session_id": "session-1",
            "authorization": capability_authorization_value(
                "worker-1",
                "session-1",
                "search_corpus"
            ),
            "request": {
                "call_id": "call-1",
                "capability": "corpus",
                "operation": "search_corpus",
                "bounded_payload": {"query": "capability-needle", "limit": 10},
                "deadline_unix_ms": null
            }
        });
        let response = app
            .oneshot(
                authorized(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/capability")
                        .header("content-type", "application/json"),
                    &state,
                )
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: bro_protocol::CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!response.is_error);
        assert_eq!(response.result_or_error[0]["id"], "record-capability-1");
    }

    #[tokio::test]
    async fn internal_corpus_capability_rejects_missing_or_mismatched_authorization() {
        let (app, state, _directory) = test_app_with_state();
        let cases = [
            ("missing", None),
            (
                "wrong-worker",
                Some(capability_authorization_value(
                    "worker-other",
                    "session-1",
                    "search_corpus",
                )),
            ),
            (
                "wrong-session",
                Some(capability_authorization_value(
                    "worker-1",
                    "session-other",
                    "search_corpus",
                )),
            ),
            (
                "wrong-operation",
                Some(capability_authorization_value(
                    "worker-1",
                    "session-1",
                    "other",
                )),
            ),
        ];

        for (name, authorization) in cases {
            let mut body = serde_json::json!({
                "worker_id": "worker-1",
                "session_id": "session-1",
                "request": {
                    "call_id": format!("call-{name}"),
                    "capability": "corpus",
                    "operation": "search_corpus",
                    "bounded_payload": {"query": "needle", "limit": 10},
                    "deadline_unix_ms": null
                }
            });
            if let Some(authorization) = authorization {
                body["authorization"] = authorization;
            }
            let response = app
                .clone()
                .oneshot(
                    authorized(
                        Request::builder()
                            .method("POST")
                            .uri("/internal/capability")
                            .header("content-type", "application/json"),
                        &state,
                    )
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let response: bro_protocol::CapabilityResponse =
                serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                response.structured_error().unwrap().unwrap().code,
                bro_protocol::CapabilityErrorCode::Unauthorized,
                "case {name}"
            );
        }
    }

    #[tokio::test]
    async fn internal_record_ingest_deduplicates_stable_ids() {
        let (app, state, _directory) = test_app_with_state();
        let body = serde_json::json!({
            "records": [{
                "record_id": "record-http-1",
                "producer": "blackopsd",
                "cursor": "1",
                "kind": "operation.accepted",
                "occurred_at": null,
                "subject": "operation-1",
                "attributes": {},
                "payload": {"state": "record-http-needle"}
            }]
        });
        for expected in [(1, 0), (0, 1)] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/records")
                        .header("content-type", "application/json")
                        .header(
                            axum::http::header::AUTHORIZATION,
                            state.service_token.authorization_header(),
                        )
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let receipt: bro_capabilities::RecordIngestReceipt =
                serde_json::from_slice(&bytes).unwrap();
            assert_eq!((receipt.accepted, receipt.deduplicated), expected);
        }
        state.index_writer.flush_blocking().unwrap();
        let search = serde_json::json!({
            "worker_id": "worker-http-1",
            "session_id": "session-http-1",
            "authorization": capability_authorization_value(
                "worker-http-1",
                "session-http-1",
                "search_corpus"
            ),
            "request": {
                "call_id": "call-http-1",
                "capability": "corpus",
                "operation": "search_corpus",
                "bounded_payload": {"query": "record-http-needle", "limit": 10},
                "deadline_unix_ms": null
            }
        });
        let response = app
            .oneshot(
                authorized(
                    Request::builder()
                        .method("POST")
                        .uri("/internal/capability")
                        .header("content-type", "application/json"),
                    &state,
                )
                .body(Body::from(serde_json::to_vec(&search).unwrap()))
                .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let search: bro_protocol::CapabilityResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!search.is_error);
        assert_eq!(search.result_or_error.as_array().unwrap().len(), 1);
        assert_eq!(search.result_or_error[0]["id"], "record-http-1");
    }

    /// Every control verb is mounted under `/control/*`. GET on the POST-only
    /// verbs yields 405 (not 404) — proving the path exists with a handler.
    #[tokio::test]
    async fn control_verbs_mounted() {
        let verbs = [
            "exec",
            "resume",
            "steer",
            "interrupt",
            "broadcast",
            "cancel",
        ];
        let (app, state, _directory) = test_app_with_state();
        for verb in verbs {
            let path = format!("/control/{verb}");
            let resp = app
                .clone()
                .oneshot(
                    authorized(Request::builder().uri(&path), &state)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} must be mounted"
            );
        }
    }

    /// The `/control/closeout` endpoint is mounted (Phase 3a, design
    /// fleet-tui/closeout-command.md §4.1). It is a daemon-side-only
    /// endpoint on the neutral `/control/*` namespace; the cockpit calls
    /// it directly. A GET on the POST-only route yields 405 (not 404) —
    /// proving the path exists with a handler.
    #[tokio::test]
    async fn control_closeout_is_mounted() {
        let (app, state, _directory) = test_app_with_state();
        let resp = app
            .oneshot(
                authorized(Request::builder().uri("/control/closeout"), &state)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "/control/closeout must be mounted (route table regression)"
        );
    }

    #[tokio::test]
    async fn control_roster_stream_is_mounted_and_yields_deltas() {
        use futures::StreamExt;
        use tokio::time::{Duration, timeout};

        let (app, state, _directory) = test_app_with_state();
        let resp = app
            .oneshot(
                authorized(Request::builder().uri("/control/roster/stream"), &state)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );

        let mut body = resp.into_body().into_data_stream();
        state.roster_events().emit_removed("task-sse-mounted");

        let chunk = timeout(Duration::from_secs(1), body.next())
            .await
            .expect("stream should yield a roster delta")
            .expect("stream should remain open")
            .expect("body chunk should be readable");
        let text = std::str::from_utf8(&chunk).expect("SSE chunk must be UTF-8");
        assert!(text.contains("event: removed"), "chunk: {text}");
        assert!(text.contains("task-sse-mounted"), "chunk: {text}");
    }

    #[tokio::test]
    async fn every_non_health_route_requires_the_service_token() {
        let (app, state, _directory) = test_app_with_state();
        for path in ["/control/dashboard", "/internal/records", "/mcp"] {
            let denied = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED, "{path}");

            let admitted = app
                .clone()
                .oneshot(
                    authorized(
                        Request::post(path).header("content-type", "application/json"),
                        &state,
                    )
                    .body(Body::from("{}"))
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(admitted.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }
}
