use crate::config;
use crate::server::routes::*;
use crate::server::tail::{roster_stream_handler, tail_handler};
use crate::server::{BlackboxServer, SharedState};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

async fn health_probe() -> axum::http::StatusCode {
    axum::http::StatusCode::OK
}

pub(super) fn build_http_app(
    shared: Arc<SharedState>,
    cfg: &config::Config,
    ct: &CancellationToken,
) -> axum::Router {
    let server_config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(cfg.daemon.mcp_allowed_hosts.clone())
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

    axum::Router::new()
        // The HTTP router is constructed only after durable state has opened,
        // so a reachable route proves startup completed as well as liveness.
        .route("/healthz", axum::routing::get(health_probe))
        .route("/readyz", axum::routing::get(health_probe))
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
        .merge(super::code_source::router(shared.clone()))
        .merge(super::git_source::router(shared.clone()))
        .merge(super::knowledge_source::router(shared.clone()))
        .with_state(shared)
        .nest_service("/mcp", mcp_service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app_with_state() -> (axum::Router, Arc<SharedState>) {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let cfg = shared.config.read().clone();
        let ct = CancellationToken::new();
        (build_http_app(shared.clone(), &cfg, &ct), shared)
    }

    fn test_app() -> axum::Router {
        test_app_with_state().0
    }

    #[tokio::test]
    async fn unauthenticated_health_and_readiness_routes_are_live() {
        for path in ["/healthz", "/readyz"] {
            let response = test_app()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn configured_external_mcp_host_is_admitted() {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let mut cfg = shared.config.read().clone();
        cfg.daemon.mcp_allowed_hosts = vec!["corpus.internal:7264".to_string()];
        let app = build_http_app(shared, &cfg, &CancellationToken::new());
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "external-host-test", "version": "1"}
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp?surface=interactive")
                    .header("accept", "application/json, text/event-stream")
                    .header("content-type", "application/json")
                    .header("host", "corpus.internal:7264")
                    .body(Body::from(initialize.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The generic control plane is reachable at the neutral `/control/*`
    /// namespace. A typo in the path table would surface here as a 404.
    #[tokio::test]
    async fn control_dashboard_resolves_to_handler() {
        let path = "/control/dashboard";
        let resp = test_app()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{path} must be mounted (route table regression)"
        );
        assert_eq!(resp.status(), StatusCode::OK, "{path} dashboard should 200");
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
        for verb in verbs {
            let path = format!("/control/{verb}");
            let resp = test_app()
                .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
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
        let resp = test_app()
            .oneshot(
                Request::builder()
                    .uri("/control/closeout")
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

        let (app, state) = test_app_with_state();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/control/roster/stream")
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
    async fn operator_blame_headers_bind_only_the_mcp_blame_locality_session() {
        use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
        use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
        use bbox_corpus_core::blame_transport::{
            OPERATOR_BLAME_REPO_ID_HEADER, OPERATOR_BLAME_ROOT_RELPATH_HEADER,
            OPERATOR_BLAME_WORKSPACE_ID_HEADER,
        };
        use bbox_corpus_core::identity::PublishedScope;
        use bro_rpc::ServiceToken;
        use std::collections::BTreeMap;

        let (app, state) = test_app_with_state();
        let token = "b".repeat(64);
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    ServiceToken::parse(token.clone()).unwrap(),
                    ProducerGrant {
                        producer_id: "operator".into(),
                        projects: BTreeMap::from([(scope, "project-bound".into())]),
                    },
                )],
            )));
        let before = state.checkout_access.health().sequence;
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "bro-cli", "version": "test"}
            }
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(ACCEPT, "application/json, text/event-stream")
                    .header(CONTENT_TYPE, "application/json")
                    .header("Host", "127.0.0.1:7264")
                    .header("Mcp-Protocol-Version", "2025-03-26")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(OPERATOR_BLAME_REPO_ID_HEADER, "repo")
                    .header(OPERATOR_BLAME_ROOT_RELPATH_HEADER, ".")
                    .header(
                        OPERATOR_BLAME_WORKSPACE_ID_HEADER,
                        "0123456789abcdef0123456789abcdef",
                    )
                    .body(Body::from(initialize.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 128 * 1024)
                .await
                .unwrap();
            panic!(
                "operator blame MCP initialize failed with {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "bbox_blame",
                "arguments": {
                    "file": "src/lib.rs",
                    "line": 7,
                    "_blame_locality": {"phase": "plan"}
                }
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(ACCEPT, "application/json, text/event-stream")
                    .header(CONTENT_TYPE, "application/json")
                    .header("Host", "127.0.0.1:7264")
                    .header("Mcp-Protocol-Version", "2025-03-26")
                    .header("Mcp-Session-Id", session_id)
                    .body(Body::from(call.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let response: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| {
            let data = body
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&data).unwrap_or_else(|error| {
                panic!("invalid MCP response {error}: {body}");
            })
        });
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        let plan: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(plan["status"], "blame_locality_plan");
        assert_eq!(plan["plan"]["project_id"], "project-bound");
        assert_eq!(
            plan["plan"]["workspace_id"],
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(state.checkout_access.health().sequence, before);
    }

    #[tokio::test]
    async fn operator_provenance_headers_bind_only_the_mcp_export_planning_session() {
        use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
        use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
        use bbox_corpus_core::identity::PublishedScope;
        use bbox_provenance::{
            OPERATOR_PROVENANCE_REPO_ID_HEADER, OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
        };
        use bro_rpc::ServiceToken;
        use std::collections::BTreeMap;

        let (app, state) = test_app_with_state();
        let project_root = tempfile::tempdir().unwrap();
        let project = state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(project_root.path())
            .unwrap();
        let token = "c".repeat(64);
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test(
                true,
                false,
                vec![(
                    ServiceToken::parse(token.clone()).unwrap(),
                    ProducerGrant {
                        producer_id: "operator".into(),
                        projects: BTreeMap::from([(scope.clone(), project.project_id.clone())]),
                    },
                )],
            )));
        let before = state.checkout_access.health().sequence;
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "bro-cli", "version": "test"}
            }
        });
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(ACCEPT, "application/json, text/event-stream")
                    .header(CONTENT_TYPE, "application/json")
                    .header("Host", "127.0.0.1:7264")
                    .header("Mcp-Protocol-Version", "2025-03-26")
                    .header(AUTHORIZATION, format!("Bearer {token}"))
                    .header(OPERATOR_PROVENANCE_REPO_ID_HEADER, scope.repo_id())
                    .header(
                        OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
                        scope.bbox_root_relpath(),
                    )
                    .body(Body::from(initialize.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "bbox_provenance_export_plan",
                "arguments": {}
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(ACCEPT, "application/json, text/event-stream")
                    .header(CONTENT_TYPE, "application/json")
                    .header("Host", "127.0.0.1:7264")
                    .header("Mcp-Protocol-Version", "2025-03-26")
                    .header("Mcp-Session-Id", session_id)
                    .body(Body::from(call.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let response: serde_json::Value = serde_json::from_str(&body).unwrap_or_else(|_| {
            let data = body
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::from_str(&data).unwrap_or_else(|error| {
                panic!("invalid MCP response {error}: {body}");
            })
        });
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        let page: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(page["project_id"], project.project_id);
        assert_eq!(page["scope"]["repo_id"], scope.repo_id());
        assert_eq!(state.checkout_access.health().sequence, before);
    }
}
