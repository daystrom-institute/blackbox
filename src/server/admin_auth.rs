//! Authentication for the daemon's operator admin plane (`/admin/*`).
//!
//! A request is admitted when the connection's peer address is loopback
//! (the historical trust boundary, so local operator tooling keeps working
//! against a loopback bind without credentials) or when it carries
//! `Authorization: Bearer <token>` matching the operator-configured service
//! token (`daemon.admin_token_file` / `BLACKBOX_ADMIN_TOKEN_FILE`, loaded
//! through `bro_rpc::ServiceToken` and compared in constant time). Every
//! other request, including one whose peer address the server cannot see,
//! is refused with 401. With no token configured the plane is
//! loopback-only.
//!
//! The gate is mounted as a `route_layer` over exactly the `/admin/*`
//! routes (`super::mcp::build_http_app`); it never covers `/mcp`,
//! `/internal/*`, `/control/*`, `/healthz`, or `/readyz`.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bbox_code_source::ErrorResponse;
use bro_rpc::ServiceToken;

use crate::config::DaemonConfig;

/// The admin plane's bearer authority, resolved once at router construction.
#[derive(Clone, Default)]
pub(crate) struct AdminAuth {
    token: Option<ServiceToken>,
}

impl AdminAuth {
    /// Load the configured admin bearer. Called once per router build (the
    /// daemon builds its router once per process lifetime), so the file read
    /// is startup work, never per-request work.
    ///
    /// A configured token that fails to load (missing file, wrong shape,
    /// lax permissions) fails closed: no bearer is admitted and the plane
    /// stays loopback-only, with one loud error. Loopback remains the
    /// operator's recovery route, so a bad token degrades the remote admin
    /// path instead of killing the daemon.
    pub(super) fn from_config(cfg: &DaemonConfig) -> Self {
        let Some(path) = cfg.admin_token_file.as_deref() else {
            return Self { token: None };
        };
        match ServiceToken::load(path) {
            Ok(token) => Self { token: Some(token) },
            Err(error) => {
                tracing::error!(
                    path = %path.display(),
                    %error,
                    "admin token file failed to load; /admin/* stays loopback-only"
                );
                Self { token: None }
            }
        }
    }
}

/// `route_layer` gate for the `/admin/*` family: loopback peer or service
/// bearer, nothing else.
pub(crate) async fn authenticate_admin_request(
    State(auth): State<AdminAuth>,
    request: Request,
    next: Next,
) -> Response {
    // Missing peer info is treated as non-loopback (fail closed): the daemon
    // serves with `into_make_service_with_connect_info`, so an absent
    // `ConnectInfo` means the request did not arrive through the real
    // serving path.
    let peer_is_loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|info| info.0.ip().is_loopback());
    if peer_is_loopback {
        return next.run(request).await;
    }
    let candidate = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    // `ServiceToken::verify` is constant time.
    let bearer_matches = candidate.is_some_and(|secret| {
        auth.token
            .as_ref()
            .is_some_and(|token| token.verify(secret))
    });
    if bearer_matches {
        return next.run(request).await;
    }
    unauthorized_response()
}

/// The daemon's standard JSON rejection shape, matching the producer lanes'
/// bearer 401 (`super::producer_auth`).
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
            code: "unauthorized".to_string(),
            message: "admin plane requires a loopback peer or the daemon service bearer"
                .to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::SharedState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    const LOOPBACK_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51111);
    const REMOTE_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)), 51112);

    /// `ServiceToken::load` refuses group/world-readable files, so the test
    /// token is written owner-only.
    fn write_owner_only_token(dir: &std::path::Path, secret: &str) -> PathBuf {
        let path = dir.join("admin-token");
        std::fs::write(&path, secret).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn admin_app(secret: Option<&str>) -> axum::Router {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let mut cfg = shared.config.read().clone();
        if let Some(secret) = secret {
            cfg.daemon.admin_token_file = Some(write_owner_only_token(dir.path(), secret));
        }
        crate::server::mcp::build_http_app(
            shared,
            &cfg,
            &tokio_util::sync::CancellationToken::new(),
        )
    }

    fn get(peer: Option<SocketAddr>, bearer: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri("/admin/artifact/list");
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(peer));
        }
        if let Some(bearer) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn status_of(
        app: axum::Router,
        request: Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    #[tokio::test]
    async fn admin_without_bearer_from_non_loopback_peer_is_unauthorized() {
        let app = admin_app(Some(&"a".repeat(64)));
        let (status, body) = status_of(app, get(Some(REMOTE_PEER), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "unauthorized");
    }

    #[tokio::test]
    async fn admin_with_service_bearer_from_non_loopback_peer_is_admitted() {
        let secret = "b".repeat(64);
        let app = admin_app(Some(&secret));
        let (status, body) = status_of(app, get(Some(REMOTE_PEER), Some(&secret))).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(body.get("artifacts").is_some());
    }

    #[tokio::test]
    async fn admin_from_loopback_peer_without_bearer_is_admitted() {
        let app = admin_app(Some(&"c".repeat(64)));
        let (status, body) = status_of(app, get(Some(LOOPBACK_PEER), None)).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(body.get("artifacts").is_some());
    }

    #[tokio::test]
    async fn admin_with_wrong_bearer_from_non_loopback_peer_is_unauthorized() {
        let app = admin_app(Some(&"d".repeat(64)));
        let (status, body) = status_of(app, get(Some(REMOTE_PEER), Some(&"e".repeat(64)))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "unauthorized");
    }

    #[tokio::test]
    async fn admin_without_configured_token_is_loopback_only() {
        let app = admin_app(None);
        let (status, _) =
            status_of(app.clone(), get(Some(REMOTE_PEER), Some(&"f".repeat(64)))).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a bearer must not authenticate when no token is configured"
        );
        let (status, _) = status_of(app, get(Some(LOOPBACK_PEER), None)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_without_peer_info_fails_closed() {
        let app = admin_app(Some(&"9".repeat(64)));
        let (status, _) = status_of(app, get(None, None)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a request with no visible peer address must not be admitted"
        );
    }
}
