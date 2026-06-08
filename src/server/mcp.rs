use crate::server::routes::*;
use crate::server::tail::{roster_stream_handler, tail_handler};
use crate::server::{BlackboxServer, SharedState};
use crate::{config, council};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn build_http_app(
    shared: Arc<SharedState>,
    cfg: &config::Config,
    ct: &CancellationToken,
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
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            session_manager.into(),
            server_config,
        );

    axum::Router::new()
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
        // (the bro-irc sidecar, the fleet client, future bridges). The canonical
        // namespace is `/control/*`; `/irc/*` is retained as a back-compat alias
        // for the IRC bridge's historical contract. Nothing here is IRC-specific
        // — consumers depend on the neutral control endpoint, not on each other's
        // namespace.
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
        // Back-compat aliases (legacy `/irc/*` contract).
        .route("/irc/exec", axum::routing::post(control_exec_handler))
        .route("/irc/resume", axum::routing::post(control_resume_handler))
        .route("/irc/steer", axum::routing::post(control_steer_handler))
        .route(
            "/irc/interrupt",
            axum::routing::post(control_interrupt_handler),
        )
        .route(
            "/irc/broadcast",
            axum::routing::post(control_broadcast_handler),
        )
        .route(
            "/irc/status/{task_id}",
            axum::routing::get(control_status_handler),
        )
        .route(
            "/irc/dashboard",
            axum::routing::get(control_dashboard_handler),
        )
        .route("/irc/cancel", axum::routing::post(control_cancel_handler))
        .route(
            "/irc/team/{team_name}",
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

    /// The generic control plane is reachable at the neutral `/control/*`
    /// namespace AND the legacy `/irc/*` alias, both bound to the same handler
    /// (harness-daemon-boundary.md /irc-decoupling). A typo in either path table
    /// would surface here as a 404.
    #[tokio::test]
    async fn control_and_irc_dashboard_both_resolve_to_same_handler() {
        for path in ["/control/dashboard", "/irc/dashboard"] {
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
    }

    /// Every control verb is mounted under both namespaces. GET on the POST-only
    /// verbs yields 405 (not 404) — proving the path exists with a handler.
    #[tokio::test]
    async fn control_verbs_mounted_under_both_namespaces() {
        let verbs = [
            "exec",
            "resume",
            "steer",
            "interrupt",
            "broadcast",
            "cancel",
        ];
        for ns in ["/control", "/irc"] {
            for verb in verbs {
                let path = format!("{ns}/{verb}");
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
    }

    /// The `/control/closeout` endpoint is mounted (Phase 3a, design
    /// fleet-tui/closeout-command.md §4.1). It is a daemon-side-only
    /// endpoint — there is no `/irc/closeout` alias, the namespace is
    /// neutral and the cockpit calls it directly. A GET on the POST-only
    /// route yields 405 (not 404) — proving the path exists with a
    /// handler.
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
