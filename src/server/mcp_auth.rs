//! The bearer gate on the daemon's MCP and control planes (`/mcp`, `/control/*`).
//!
//! Off by default (`daemon.mcp_require_bearer`), the gate is identity. On, a
//! request passes when its peer is loopback, when its peer lies inside one
//! of `daemon.mcp_trusted_peer_networks`, or when it carries
//! `Authorization: Bearer <token>` matching the daemon service token
//! (`daemon.service_token_file`, `bro_rpc::ServiceToken` shape, compared in
//! constant time). Every other request is refused with a bare 401 before any
//! handler, the MCP session machinery included: sessionless and handshake
//! requests, the event stream `GET` and the session `DELETE` alike.
//!
//! The peer is the accepted connection's address and nothing else. Behind
//! an ingress the peer is the ingress, so a trusted network can admit a
//! direct client (the dispatch path to a worker host) but never tell one
//! forwarded client from another; through an ingress the bearer is the
//! whole gate. `X-Forwarded-For` is deliberately not read.
//!
//! The gate is mounted over the nested `/mcp` service and over exactly the
//! `/control/*` routes (`super::mcp::build_http_app`). It never covers
//! `/healthz`, `/readyz`, `/tail`, `/internal/*`, or `/admin/*`, which has
//! its own gate (`super::admin_auth`).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bro_rpc::ServiceToken;

use crate::config::DaemonConfig;

/// One trusted peer network, `address/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerNetwork {
    address: IpAddr,
    prefix: u8,
}

impl PeerNetwork {
    /// Parse `a.b.c.d/n` or `x::y/n`; a bare address is a single host.
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let address: IpAddr = address
            .parse()
            .map_err(|error| anyhow::anyhow!("trusted peer network `{text}`: {error}"))?;
        let bits = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= bits)
                .ok_or_else(|| {
                    anyhow::anyhow!("trusted peer network `{text}`: the prefix must be 0 to {bits}")
                })?,
            None => bits,
        };
        Ok(Self { address, prefix })
    }

    fn contains(&self, peer: IpAddr) -> bool {
        match (self.address, peer) {
            (IpAddr::V4(network), IpAddr::V4(peer)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - u32::from(self.prefix))
                };
                u32::from(network) & mask == u32::from(peer) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(peer)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - u32::from(self.prefix))
                };
                u128::from(network) & mask == u128::from(peer) & mask
            }
            // A v4-mapped v6 peer against a v4 network is the same host.
            (IpAddr::V4(_), IpAddr::V6(peer)) => peer
                .to_ipv4_mapped()
                .is_some_and(|peer| self.contains(IpAddr::V4(peer))),
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// The gate's authority, resolved once at router construction.
#[derive(Clone, Default)]
pub(crate) struct McpAuth {
    inner: Option<Arc<Enabled>>,
}

struct Enabled {
    token: ServiceToken,
    trusted: Vec<PeerNetwork>,
}

impl McpAuth {
    /// Resolve the gate from configuration. With the gate on, a service
    /// token that is absent or fails to load is a startup error: there is
    /// no loopback recovery for the whole MCP surface, so a daemon must not
    /// come up refusing every client by accident.
    pub(super) fn from_config(cfg: &DaemonConfig) -> anyhow::Result<Self> {
        if !cfg.mcp_require_bearer {
            return Ok(Self { inner: None });
        }
        let path = cfg.service_token_file.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "daemon.mcp_require_bearer is on but daemon.service_token_file (BLACKBOX_SERVICE_TOKEN_FILE) is not set"
            )
        })?;
        let token = ServiceToken::load(path).map_err(|error| {
            anyhow::anyhow!(
                "daemon.mcp_require_bearer is on but the service token {} cannot be loaded: {error}",
                path.display()
            )
        })?;
        let trusted = cfg
            .mcp_trusted_peer_networks
            .iter()
            .map(|text| PeerNetwork::parse(text))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            inner: Some(Arc::new(Enabled { token, trusted })),
        })
    }

    /// Whether the gate is on.
    pub(crate) fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    fn admits(&self, peer: Option<SocketAddr>, bearer: Option<&str>) -> bool {
        let Some(enabled) = &self.inner else {
            return true;
        };
        if let Some(peer) = peer {
            let ip = peer.ip();
            if ip.is_loopback() || enabled.trusted.iter().any(|network| network.contains(ip)) {
                return true;
            }
        }
        // `ServiceToken::verify` is constant time.
        bearer.is_some_and(|secret| enabled.token.verify(secret))
    }
}

/// The credentials of an `Authorization` value in the `Bearer` scheme: the
/// scheme name is case-insensitive (RFC 7235) and is followed by exactly one
/// space; anything else is not a bearer at all.
fn bearer_credentials(value: &str) -> Option<&str> {
    let (scheme, credentials) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || credentials.is_empty()
        || credentials.starts_with(' ')
    {
        return None;
    }
    Some(credentials)
}

/// The layer over `/mcp` and `/control/*`.
pub(crate) async fn authenticate_mcp_request(
    State(auth): State<McpAuth>,
    request: Request,
    next: Next,
) -> Response {
    // A missing peer address is non-loopback: the daemon serves with
    // `into_make_service_with_connect_info`, so the real path always has one.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_credentials);
    if auth.admits(peer, bearer) {
        return next.run(request).await;
    }
    // Bare: no code, no sentence. The refused caller learns the scheme and
    // nothing about what was expected.
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::SharedState;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt as _;

    const REMOTE: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(10, 42, 0, 9)), 51000);
    const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 51001);
    const TRUSTED: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 149)), 51002);

    /// An app with the gate on (`Some(secret)`) or off, plus the secret.
    fn app(gate: bool) -> (axum::Router, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let shared = Arc::new(SharedState::for_test(dir.path()));
        let mut cfg = shared.config.read().clone();
        let token_file = dir.path().join("service.token");
        let secret = ServiceToken::load_or_create(&token_file)
            .unwrap()
            .expose_secret()
            .to_string();
        cfg.daemon.mcp_require_bearer = gate;
        cfg.daemon.service_token_file = Some(token_file);
        cfg.daemon.mcp_trusted_peer_networks = vec!["192.168.0.149".to_string()];
        let app = crate::server::mcp::build_http_app(
            shared,
            &cfg,
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        (app, secret, dir)
    }

    fn request(
        method: Method,
        uri: &str,
        peer: Option<SocketAddr>,
        bearer: Option<&str>,
        body: &str,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("host", "127.0.0.1");
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(peer));
        }
        if let Some(bearer) = bearer {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        builder.body(Body::from(body.to_string())).unwrap()
    }

    fn initialize() -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "gate-test", "version": "0"}},
        })
        .to_string()
    }

    fn sessionless_list() -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {"name": "gate-test", "version": "0"},
                "io.modelcontextprotocol/clientCapabilities": {}}},
        })
        .to_string()
    }

    async fn status(app: &axum::Router, request: Request<Body>) -> (StatusCode, bool) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bare = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .is_some_and(|value| value == "Bearer");
        if status == StatusCode::UNAUTHORIZED {
            let bytes = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert!(bytes.is_empty(), "a refusal carries no body");
        }
        (status, bare)
    }

    /// With the gate on, a remote peer without the bearer is refused on the
    /// handshake, the sessionless request, the event stream and the session
    /// end, and on every control route, before any handler; loopback, a
    /// trusted peer and the bearer each pass.
    #[tokio::test]
    async fn the_gate_refuses_remote_peers_without_the_bearer_on_every_mcp_and_control_path() {
        let (app, secret, _dir) = app(true);
        let paths: Vec<(Method, &str, String)> = vec![
            (Method::POST, "/mcp?surface=readonly", initialize()),
            (Method::POST, "/mcp?surface=readonly", sessionless_list()),
            (Method::GET, "/mcp", String::new()),
            (Method::DELETE, "/mcp", String::new()),
            (Method::POST, "/control/exec", "{}".into()),
            (Method::POST, "/control/resume", "{}".into()),
            (Method::POST, "/control/closeout", "{}".into()),
            (Method::POST, "/control/steer", "{}".into()),
            (Method::POST, "/control/interrupt", "{}".into()),
            (Method::GET, "/control/status/no-such-task", String::new()),
            (Method::GET, "/control/roster", String::new()),
            (
                Method::DELETE,
                "/control/roster/no-such-task",
                String::new(),
            ),
            (Method::GET, "/control/roster/stream", String::new()),
            (Method::GET, "/control/dashboard", String::new()),
            (Method::POST, "/control/cancel", "{}".into()),
        ];
        for (method, uri, body) in &paths {
            let refused =
                status(&app, request(method.clone(), uri, Some(REMOTE), None, body)).await;
            assert_eq!(refused, (StatusCode::UNAUTHORIZED, true), "{method} {uri}");
            let no_peer = status(&app, request(method.clone(), uri, None, None, body)).await;
            assert_eq!(
                no_peer,
                (StatusCode::UNAUTHORIZED, true),
                "{method} {uri} without peer"
            );
            let wrong = status(
                &app,
                request(method.clone(), uri, Some(REMOTE), Some(&secret[..63]), body),
            )
            .await;
            assert_eq!(
                wrong,
                (StatusCode::UNAUTHORIZED, true),
                "{method} {uri} wrong bearer"
            );
            let mut two_spaces = request(method.clone(), uri, Some(REMOTE), None, body);
            two_spaces.headers_mut().insert(
                header::AUTHORIZATION,
                format!("Bearer  {secret}").parse().unwrap(),
            );
            assert_eq!(
                status(&app, two_spaces).await,
                (StatusCode::UNAUTHORIZED, true),
                "{method} {uri} two spaces"
            );
            let mut lowercase = request(method.clone(), uri, Some(REMOTE), None, body);
            lowercase.headers_mut().insert(
                header::AUTHORIZATION,
                format!("bearer {secret}").parse().unwrap(),
            );
            for admitted in [
                request(method.clone(), uri, Some(LOOPBACK), None, body),
                request(method.clone(), uri, Some(TRUSTED), None, body),
                request(method.clone(), uri, Some(REMOTE), Some(&secret), body),
                lowercase,
            ] {
                let (status, _) = status(&app, admitted).await;
                assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {uri} admitted");
            }
        }
        // The planes outside the gate stay open to a remote peer.
        for (method, uri) in [(Method::GET, "/healthz"), (Method::GET, "/readyz")] {
            let (status, _) =
                status(&app, request(method.clone(), uri, Some(REMOTE), None, "")).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
        }
    }

    /// With the gate off nothing changes for anyone.
    #[tokio::test]
    async fn the_gate_off_is_identity() {
        let (app, _secret, _dir) = app(false);
        for (method, uri, body) in [
            (Method::POST, "/mcp?surface=readonly", initialize()),
            (Method::POST, "/mcp?surface=readonly", sessionless_list()),
            (Method::GET, "/control/roster", String::new()),
        ] {
            for peer in [Some(REMOTE), None] {
                let (status, _) =
                    status(&app, request(method.clone(), uri, peer, None, &body)).await;
                assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {peer:?}");
            }
        }
    }

    /// The gate on without a loadable token is a startup error, never a
    /// daemon that refuses everyone.
    #[test]
    fn the_gate_on_needs_a_loadable_service_token() {
        let dir = tempfile::tempdir().unwrap();
        let shared = SharedState::for_test(dir.path());
        let mut cfg = shared.config.read().daemon.clone();
        cfg.mcp_require_bearer = true;
        let error = McpAuth::from_config(&cfg)
            .err()
            .map(|e| e.to_string())
            .unwrap();
        assert!(error.contains("service_token_file"), "{error}");
        cfg.service_token_file = Some(dir.path().join("missing.token"));
        let error = McpAuth::from_config(&cfg)
            .err()
            .map(|e| e.to_string())
            .unwrap();
        assert!(error.contains("cannot be loaded"), "{error}");
        cfg.mcp_trusted_peer_networks = vec!["not a network".into()];
        ServiceToken::load_or_create(cfg.service_token_file.as_ref().unwrap()).unwrap();
        assert!(McpAuth::from_config(&cfg).is_err());
        cfg.mcp_trusted_peer_networks.clear();
        assert!(McpAuth::from_config(&cfg).unwrap().enabled());
        cfg.mcp_require_bearer = false;
        cfg.service_token_file = None;
        assert!(!McpAuth::from_config(&cfg).unwrap().enabled());
    }

    #[test]
    fn a_peer_network_contains_its_hosts_and_nothing_else() {
        let lan = PeerNetwork::parse("192.168.0.0/24").unwrap();
        assert!(lan.contains("192.168.0.149".parse().unwrap()));
        assert!(!lan.contains("192.168.1.1".parse().unwrap()));
        assert!(lan.contains("::ffff:192.168.0.7".parse().unwrap()));
        assert!(!lan.contains("fe80::1".parse().unwrap()));

        let host = PeerNetwork::parse("192.168.0.149").unwrap();
        assert_eq!(host.prefix, 32);
        assert!(host.contains("192.168.0.149".parse().unwrap()));
        assert!(!host.contains("192.168.0.150".parse().unwrap()));

        let six = PeerNetwork::parse("2001:db8::/32").unwrap();
        assert!(six.contains("2001:db8:1::5".parse().unwrap()));
        assert!(!six.contains("2001:db9::5".parse().unwrap()));
        assert!(!six.contains("192.168.0.1".parse().unwrap()));

        let all = PeerNetwork::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));

        for bad in ["", "lan", "192.168.0.0/33", "2001:db8::/129", "10.0.0.0/-1"] {
            assert!(PeerNetwork::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive_with_exactly_one_space() {
        assert_eq!(bearer_credentials("Bearer abc"), Some("abc"));
        assert_eq!(bearer_credentials("bearer abc"), Some("abc"));
        assert_eq!(bearer_credentials("BEARER abc"), Some("abc"));
        assert_eq!(bearer_credentials("Bearer  abc"), None);
        assert_eq!(bearer_credentials("Bearer"), None);
        assert_eq!(bearer_credentials("Bearer "), None);
        assert_eq!(bearer_credentials("Basic abc"), None);
        assert_eq!(bearer_credentials("abc"), None);
    }

    #[test]
    fn the_gate_off_admits_everything_and_on_admits_loopback_trusted_or_bearer() {
        let off = McpAuth::default();
        assert!(off.admits(None, None));
        assert!(!off.enabled());

        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("service.token");
        let token = ServiceToken::load_or_create(&token_file).unwrap();
        let on = McpAuth {
            inner: Some(Arc::new(Enabled {
                token: ServiceToken::load(&token_file).unwrap(),
                trusted: vec![PeerNetwork::parse("192.168.0.149").unwrap()],
            })),
        };
        assert!(on.enabled());
        let remote: SocketAddr = "10.42.0.9:51000".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:51000".parse().unwrap();
        let trusted: SocketAddr = "192.168.0.149:51000".parse().unwrap();
        assert!(on.admits(Some(loopback), None));
        assert!(on.admits(Some(trusted), None));
        assert!(!on.admits(Some(remote), None));
        assert!(!on.admits(None, None));
        let secret = token.expose_secret().to_string();
        assert!(on.admits(Some(remote), Some(&secret)));
        assert!(on.admits(None, Some(&secret)));
        assert!(!on.admits(Some(remote), Some(&secret[..63])));
        assert!(!on.admits(Some(remote), Some("")));
    }
}
