use crate::config;
use crate::server::control::*;
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
        .with_legacy_session_mode(true);

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

    // Operator admin plane. The loopback-or-service-bearer gate rides as a
    // `route_layer` over exactly these routes (see `super::admin_auth`); it
    // must never cover `/mcp`, `/internal/*`, `/control/*`, `/healthz`, or
    // `/readyz`.
    let admin_routes = axum::Router::new()
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
            "/admin/orchestration-activity",
            axum::routing::get(admin_orchestration_activity),
        )
        .route(
            "/admin/drain",
            axum::routing::get(admin_drain_status).post(admin_drain_set),
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
            "/admin/brofile/upsert",
            axum::routing::post(admin_brofile_upsert),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            super::admin_auth::AdminAuth::from_config(&cfg.daemon),
            super::admin_auth::authenticate_admin_request,
        ));

    axum::Router::new()
        // The HTTP router is constructed only after durable state has opened,
        // so a reachable route proves startup completed as well as liveness.
        .route("/healthz", axum::routing::get(health_probe))
        .route("/readyz", axum::routing::get(health_probe))
        .route("/tail", axum::routing::get(tail_handler))
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
        .merge(admin_routes)
        .merge(super::code_source::router(shared.clone()))
        .merge(super::file_source::router(shared.clone()))
        .merge(super::conversation_source::router(shared.clone()))
        .merge(super::transcript_source::router(shared.clone()))
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
    async fn retired_application_routes_are_absent_while_control_remains() {
        let app = test_app();
        for path in [
            "/orchestrate",
            "/orchestrate/stream",
            "/orchestrate/by-id",
            "/webhook/example",
            "/webhook/example/replay",
            "/admin/workflow/install",
            "/admin/cron/install",
            "/admin/poller/install",
            "/admin/webhook/install",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/exec")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
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

    async fn post_mcp(
        headers: &[(&str, &str)],
        body: serde_json::Value,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/mcp?surface=interactive")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("host", "127.0.0.1");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = test_app()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            axum::body::to_bytes(response.into_body(), 1 << 20),
        )
        .await
        .map(|bytes| String::from_utf8(bytes.unwrap().to_vec()).unwrap())
        .unwrap_or_default();
        (status, headers, body)
    }

    fn modern_meta() -> serde_json::Value {
        serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "modern-test", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    /// One sessionless 2026-07-28 request against an app whose daemon serves
    /// the modern lifecycle, returning the status and the JSON-RPC reply.
    async fn modern_request(
        uri: &str,
        method: &str,
        params: serde_json::Value,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        sessionless_request(true, uri, method, params, extra_headers).await
    }

    async fn sessionless_request(
        modern_lifecycle: bool,
        uri: &str,
        method: &str,
        mut params: serde_json::Value,
        extra_headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let (app, state) = test_app_with_state();
        state
            .mcp_modern_lifecycle
            .store(modern_lifecycle, std::sync::atomic::Ordering::Relaxed);
        params["_meta"] = modern_meta();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        });
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("host", "127.0.0.1")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", method);
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        let response = app
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert!(!response.headers().contains_key("mcp-session-id"));
        let text = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            axum::body::to_bytes(response.into_body(), 4 << 20),
        )
        .await
        .map(|bytes| String::from_utf8(bytes.unwrap().to_vec()).unwrap())
        .unwrap_or_default();
        let reply = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: ").or(Some(line)))
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .unwrap_or_else(|| panic!("no JSON-RPC reply in {text:?}"));
        (status, reply)
    }

    fn tool_names(reply: &serde_json::Value) -> Vec<&str> {
        reply["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tool list in {reply}"))
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect()
    }

    /// With the modern lifecycle off, a sessionless request is refused as an
    /// unsupported revision, and the refusal says nothing about the tool it
    /// named: an operator tool, a visible tool and a tool that does not exist
    /// answer alike, with or without parameter headers.
    #[tokio::test]
    async fn a_refused_sessionless_call_does_not_distinguish_tools() {
        let call = |tool: &'static str, headers: Vec<(&'static str, &'static str)>| async move {
            let mut headers = headers;
            headers.push(("mcp-name", tool));
            sessionless_request(
                false,
                "/mcp?surface=readonly",
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": { "format": "summary" } }),
                &headers,
            )
            .await
        };
        let (status, missing) = call("no_such_tool", vec![]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");
        assert_eq!(missing["error"]["code"], -32022, "{missing}");
        for tool in ["bbox_doctor", "bbox_thread_list", "no_such_tool"] {
            for headers in [vec![], vec![("mcp-param-format", "bogus")]] {
                let (tool_status, reply) = call(tool, headers.clone()).await;
                assert_eq!(tool_status, status, "{tool} {headers:?}: {reply}");
                assert_eq!(reply, missing, "{tool} {headers:?}");
            }
        }
    }

    /// With the modern lifecycle on, discovery names the sessionless revision
    /// beside the handshake ones and never the handshake revision the legacy
    /// answer must not move to.
    #[tokio::test]
    async fn discover_advertises_the_sessionless_revision_when_enabled() {
        let (status, reply) = modern_request(
            "/mcp?surface=readonly",
            "server/discover",
            serde_json::json!({}),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(
            reply["result"]["supportedVersions"],
            serde_json::json!(["2024-11-05", "2025-03-26", "2025-06-18", "2026-07-28"])
        );
        assert_eq!(reply["result"]["cacheScope"], "private");
        assert!(reply["result"]["capabilities"]["tools"].is_object());
    }

    /// A sessionless request is served under the scope its own URL names:
    /// each surface lists and calls exactly its own tools.
    #[tokio::test]
    async fn sessionless_requests_are_served_under_their_own_surface() {
        let (status, readonly) = modern_request(
            "/mcp?surface=readonly",
            "tools/list",
            serde_json::json!({}),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{readonly}");
        let readonly_tools = tool_names(&readonly);
        assert!(readonly_tools.contains(&"bbox_hybrid_search"));
        assert!(!readonly_tools.contains(&"bbox_learn"));
        assert!(!readonly_tools.contains(&"bro_exec"));
        let mut sorted = readonly_tools.clone();
        sorted.sort_unstable();
        assert_eq!(readonly_tools, sorted, "the listing is in name order");
        assert_eq!(readonly["result"]["ttlMs"], 300_000);
        assert_eq!(readonly["result"]["cacheScope"], "private");

        let (_, ops) =
            modern_request("/mcp?surface=ops", "tools/list", serde_json::json!({}), &[]).await;
        let ops_tools = tool_names(&ops);
        assert!(ops_tools.contains(&"bro_exec") && ops_tools.contains(&"bbox_learn"));
        assert!(ops_tools.len() > readonly_tools.len());

        let call = |surface: &'static str, tool: &'static str| async move {
            modern_request(
                surface,
                "tools/call",
                serde_json::json!({ "name": tool, "arguments": {} }),
                &[("mcp-name", tool)],
            )
            .await
        };
        let (status, allowed) = call("/mcp?surface=readonly", "bbox_thread_list").await;
        assert_eq!(status, StatusCode::OK, "{allowed}");
        assert_eq!(allowed["result"]["isError"], false);
        assert_eq!(
            allowed["result"]["resultType"], "complete",
            "a sessionless revision requires the result type: {allowed}"
        );
        let (_, refused) = call("/mcp?surface=readonly", "bbox_learn").await;
        assert_eq!(refused["error"]["code"], -32601, "{refused}");
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not available on surface 'readonly'"),
            "{refused}"
        );
    }

    /// An unknown surface is refused at discovery and on every method, so a
    /// misconfigured client never receives a partial catalog.
    #[tokio::test]
    async fn an_unknown_surface_is_refused_on_every_sessionless_method() {
        for (method, params, headers) in [
            ("server/discover", serde_json::json!({}), vec![]),
            ("tools/list", serde_json::json!({}), vec![]),
            ("resources/list", serde_json::json!({}), vec![]),
            ("prompts/list", serde_json::json!({}), vec![]),
            (
                "resources/read",
                serde_json::json!({ "uri": "blackbox://skills/onboard-project/SKILL.md" }),
                vec![("mcp-name", "blackbox://skills/onboard-project/SKILL.md")],
            ),
            (
                "tools/call",
                serde_json::json!({ "name": "bbox_thread_list", "arguments": {} }),
                vec![("mcp-name", "bbox_thread_list")],
            ),
        ] {
            let (_, reply) = modern_request("/mcp?surface=missing", method, params, &headers).await;
            assert_eq!(reply["error"]["code"], -32600, "{method}: {reply}");
            assert!(
                reply["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("tool surface denied"),
                "{method}: {reply}"
            );
            assert!(reply.get("result").is_none(), "{method}: {reply}");
        }
    }

    /// Enabling the modern lifecycle leaves the handshake answer where it
    /// was: a client naming a newer handshake revision still gets 2025-06-18.
    #[tokio::test]
    async fn the_handshake_answer_is_unchanged_when_the_modern_lifecycle_is_enabled() {
        let (app, state) = test_app_with_state();
        state
            .mcp_modern_lifecycle
            .store(true, std::sync::atomic::Ordering::Relaxed);
        for requested in ["2026-07-28", "2025-11-25", "2025-06-18"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp?surface=interactive")
                        .header("accept", "application/json, text/event-stream")
                        .header("content-type", "application/json")
                        .header("host", "127.0.0.1")
                        .body(Body::from(initialize_body(requested).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{requested}");
            assert!(response.headers().contains_key("mcp-session-id"));
            let body = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                axum::body::to_bytes(response.into_body(), 1 << 20),
            )
            .await
            .map(|bytes| String::from_utf8(bytes.unwrap().to_vec()).unwrap())
            .unwrap_or_default();
            assert!(
                body.contains(r#""protocolVersion":"2025-06-18""#),
                "{requested}: {body}"
            );
        }
    }

    fn initialize_body(protocol_version: &str) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "wire-test", "version": "1"}
            }
        })
    }

    /// Scope is pinned at `initialize`, so a client naming a newer revision is
    /// answered with the newest handshake revision the wire head serves.
    #[tokio::test]
    async fn initialize_answers_with_a_handshake_revision() {
        for (requested, negotiated) in [
            ("2026-07-28", "2025-06-18"),
            ("2025-11-25", "2025-06-18"),
            ("2025-06-18", "2025-06-18"),
            ("2025-03-26", "2025-03-26"),
        ] {
            let (status, headers, body) = post_mcp(&[], initialize_body(requested)).await;
            assert_eq!(status, StatusCode::OK, "{requested}");
            assert!(headers.contains_key("mcp-session-id"), "{requested}");
            assert!(
                body.contains(&format!(r#""protocolVersion":"{negotiated}""#)),
                "{requested}: {body}"
            );
            assert!(!body.contains("resultType"), "{requested}: {body}");
        }
    }

    /// A sessionless request would bypass the surface, project and workspace
    /// binding pinned at `initialize`, so the stateless lifecycle is refused
    /// before any handler runs.
    #[tokio::test]
    async fn stateless_lifecycle_requests_are_refused() {
        let call = serde_json::json!({
            "name": "bbox_thread_list",
            "arguments": {},
            "_meta": modern_meta(),
        });
        for (method, extra_header, params) in [
            (
                "server/discover",
                None,
                serde_json::json!({"_meta": modern_meta()}),
            ),
            (
                "tools/list",
                None,
                serde_json::json!({"_meta": modern_meta()}),
            ),
            ("tools/call", Some(("mcp-name", "bbox_thread_list")), call),
        ] {
            let mut headers = vec![
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", method),
            ];
            headers.extend(extra_header);
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params,
            });
            let (status, headers, body) = post_mcp(&headers, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method}: {body}");
            assert!(!headers.contains_key("mcp-session-id"), "{method}");
            let reply: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(reply["error"]["code"], -32022, "{method}: {body}");
            assert!(reply.get("result").is_none(), "{method}: {body}");
        }

        let (status, _, body) = post_mcp(
            &[("mcp-protocol-version", "2025-06-18")],
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
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
        let verbs = ["exec", "resume", "steer", "interrupt", "cancel"];
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
}
