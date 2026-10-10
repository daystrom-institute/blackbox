//! OAuth access tokens on the MCP and control planes (`daemon.mcp_oauth`).
//!
//! With `[daemon.mcp_oauth]` configured, the MCP gate (`super::mcp_auth`)
//! admits a request whose `Authorization: Bearer` value is an access token
//! from the configured issuer, after the loopback, trusted-peer and service
//! bearer checks have all declined it. A token is admitted when:
//!
//! - it is at most [`MAX_TOKEN_BYTES`] long (checked before any parsing);
//! - its header names `RS256` and a `kid` found in the issuer's JWKS;
//! - its signature verifies against that key;
//! - `iss` equals the issuer and `aud` (a string or an array) contains the
//!   audience;
//! - `exp` has not passed and `nbf`, when present, has, each with
//!   [`CLOCK_SKEW_SECS`] of skew;
//! - `groups` contains the required group (exact string match);
//! - it names a principal: `email`, else `preferred_username`, else `sub`.
//!
//! Keys come from the configured `jwks_uri`, or from the `jwks_uri` of the
//! issuer's OpenID discovery document. They are fetched on first use, kept
//! for [`KEY_TTL`], and refetched early only for an unknown `kid` and never
//! more often than [`REFETCH_INTERVAL`]; a failed refresh keeps the last
//! good keys. Failed verifications are counted per peer address in a fixed
//! window; a peer over [`FAILURE_LIMIT`] is refused without verification
//! until the window ends, so a flood of bad tokens cannot drive key fetches.
//!
//! Token material never reaches a log, a panic, a `Debug` rendering or an
//! error string: a refusal is a [`Refusal`] category and nothing else.
//!
//! The verified principal rides the HTTP request extensions
//! ([`McpPrincipal`]); `call_tool` lifts it into a task-local scope
//! ([`with_call_principal`]) so the per-call telemetry line can name it.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::config::McpOAuthConfig;

/// The path of the protected resource metadata document (RFC 9728) for the
/// `/mcp` resource.
pub(crate) const METADATA_PATH: &str = "/.well-known/oauth-protected-resource/mcp";
/// The scopes a client requests and the metadata advertises.
pub(crate) const SCOPES: [&str; 4] = ["openid", "profile", "email", "groups"];
/// The longest bearer value treated as a candidate token.
pub(crate) const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Allowed clock skew on `exp` and `nbf`.
pub(crate) const CLOCK_SKEW_SECS: u64 = 30;
/// How long fetched keys are trusted before a refresh.
pub(crate) const KEY_TTL: Duration = Duration::from_secs(300);
/// The minimum interval between two key fetches.
pub(crate) const REFETCH_INTERVAL: Duration = Duration::from_secs(10);
/// Failed verifications a peer may accumulate in one window.
pub(crate) const FAILURE_LIMIT: u32 = 20;
/// The failure counting window.
pub(crate) const FAILURE_WINDOW: Duration = Duration::from_secs(60);

const RESOURCE_NAME: &str = "blackboxd MCP";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_KEYS: usize = 64;
const MAX_KID_BYTES: usize = 256;
const MAX_PRINCIPAL_BYTES: usize = 256;
/// Peers tracked individually by the failure limiter; beyond this, new
/// peers share one bucket.
const MAX_TRACKED_PEERS: usize = 4096;

/// A principal named by a verified access token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpPrincipal(Arc<str>);

impl McpPrincipal {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for McpPrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

tokio::task_local! {
    static CALL_PRINCIPAL: Option<McpPrincipal>;
}

/// Run one tool call with its verified principal in scope.
pub(crate) async fn with_call_principal<F: Future>(
    principal: Option<McpPrincipal>,
    call: F,
) -> F::Output {
    CALL_PRINCIPAL.scope(principal, call).await
}

/// The verified principal of the tool call in progress on this task, if any.
pub(crate) fn call_principal() -> Option<McpPrincipal> {
    CALL_PRINCIPAL.try_with(Clone::clone).ok().flatten()
}

/// Why a bearer was not admitted as an access token. Carries no token
/// material by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    RateLimited,
    TooLong,
    Malformed,
    Algorithm,
    NoKeyId,
    UnknownKey,
    Signature,
    Expired,
    NotYetValid,
    Issuer,
    Audience,
    MissingClaim,
    Group,
    NoPrincipal,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RateLimited => "peer over the failed verification limit",
            Self::TooLong => "token too long",
            Self::Malformed => "malformed token",
            Self::Algorithm => "algorithm is not RS256",
            Self::NoKeyId => "no key id",
            Self::UnknownKey => "unknown key id",
            Self::Signature => "bad signature",
            Self::Expired => "expired",
            Self::NotYetValid => "not yet valid",
            Self::Issuer => "wrong issuer",
            Self::Audience => "wrong audience",
            Self::MissingClaim => "missing required claim",
            Self::Group => "missing required group",
            Self::NoPrincipal => "no principal claim",
        })
    }
}

impl Refusal {
    fn from_kind(kind: &ErrorKind) -> Self {
        match kind {
            ErrorKind::InvalidSignature => Self::Signature,
            ErrorKind::ExpiredSignature => Self::Expired,
            ErrorKind::ImmatureSignature => Self::NotYetValid,
            ErrorKind::InvalidIssuer => Self::Issuer,
            ErrorKind::InvalidAudience => Self::Audience,
            ErrorKind::InvalidAlgorithm => Self::Algorithm,
            ErrorKind::MissingRequiredClaim(_) => Self::MissingClaim,
            _ => Self::Malformed,
        }
    }
}

/// Timing knobs, fixed in production and shortened by tests.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    key_ttl: Duration,
    refetch_interval: Duration,
    failure_limit: u32,
    failure_window: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            key_ttl: KEY_TTL,
            refetch_interval: REFETCH_INTERVAL,
            failure_limit: FAILURE_LIMIT,
            failure_window: FAILURE_WINDOW,
        }
    }
}

/// The access token verifier, resolved once at router construction.
#[derive(Clone)]
pub(crate) struct McpOAuth {
    inner: Arc<Inner>,
}

struct Inner {
    issuer: String,
    required_group: String,
    configured_jwks_uri: Option<String>,
    validation: Validation,
    challenge: HeaderValue,
    metadata: serde_json::Value,
    http: reqwest::Client,
    keys: tokio::sync::Mutex<KeyCache>,
    failures: parking_lot::Mutex<FailureLimiter>,
    tuning: Tuning,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    /// When the current key set was fetched.
    fetched_at: Option<Instant>,
    /// When a fetch was last attempted, successful or not.
    attempted_at: Option<Instant>,
    /// The JWKS location read from discovery, kept until a fetch fails.
    discovered_jwks_uri: Option<String>,
}

impl std::fmt::Debug for McpOAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuth")
            .field("issuer", &self.inner.issuer)
            .field("required_group", &self.inner.required_group)
            .finish_non_exhaustive()
    }
}

impl McpOAuth {
    /// Resolve the verifier from `[daemon.mcp_oauth]`. Nothing is fetched
    /// here: keys load on the first token, so an unreachable issuer never
    /// stops the daemon.
    pub(crate) fn from_config(cfg: &McpOAuthConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(3))
            // The issuer is reached directly, never through a proxy from the
            // environment.
            .no_proxy()
            .build()
            .map_err(|error| anyhow::anyhow!("daemon.mcp_oauth: HTTP client: {error}"))?;
        Self::with_tuning(cfg, Tuning::default(), http)
    }

    fn with_tuning(
        cfg: &McpOAuthConfig,
        tuning: Tuning,
        http: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let audience = reqwest::Url::parse(&cfg.audience)
            .map_err(|error| anyhow::anyhow!("daemon.mcp_oauth.audience: {error}"))?;
        if !matches!(audience.scheme(), "http" | "https") || !audience.has_host() {
            anyhow::bail!("daemon.mcp_oauth.audience must be an http or https URL with a host");
        }
        let metadata_url = format!("{}{METADATA_PATH}", audience.origin().ascii_serialization());
        let challenge = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{metadata_url}\", scope=\"{}\"",
            SCOPES.join(" ")
        ))
        .map_err(|_| {
            anyhow::anyhow!("daemon.mcp_oauth.audience does not fit a WWW-Authenticate header")
        })?;
        for (name, value) in [
            ("issuer", Some(&cfg.issuer)),
            ("jwks_uri", cfg.jwks_uri.as_ref()),
        ] {
            let Some(value) = value else { continue };
            let url = reqwest::Url::parse(value)
                .map_err(|error| anyhow::anyhow!("daemon.mcp_oauth.{name}: {error}"))?;
            if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
                anyhow::bail!("daemon.mcp_oauth.{name} must be an http or https URL with a host");
            }
        }

        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = CLOCK_SKEW_SECS;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.set_issuer(&[&cfg.issuer]);
        validation.set_audience(&[&cfg.audience]);
        validation.required_spec_claims = ["exp", "iss", "aud"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let metadata = serde_json::json!({
            "resource": cfg.audience,
            "authorization_servers": [cfg.issuer],
            "bearer_methods_supported": ["header"],
            "scopes_supported": SCOPES,
            "resource_name": RESOURCE_NAME,
        });
        Ok(Self {
            inner: Arc::new(Inner {
                issuer: cfg.issuer.clone(),
                required_group: cfg.required_group.clone(),
                configured_jwks_uri: cfg.jwks_uri.clone(),
                validation,
                challenge,
                metadata,
                http,
                keys: tokio::sync::Mutex::new(KeyCache::default()),
                failures: parking_lot::Mutex::new(FailureLimiter::default()),
                tuning,
            }),
        })
    }

    /// The `WWW-Authenticate` value every refusal carries.
    pub(crate) fn challenge(&self) -> &HeaderValue {
        &self.inner.challenge
    }

    /// The protected resource metadata document.
    pub(crate) fn metadata(&self) -> &serde_json::Value {
        &self.inner.metadata
    }

    /// Verify a bearer as an access token for `peer`. Failures count against
    /// the peer; a peer over the limit is refused before any verification.
    pub(crate) async fn verify(
        &self,
        peer: Option<SocketAddr>,
        token: &str,
    ) -> Result<McpPrincipal, Refusal> {
        let peer = peer.map(|peer| peer.ip());
        let tuning = self.inner.tuning;
        if self
            .inner
            .failures
            .lock()
            .limited(peer, Instant::now(), &tuning)
        {
            return Err(Refusal::RateLimited);
        }
        let verdict = self.verify_token(token).await;
        if verdict.is_err() {
            self.inner
                .failures
                .lock()
                .record(peer, Instant::now(), &tuning);
        }
        verdict
    }

    async fn verify_token(&self, token: &str) -> Result<McpPrincipal, Refusal> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(Refusal::TooLong);
        }
        // An unsupported `alg` (`none` among them) fails to parse here.
        let header = jsonwebtoken::decode_header(token).map_err(|_| Refusal::Malformed)?;
        if header.alg != Algorithm::RS256 {
            return Err(Refusal::Algorithm);
        }
        let kid = header.kid.ok_or(Refusal::NoKeyId)?;
        if kid.is_empty() || kid.len() > MAX_KID_BYTES {
            return Err(Refusal::NoKeyId);
        }
        let key = self.key_for(&kid).await?;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &self.inner.validation)
            .map_err(|error| Refusal::from_kind(error.kind()))?;
        let claims = data.claims;
        if claims.iss != self.inner.issuer {
            return Err(Refusal::Issuer);
        }
        if !claims
            .groups
            .iter()
            .any(|group| group == &self.inner.required_group)
        {
            return Err(Refusal::Group);
        }
        [claims.email, claims.preferred_username, claims.sub]
            .into_iter()
            .flatten()
            .find(|name| {
                !name.is_empty()
                    && name.len() <= MAX_PRINCIPAL_BYTES
                    && !name.chars().any(char::is_control)
            })
            .map(|name| McpPrincipal(name.into()))
            .ok_or(Refusal::NoPrincipal)
    }

    /// The key for `kid`, refreshing the key set when it is stale or does
    /// not know `kid`, at most once per refetch interval. Concurrent callers
    /// wait on the one fetch in flight.
    async fn key_for(&self, kid: &str) -> Result<DecodingKey, Refusal> {
        let tuning = self.inner.tuning;
        let mut cache = self.inner.keys.lock().await;
        let now = Instant::now();
        let fresh = cache
            .fetched_at
            .is_some_and(|at| now.duration_since(at) < tuning.key_ttl);
        let due = cache
            .attempted_at
            .is_none_or(|at| now.duration_since(at) >= tuning.refetch_interval);
        if (!fresh || !cache.keys.contains_key(kid)) && due {
            cache.attempted_at = Some(now);
            let discovered = cache.discovered_jwks_uri.take();
            match self.fetch_keys(discovered).await {
                Ok((keys, jwks_uri)) => {
                    tracing::info!(
                        target: "blackbox::mcp_auth",
                        keys = keys.len(),
                        "OAuth signing keys refreshed"
                    );
                    cache.keys = keys;
                    cache.fetched_at = Some(Instant::now());
                    cache.discovered_jwks_uri = jwks_uri;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "blackbox::mcp_auth",
                        error = %error,
                        "OAuth signing key refresh failed; keeping the last good keys"
                    );
                }
            }
        }
        cache.keys.get(kid).cloned().ok_or(Refusal::UnknownKey)
    }

    /// Fetch the key set. Returns the keys and, when discovery located the
    /// set, the discovered location to reuse on the next fetch.
    async fn fetch_keys(
        &self,
        discovered: Option<String>,
    ) -> anyhow::Result<(HashMap<String, DecodingKey>, Option<String>)> {
        let (jwks_uri, keep) = match (&self.inner.configured_jwks_uri, discovered) {
            (Some(configured), _) => (configured.clone(), None),
            (None, Some(discovered)) => (discovered.clone(), Some(discovered)),
            (None, None) => {
                let discovery = format!(
                    "{}/.well-known/openid-configuration",
                    self.inner.issuer.trim_end_matches('/')
                );
                let document: Discovery = self.fetch_json(&discovery).await?;
                let url = reqwest::Url::parse(&document.jwks_uri)
                    .map_err(|error| anyhow::anyhow!("discovered jwks_uri: {error}"))?;
                if !matches!(url.scheme(), "http" | "https") {
                    anyhow::bail!("discovered jwks_uri is not an http or https URL");
                }
                (document.jwks_uri.clone(), Some(document.jwks_uri))
            }
        };
        let set: JwkSet = self.fetch_json(&jwks_uri).await?;
        let keys = set
            .keys
            .iter()
            .filter_map(signing_key)
            .take(MAX_KEYS)
            .collect::<HashMap<_, _>>();
        Ok((keys, keep))
    }

    async fn fetch_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let mut response = self
            .inner
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| anyhow::anyhow!("GET {url}: {}", error.without_url()))?;
        if !response.status().is_success() {
            anyhow::bail!("GET {url}: HTTP {}", response.status());
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64)
        {
            anyhow::bail!("GET {url}: document over {MAX_DOCUMENT_BYTES} bytes");
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| anyhow::anyhow!("GET {url}: {}", error.without_url()))?
        {
            if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
                anyhow::bail!("GET {url}: document over {MAX_DOCUMENT_BYTES} bytes");
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|error| anyhow::anyhow!("GET {url}: {error}"))
    }
}

/// One usable RS256 signing key from a JWK, keyed by `kid`. Keys for other
/// uses or algorithms, and malformed keys, are skipped.
fn signing_key(jwk: &serde_json::Value) -> Option<(String, DecodingKey)> {
    let field = |name: &str| jwk.get(name).and_then(serde_json::Value::as_str);
    if field("kty") != Some("RSA") {
        return None;
    }
    if field("use").is_some_and(|value| value != "sig")
        || field("alg").is_some_and(|value| value != "RS256")
    {
        return None;
    }
    let kid = field("kid").filter(|kid| !kid.is_empty() && kid.len() <= MAX_KID_BYTES)?;
    let key = DecodingKey::from_rsa_components(field("n")?, field("e")?).ok()?;
    Some((kid.to_string(), key))
}

#[derive(Deserialize)]
struct Discovery {
    jwks_uri: String,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<serde_json::Value>,
}

/// The claims the gate reads beyond those `jsonwebtoken` validates.
#[derive(Deserialize)]
struct Claims {
    iss: String,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    sub: Option<String>,
}

/// Failed verifications per peer in a fixed window.
#[derive(Default)]
struct FailureLimiter {
    peers: HashMap<IpAddr, Window>,
    /// Peers without an address, and new peers once the table is full.
    shared: Window,
}

#[derive(Default, Clone, Copy)]
struct Window {
    started: Option<Instant>,
    failures: u32,
}

impl Window {
    fn current(&mut self, now: Instant, tuning: &Tuning) -> &mut Self {
        if self
            .started
            .is_none_or(|started| now.duration_since(started) >= tuning.failure_window)
        {
            *self = Self {
                started: Some(now),
                failures: 0,
            };
        }
        self
    }
}

impl FailureLimiter {
    /// Whether `peer` is over the limit in its current window.
    fn limited(&mut self, peer: Option<IpAddr>, now: Instant, tuning: &Tuning) -> bool {
        let tracked = peer.is_some_and(|peer| self.peers.contains_key(&peer));
        let window = match peer {
            Some(peer) if tracked => self.peers.entry(peer).or_default(),
            // An untracked peer with room in the table has no failures yet.
            Some(_) if self.peers.len() < MAX_TRACKED_PEERS => return false,
            _ => &mut self.shared,
        };
        window.current(now, tuning).failures >= tuning.failure_limit
    }

    /// Count one failed verification against `peer`.
    fn record(&mut self, peer: Option<IpAddr>, now: Instant, tuning: &Tuning) {
        if let Some(peer) = peer
            && !self.peers.contains_key(&peer)
            && self.peers.len() >= MAX_TRACKED_PEERS
        {
            self.peers.retain(|_, window| {
                window
                    .started
                    .is_some_and(|started| now.duration_since(started) < tuning.failure_window)
            });
        }
        let window = match peer {
            Some(peer)
                if self.peers.contains_key(&peer) || self.peers.len() < MAX_TRACKED_PEERS =>
            {
                self.peers.entry(peer).or_default()
            }
            _ => &mut self.shared,
        };
        let window = window.current(now, tuning);
        window.failures = window.failures.saturating_add(1);
    }
}

/// `GET /.well-known/oauth-protected-resource/mcp`, served without the gate.
pub(crate) async fn protected_resource_metadata(State(oauth): State<McpOAuth>) -> Response {
    Json(oauth.metadata().clone()).into_response()
}

/// A test issuer: an in-process HTTP server on loopback serving an OpenID
/// discovery document and a JWKS, and tokens signed with the committed
/// test-only key (`tests/fixtures/mcp-oauth`).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::response::IntoResponse as _;
    use base64::Engine as _;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde_json::{Value, json};

    use crate::config::McpOAuthConfig;

    pub(crate) const AUDIENCE: &str = "https://corpus.example.test:7264/mcp";
    pub(crate) const GROUP: &str = "bbox-users";
    pub(crate) const KID: &str = "test-key-1";

    const KEY_PEM: &str = include_str!("../../tests/fixtures/mcp-oauth/test-signing-key.pem");
    const PUBLIC_JWK: &str =
        include_str!("../../tests/fixtures/mcp-oauth/test-signing-key.jwk.json");

    pub(crate) struct TestIssuer {
        pub(crate) issuer: String,
        jwks: Arc<parking_lot::Mutex<Value>>,
        failing: Arc<AtomicBool>,
        discovery_hits: Arc<AtomicUsize>,
        jwks_hits: Arc<AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestIssuer {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// The public JWK of the test key under `kid`.
    pub(crate) fn public_jwk(kid: &str) -> Value {
        let mut jwk: Value = serde_json::from_str(PUBLIC_JWK).unwrap();
        jwk["kid"] = json!(kid);
        jwk
    }

    fn encoding_key() -> EncodingKey {
        let body: String = KEY_PEM
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        EncodingKey::from_rsa_der(&der)
    }

    pub(crate) fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    impl TestIssuer {
        /// Start an issuer whose JWKS publishes the test key under `kids`.
        pub(crate) async fn start(kids: &[&str]) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let issuer = format!("{base}/realms/test");
            let jwks = Arc::new(parking_lot::Mutex::new(json!({
                "keys": kids.iter().map(|kid| public_jwk(kid)).collect::<Vec<_>>(),
            })));
            let failing = Arc::new(AtomicBool::new(false));
            let discovery_hits = Arc::new(AtomicUsize::new(0));
            let jwks_hits = Arc::new(AtomicUsize::new(0));
            let jwks_uri = format!("{issuer}/protocol/openid-connect/certs");
            let app = axum::Router::new()
                .route(
                    "/realms/test/.well-known/openid-configuration",
                    axum::routing::get({
                        let hits = discovery_hits.clone();
                        let issuer = issuer.clone();
                        move || async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            axum::Json(json!({ "issuer": issuer, "jwks_uri": jwks_uri }))
                        }
                    }),
                )
                .route(
                    "/realms/test/protocol/openid-connect/certs",
                    axum::routing::get({
                        let hits = jwks_hits.clone();
                        let jwks = jwks.clone();
                        let failing = failing.clone();
                        move || async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            if failing.load(Ordering::SeqCst) {
                                return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                            }
                            axum::Json(jwks.lock().clone()).into_response()
                        }
                    }),
                );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                issuer,
                jwks,
                failing,
                discovery_hits,
                jwks_hits,
                server,
            }
        }

        pub(crate) fn config(&self) -> McpOAuthConfig {
            McpOAuthConfig {
                issuer: self.issuer.clone(),
                audience: AUDIENCE.to_string(),
                required_group: GROUP.to_string(),
                jwks_uri: None,
            }
        }

        pub(crate) fn jwks_uri(&self) -> String {
            format!("{}/protocol/openid-connect/certs", self.issuer)
        }

        pub(crate) fn publish(&self, kids: &[&str]) {
            *self.jwks.lock() = json!({
                "keys": kids.iter().map(|kid| public_jwk(kid)).collect::<Vec<_>>(),
            });
        }

        /// Make the key set endpoint answer 503 (or recover).
        pub(crate) fn fail(&self, failing: bool) {
            self.failing.store(failing, Ordering::SeqCst);
        }

        pub(crate) fn discovery_hits(&self) -> usize {
            self.discovery_hits.load(Ordering::SeqCst)
        }

        pub(crate) fn jwks_hits(&self) -> usize {
            self.jwks_hits.load(Ordering::SeqCst)
        }

        /// Claims a valid token for this issuer carries.
        pub(crate) fn claims(&self) -> Value {
            let now = now();
            json!({
                "iss": self.issuer,
                "aud": AUDIENCE,
                "exp": now + 300,
                "iat": now,
                "nbf": now,
                "groups": [GROUP, "other"],
                "email": "operator@example.test",
                "preferred_username": "operator",
                "sub": "0f1e2d3c-subject",
            })
        }

        /// A valid token for this issuer.
        pub(crate) fn token(&self) -> String {
            sign(KID, &self.claims())
        }
    }

    /// Sign `claims` with the test key as RS256 under `kid`.
    pub(crate) fn sign(kid: &str, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, claims, &encoding_key()).unwrap()
    }

    /// `claims` signed with the test key as RS256 with no `kid`.
    pub(crate) fn sign_without_kid(claims: &Value) -> String {
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), claims, &encoding_key()).unwrap()
    }

    /// `claims` signed with HS256 under a shared secret.
    pub(crate) fn sign_hs256(kid: &str, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, claims, &EncodingKey::from_secret(b"shared")).unwrap()
    }

    /// `claims` with `alg: none` and no signature.
    pub(crate) fn unsigned(kid: &str, claims: &Value) -> String {
        let encode = |value: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.",
            encode(&json!({ "alg": "none", "typ": "JWT", "kid": kid })),
            encode(claims)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use serde_json::json;

    fn verifier(issuer: &TestIssuer, tuning: Tuning) -> McpOAuth {
        verifier_for(&issuer.config(), tuning)
    }

    fn verifier_for(cfg: &McpOAuthConfig, tuning: Tuning) -> McpOAuth {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        McpOAuth::with_tuning(cfg, tuning, http).unwrap()
    }

    const PEER: Option<SocketAddr> = Some(SocketAddr::new(
        IpAddr::V4(std::net::Ipv4Addr::new(10, 42, 0, 9)),
        51000,
    ));

    fn with(mut claims: serde_json::Value, edits: serde_json::Value) -> serde_json::Value {
        for (key, value) in edits.as_object().unwrap() {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(key);
            } else {
                claims[key] = value.clone();
            }
        }
        claims
    }

    #[tokio::test]
    async fn a_valid_token_names_email_then_username_then_subject() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(&issuer, Tuning::default());
        let claims = issuer.claims();
        assert_eq!(
            oauth
                .verify(PEER, &sign(KID, &claims))
                .await
                .unwrap()
                .as_str(),
            "operator@example.test"
        );
        let claims = with(claims, json!({ "email": null }));
        assert_eq!(
            oauth
                .verify(PEER, &sign(KID, &claims))
                .await
                .unwrap()
                .as_str(),
            "operator"
        );
        let claims = with(claims, json!({ "preferred_username": "" }));
        assert_eq!(
            oauth
                .verify(PEER, &sign(KID, &claims))
                .await
                .unwrap()
                .as_str(),
            "0f1e2d3c-subject"
        );
        let claims = with(claims, json!({ "sub": null, "preferred_username": null }));
        assert_eq!(
            oauth.verify(PEER, &sign(KID, &claims)).await,
            Err(Refusal::NoPrincipal)
        );
        // An audience array containing the resource is accepted.
        let listed = with(issuer.claims(), json!({ "aud": ["account", AUDIENCE] }));
        assert!(oauth.verify(PEER, &sign(KID, &listed)).await.is_ok());
        // Discovery ran once and the key set was fetched once.
        assert_eq!((issuer.discovery_hits(), issuer.jwks_hits()), (1, 1));
    }

    #[tokio::test]
    async fn every_invalid_token_is_refused_with_its_category() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(&issuer, Tuning::default());
        let now = now();
        let claims = issuer.claims();
        let cases = [
            (
                sign(
                    KID,
                    &with(
                        claims.clone(),
                        json!({ "iss": "https://elsewhere.example.test/realms/test" }),
                    ),
                ),
                Refusal::Issuer,
            ),
            (
                sign(
                    KID,
                    &with(
                        claims.clone(),
                        json!({ "aud": "https://other.example.test/mcp" }),
                    ),
                ),
                Refusal::Audience,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "aud": ["account"] }))),
                Refusal::Audience,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "aud": null }))),
                Refusal::MissingClaim,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "groups": ["other"] }))),
                Refusal::Group,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "groups": null }))),
                Refusal::Group,
            ),
            (
                sign(
                    KID,
                    &with(claims.clone(), json!({ "groups": [format!("/{GROUP}")] })),
                ),
                Refusal::Group,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "exp": now - 120 }))),
                Refusal::Expired,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "exp": null }))),
                Refusal::MissingClaim,
            ),
            (
                sign(KID, &with(claims.clone(), json!({ "nbf": now + 120 }))),
                Refusal::NotYetValid,
            ),
            (sign_hs256(KID, &claims), Refusal::Algorithm),
            (unsigned(KID, &claims), Refusal::Malformed),
            (
                {
                    // A valid header and claims under a signature from
                    // another token.
                    let good = sign(KID, &claims);
                    let other = sign(KID, &with(claims.clone(), json!({ "sub": "x" })));
                    let (message, _) = good.rsplit_once('.').unwrap();
                    let (_, signature) = other.rsplit_once('.').unwrap();
                    format!("{message}.{signature}")
                },
                Refusal::Signature,
            ),
            ("not a token".to_string(), Refusal::Malformed),
            (sign_without_kid(&claims), Refusal::NoKeyId),
            ("x".repeat(MAX_TOKEN_BYTES + 1), Refusal::TooLong),
        ];
        for (token, expected) in cases {
            assert_eq!(
                oauth.verify(None, &token).await,
                Err(expected),
                "{expected}"
            );
        }
        // Inside the skew either way is admitted.
        let skewed = with(claims.clone(), json!({ "exp": now - 10, "nbf": now + 10 }));
        assert!(oauth.verify(PEER, &sign(KID, &skewed)).await.is_ok());
    }

    #[tokio::test]
    async fn an_unknown_kid_is_refused_after_one_bounded_refresh() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(&issuer, Tuning::default());
        let rotated = sign("test-key-2", &issuer.claims());
        assert_eq!(oauth.verify(PEER, &rotated).await, Err(Refusal::UnknownKey));
        assert_eq!(issuer.jwks_hits(), 1);
        // Within the refetch interval an unknown kid fetches nothing.
        for _ in 0..3 {
            assert_eq!(oauth.verify(PEER, &rotated).await, Err(Refusal::UnknownKey));
        }
        assert_eq!(issuer.jwks_hits(), 1);
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        assert_eq!(issuer.jwks_hits(), 1);
    }

    #[tokio::test]
    async fn a_rotated_key_is_found_by_refetching_an_unknown_kid() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(
            &issuer,
            Tuning {
                refetch_interval: Duration::ZERO,
                ..Tuning::default()
            },
        );
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        issuer.publish(&[KID, "test-key-2"]);
        let rotated = sign("test-key-2", &issuer.claims());
        assert!(oauth.verify(PEER, &rotated).await.is_ok());
        assert_eq!(issuer.jwks_hits(), 2);
        // Discovery is not repeated while its answer keeps working.
        assert_eq!(issuer.discovery_hits(), 1);
        // A known kid inside the TTL fetches nothing.
        assert!(oauth.verify(PEER, &rotated).await.is_ok());
        assert_eq!(issuer.jwks_hits(), 2);
    }

    #[tokio::test]
    async fn stale_keys_refresh_after_the_ttl_and_survive_a_failed_refresh() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(
            &issuer,
            Tuning {
                key_ttl: Duration::ZERO,
                refetch_interval: Duration::ZERO,
                ..Tuning::default()
            },
        );
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        assert_eq!(issuer.jwks_hits(), 2);
        // A failed refresh keeps the last good keys.
        issuer.fail(true);
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        assert_eq!(issuer.jwks_hits(), 3);
        // A successful refresh replaces them.
        issuer.fail(false);
        issuer.publish(&["test-key-2"]);
        assert_eq!(
            oauth.verify(PEER, &issuer.token()).await,
            Err(Refusal::UnknownKey)
        );
    }

    #[tokio::test]
    async fn a_configured_jwks_uri_skips_discovery() {
        let issuer = TestIssuer::start(&[KID]).await;
        let mut config = issuer.config();
        config.jwks_uri = Some(issuer.jwks_uri());
        let oauth = verifier_for(&config, Tuning::default());
        assert!(oauth.verify(PEER, &issuer.token()).await.is_ok());
        assert_eq!((issuer.discovery_hits(), issuer.jwks_hits()), (0, 1));
    }

    #[tokio::test]
    async fn failed_verifications_are_limited_per_peer_before_any_fetch() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(
            &issuer,
            Tuning {
                refetch_interval: Duration::ZERO,
                failure_limit: 3,
                ..Tuning::default()
            },
        );
        let unknown = sign("no-such-key", &issuer.claims());
        for _ in 0..3 {
            assert_eq!(oauth.verify(PEER, &unknown).await, Err(Refusal::UnknownKey));
        }
        let fetched = issuer.jwks_hits();
        assert_eq!(fetched, 3);
        // Over the limit the peer is refused without verification, valid
        // token or not, and nothing is fetched.
        for token in [unknown.clone(), issuer.token()] {
            assert_eq!(oauth.verify(PEER, &token).await, Err(Refusal::RateLimited));
        }
        assert_eq!(issuer.jwks_hits(), fetched);
        // Another peer is unaffected.
        let other: SocketAddr = "10.42.0.10:51000".parse().unwrap();
        assert!(oauth.verify(Some(other), &issuer.token()).await.is_ok());
    }

    #[test]
    fn the_failure_window_resets_and_a_full_table_shares_one_bucket() {
        let tuning = Tuning {
            failure_limit: 2,
            failure_window: Duration::from_secs(60),
            ..Tuning::default()
        };
        let start = Instant::now();
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        let mut limiter = FailureLimiter::default();
        assert!(!limiter.limited(Some(peer), start, &tuning));
        limiter.record(Some(peer), start, &tuning);
        limiter.record(Some(peer), start, &tuning);
        assert!(limiter.limited(Some(peer), start, &tuning));
        let later = start + Duration::from_secs(61);
        assert!(!limiter.limited(Some(peer), later, &tuning));

        for index in 0..MAX_TRACKED_PEERS as u32 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(0x0b00_0000 + index));
            limiter.record(Some(ip), later, &tuning);
        }
        assert_eq!(limiter.peers.len(), MAX_TRACKED_PEERS);
        let overflow: IpAddr = "12.0.0.1".parse().unwrap();
        limiter.record(Some(overflow), later, &tuning);
        limiter.record(None, later, &tuning);
        assert!(limiter.limited(Some(overflow), later, &tuning));
        assert!(limiter.limited(None, later, &tuning));
        assert_eq!(limiter.peers.len(), MAX_TRACKED_PEERS);
        // Expired windows make room again.
        let much_later = later + Duration::from_secs(61);
        limiter.record(Some(overflow), much_later, &tuning);
        assert!(limiter.peers.contains_key(&overflow));
        assert!(limiter.peers.len() < MAX_TRACKED_PEERS);
    }

    #[test]
    fn the_challenge_and_metadata_derive_from_the_configuration() {
        let config = McpOAuthConfig {
            issuer: "https://sso.example.test/realms/estate".into(),
            audience: AUDIENCE.into(),
            required_group: GROUP.into(),
            jwks_uri: None,
        };
        let oauth = verifier_for(&config, Tuning::default());
        assert_eq!(
            oauth.challenge(),
            "Bearer resource_metadata=\"https://corpus.example.test:7264/.well-known/oauth-protected-resource/mcp\", scope=\"openid profile email groups\""
        );
        assert_eq!(
            oauth.metadata(),
            &json!({
                "resource": AUDIENCE,
                "authorization_servers": ["https://sso.example.test/realms/estate"],
                "bearer_methods_supported": ["header"],
                "scopes_supported": ["openid", "profile", "email", "groups"],
                "resource_name": "blackboxd MCP",
            })
        );
        // A default port leaves the origin bare.
        let config = McpOAuthConfig {
            audience: "https://corpus.example.test/mcp".into(),
            ..config
        };
        assert!(
            verifier_for(&config, Tuning::default())
                .challenge()
                .to_str()
                .unwrap()
                .starts_with("Bearer resource_metadata=\"https://corpus.example.test/.well-known/")
        );
        for audience in ["corpus.example.test/mcp", "mailto:x@example.test"] {
            let config = McpOAuthConfig {
                audience: audience.into(),
                ..config.clone()
            };
            let http = reqwest::Client::new();
            assert!(McpOAuth::with_tuning(&config, Tuning::default(), http).is_err());
        }
    }

    #[tokio::test]
    async fn refusals_and_renderings_carry_no_token_material() {
        let issuer = TestIssuer::start(&[KID]).await;
        let oauth = verifier(&issuer, Tuning::default());
        let token = sign("no-such-key", &issuer.claims());
        let refusal = oauth.verify(PEER, &token).await.unwrap_err();
        let signature = token.rsplit_once('.').unwrap().1;
        for rendered in [
            refusal.to_string(),
            format!("{refusal:?}"),
            format!("{oauth:?}"),
        ] {
            assert!(!rendered.contains(signature), "{rendered}");
            assert!(!rendered.contains(&token[..32]), "{rendered}");
        }
    }

    #[tokio::test]
    async fn the_call_principal_is_scoped_to_its_call() {
        assert_eq!(call_principal(), None);
        let principal = McpPrincipal("operator@example.test".into());
        let seen = with_call_principal(Some(principal.clone()), async { call_principal() }).await;
        assert_eq!(seen, Some(principal));
        assert_eq!(call_principal(), None);
    }
}
