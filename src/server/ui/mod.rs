//! The operator web UI plane: a read-only, server-rendered browser surface
//! served on its own loopback-only listener (`daemon.ui_listen`).
//!
//! Trust model: the listener binds a loopback address only, and an identity
//! proxy running beside the daemon is the one process that reaches it. Every
//! request must carry the proxy's forwarded identity (`X-Forwarded-Email`,
//! else `X-Forwarded-User`); without it the answer is a 401 page and nothing
//! else renders. This is the only listener in the daemon that reads a
//! forwarded header. The main listener never serves a UI route.
//!
//! Pages read through the same internal functions the MCP tools call and
//! never dial the daemon's own MCP endpoint.

mod assets;
mod pages;
mod shell;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use maud::{DOCTYPE, Markup, html};
use tokio_util::sync::CancellationToken;

use super::{BlackboxServer, SharedState};

/// Forwarded identity headers, in precedence order.
const PRINCIPAL_HEADERS: [&str; 2] = ["x-forwarded-email", "x-forwarded-user"];

/// Content Security Policy for every UI page: assets come from this origin
/// only and nothing inline executes.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
     img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; \
     frame-ancestors 'none'";

/// The signed-in operator, as forwarded by the identity proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Principal(pub(crate) String);

/// Facts about this daemon process that the shell header and footer show.
pub(crate) struct DaemonIdentity {
    pub(crate) hostname: String,
    pub(crate) version: &'static str,
    pub(crate) build_id: &'static str,
    pub(crate) started_at: Instant,
}

impl DaemonIdentity {
    fn current() -> Self {
        Self {
            hostname: local_hostname(),
            version: env!("CARGO_PKG_VERSION"),
            build_id: env!("BLACKBOX_BUILD_ID"),
            started_at: Instant::now(),
        }
    }
}

/// Router state for the UI plane.
pub(crate) struct UiState {
    pub(crate) shared: Arc<SharedState>,
    /// An unpinned tool server, for readers that live on `BlackboxServer`.
    pub(crate) server: BlackboxServer,
    pub(crate) identity: DaemonIdentity,
}

impl UiState {
    pub(crate) fn new(shared: Arc<SharedState>) -> Self {
        Self {
            server: BlackboxServer::new(shared.clone()),
            shared,
            identity: DaemonIdentity::current(),
        }
    }

    pub(crate) fn shell<'a>(&'a self, principal: &'a Principal) -> shell::ShellContext<'a> {
        shell::ShellContext {
            principal,
            identity: &self.identity,
            surfaces: self.shared.config.read().surfaces.len(),
        }
    }
}

/// Parse `daemon.ui_listen` and refuse anything that is not a loopback
/// address. A host name other than `localhost` is refused because loopback
/// cannot be proven without resolving it.
pub(crate) fn parse_ui_listen(raw: &str) -> anyhow::Result<SocketAddr> {
    let raw = raw.trim();
    let addr = match raw.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => match raw.rsplit_once(':') {
            Some((host, port)) if host.eq_ignore_ascii_case("localhost") => {
                let port = port
                    .parse::<u16>()
                    .with_context(|| format!("daemon.ui_listen `{raw}` has an invalid port"))?;
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
            }
            _ => anyhow::bail!(
                "daemon.ui_listen `{raw}` must be a loopback address and port \
                 (127.0.0.1:<port>, [::1]:<port> or localhost:<port>)"
            ),
        },
    };
    anyhow::ensure!(
        addr.ip().is_loopback(),
        "refusing daemon.ui_listen `{raw}`: the operator UI listener trusts the \
         identity headers of the proxy in front of it, so it binds a loopback \
         address only"
    );
    Ok(addr)
}

/// Validate and bind the UI listener. A non-loopback address is refused
/// before any socket is opened.
pub(crate) async fn bind_ui_listener(raw: &str) -> anyhow::Result<tokio::net::TcpListener> {
    let addr = parse_ui_listen(raw)?;
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding daemon.ui_listen {addr}"))
}

/// Serve the UI router on its listener until the daemon's cancellation
/// token fires.
pub(crate) fn spawn_ui_server(
    listener: tokio::net::TcpListener,
    router: Router,
    ct: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { ct.cancelled().await });
        if let Err(error) = server.await {
            tracing::error!(%error, "operator UI listener stopped");
        }
    })
}

/// The UI router. Every route, assets and the fallback included, sits
/// behind the principal layer.
pub(crate) fn build_ui_router(state: UiState) -> Router {
    Router::new()
        .route("/", get(pages::overview::page))
        .route("/threads", get(pages::threads::list_page))
        .route("/threads/{id}", get(pages::threads::detail_page))
        .route(
            "/fragments/threads/table",
            get(pages::threads::table_fragment),
        )
        .route(
            "/fragments/threads/{id}",
            get(pages::threads::detail_fragment),
        )
        .route("/bros", get(pages::bros::roster_page))
        .route("/bros/{task}", get(pages::bros::task_page))
        .route("/fragments/bros/rows", get(pages::bros::rows_fragment))
        .route(
            "/fragments/bros/{task}/summary",
            get(pages::bros::summary_fragment),
        )
        .route(
            "/fragments/bros/{task}/tail",
            get(pages::bros::tail_fragment),
        )
        .route("/streams/bros", get(pages::bros::roster_stream))
        .route("/streams/bros/{task}/tail", get(pages::bros::task_stream))
        .route("/projects", get(pages::projects::page))
        .route("/system", get(pages::system::page))
        .route("/config", get(pages::configuration::page))
        .route("/projects/{id}", get(pages::projects::detail_page))
        .route("/fragments/projects", get(pages::projects::fragment))
        .route(
            "/fragments/projects/{id}",
            get(pages::projects::detail_fragment),
        )
        .route("/fragments/system/{panel}", get(pages::system::fragment))
        .route(
            "/fragments/config/{section}",
            get(pages::configuration::fragment),
        )
        .route("/assets/{file}", get(assets::serve))
        .fallback(pages::placeholder::not_found)
        .layer(axum::middleware::from_fn(require_principal))
        .with_state(Arc::new(state))
}

/// Admit a request only from a loopback peer carrying a forwarded identity.
///
/// The peer check restates the bind invariant at the request boundary: a
/// request whose peer address is missing or not loopback did not come
/// through the local proxy and is refused.
async fn require_principal(mut request: Request, next: Next) -> Response {
    let peer_is_loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|info| info.0.ip().is_loopback());
    if !peer_is_loopback {
        return html_response(StatusCode::FORBIDDEN, refusal_page(Refusal::Peer));
    }
    match principal_from_headers(request.headers()) {
        Some(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        None => html_response(StatusCode::UNAUTHORIZED, refusal_page(Refusal::Identity)),
    }
}

/// The first identity header that carries exactly one non-empty value. A
/// header repeated with several values is ambiguous and is not an identity.
fn principal_from_headers(headers: &HeaderMap) -> Option<Principal> {
    PRINCIPAL_HEADERS.iter().find_map(|name| {
        let mut values = headers.get_all(*name).iter();
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        let value = value.to_str().ok()?.trim();
        (!value.is_empty()).then(|| Principal(value.to_string()))
    })
}

/// An HTML response with the UI's fixed headers: pages hold live state, so
/// they are never cached.
pub(crate) fn html_response(status: StatusCode, markup: Markup) -> Response {
    let mut response = (status, markup).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

enum Refusal {
    Identity,
    Peer,
}

/// The stand-alone refusal page. It renders no shell and no daemon state.
fn refusal_page(refusal: Refusal) -> Markup {
    let (title, message) = match refusal {
        Refusal::Identity => (
            "Sign-in required",
            "This page is served only to a signed-in operator. The request \
             reached the daemon without an identity from the sign-in proxy.",
        ),
        Refusal::Peer => (
            "Not available here",
            "This listener accepts requests only from the local sign-in proxy.",
        ),
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " | blackbox" }
                link rel="stylesheet" href=(assets::url("style.css"));
            }
            body {
                main.standalone {
                    h1 { (title) }
                    p { (message) }
                }
            }
        }
    }
}

fn local_hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe a writable buffer this frame
    // owns; gethostname writes at most `len` bytes into it.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return "unknown".to_string();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let name = String::from_utf8_lossy(&buffer[..end]).trim().to_string();
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::net::Ipv6Addr;
    use tower::ServiceExt;

    const PROXY_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 52000);
    const REMOTE_PEER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 2, 3, 4)), 52001);

    fn ui_router(dir: &tempfile::TempDir) -> Router {
        let shared = Arc::new(SharedState::for_test(dir.path()));
        build_ui_router(UiState::new(shared))
    }

    fn request(uri: &str, peer: Option<SocketAddr>, headers: &[(&str, &str)]) -> Request {
        let mut builder = axum::http::Request::builder().uri(uri);
        if let Some(peer) = peer {
            builder = builder.extension(ConnectInfo(peer));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn send(router: Router, request: Request) -> (StatusCode, HeaderMap, String) {
        let response = router.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    #[test]
    fn ui_listen_accepts_loopback_and_refuses_everything_else() {
        assert_eq!(
            parse_ui_listen("127.0.0.1:7300").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7300)
        );
        assert_eq!(
            parse_ui_listen("[::1]:7300").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 7300)
        );
        assert_eq!(
            parse_ui_listen(" localhost:7300 ").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7300)
        );
        for refused in [
            "0.0.0.0:7300",
            "[::]:7300",
            "10.1.2.3:7300",
            "192.168.1.10:7300",
            "ui.example.test:7300",
            "localhost",
            "7300",
            "localhost:notaport",
        ] {
            assert!(
                parse_ui_listen(refused).is_err(),
                "{refused} must be refused"
            );
        }
        let error = parse_ui_listen("0.0.0.0:7300").unwrap_err().to_string();
        assert!(error.contains("loopback"), "{error}");
    }

    #[tokio::test]
    async fn ui_listener_refuses_a_non_loopback_bind_before_opening_a_socket() {
        let error = bind_ui_listener("0.0.0.0:0").await.unwrap_err().to_string();
        assert!(error.contains("refusing daemon.ui_listen"), "{error}");
    }

    #[tokio::test]
    async fn forwarded_email_renders_the_shell_with_that_principal() {
        let dir = tempfile::tempdir().unwrap();
        let (status, headers, body) = send(
            ui_router(&dir),
            request(
                "/",
                Some(PROXY_PEER),
                &[("x-forwarded-email", "operator@example.test")],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert!(body.contains("operator@example.test"), "{body}");
        for label in [
            "Overview",
            "Threads",
            "Bros",
            "Projects",
            "System",
            "Configuration",
        ] {
            assert!(body.contains(label), "navigation must list {label}");
        }
        assert!(body.contains(env!("CARGO_PKG_VERSION")));
        assert!(body.contains(env!("BLACKBOX_BUILD_ID")));
        assert!(body.contains(assets::url("style.css")));
        assert!(body.contains(assets::url("htmx.min.js")));
    }

    #[tokio::test]
    async fn forwarded_user_is_the_fallback_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, body) = send(
            ui_router(&dir),
            request(
                "/",
                Some(PROXY_PEER),
                &[
                    ("x-forwarded-email", "  "),
                    ("x-forwarded-user", "operator"),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("operator"));
    }

    #[tokio::test]
    async fn a_request_without_identity_is_a_401_page() {
        let dir = tempfile::tempdir().unwrap();
        for headers in [
            &[][..],
            &[("x-forwarded-email", "")][..],
            &[("x-forwarded-user", "   ")][..],
        ] {
            for uri in ["/", "/threads", "/assets/anything.css", "/missing"] {
                let (status, response_headers, body) =
                    send(ui_router(&dir), request(uri, Some(PROXY_PEER), headers)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} {headers:?}");
                assert_eq!(response_headers[header::CACHE_CONTROL], "no-store");
                assert!(body.contains("Sign-in required"), "{body}");
                assert!(
                    !body.contains(env!("BLACKBOX_BUILD_ID")),
                    "a refusal renders no daemon state"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_repeated_identity_header_is_not_an_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (status, _, _) = send(
            ui_router(&dir),
            request(
                "/",
                Some(PROXY_PEER),
                &[
                    ("x-forwarded-email", "one@example.test"),
                    ("x-forwarded-email", "two@example.test"),
                ],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_non_loopback_or_unknown_peer_is_refused_even_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        for peer in [Some(REMOTE_PEER), None] {
            let (status, _, body) = send(
                ui_router(&dir),
                request("/", peer, &[("x-forwarded-email", "operator@example.test")]),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{peer:?}");
            assert!(!body.contains("operator@example.test"));
        }
    }

    #[tokio::test]
    async fn assets_serve_immutable_under_their_hash_and_a_wrong_hash_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let identity = [("x-forwarded-email", "operator@example.test")];
        for asset in assets::all() {
            let (status, headers, body) = send(
                ui_router(&dir),
                request(&asset.url, Some(PROXY_PEER), &identity),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{}", asset.url);
            assert_eq!(
                headers[header::CACHE_CONTROL],
                assets::IMMUTABLE_CACHE_CONTROL
            );
            assert!(!body.is_empty());
        }
        let stylesheet = assets::url("style.css");
        let wrong_hash = format!(
            "/assets/style-{}.css",
            "0".repeat(stylesheet.len() - "/assets/style-.css".len())
        );
        assert_ne!(wrong_hash, stylesheet);
        for uri in [
            wrong_hash.as_str(),
            "/assets/style.css",
            "/assets/nothing.js",
        ] {
            let (status, headers, _) =
                send(ui_router(&dir), request(uri, Some(PROXY_PEER), &identity)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_ne!(
                headers[header::CACHE_CONTROL],
                assets::IMMUTABLE_CACHE_CONTROL
            );
        }
    }
}
