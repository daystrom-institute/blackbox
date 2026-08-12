//! Operator-gated mint for workspace binding tokens.
//!
//! # Why this exists
//!
//! A workspace binding is the capability that selects one exact provisional
//! workspace: the knowledge/gap/graph capture lane authenticates it
//! (`super::knowledge_source`), and an MCP session presenting it in the
//! `x-blackbox-workspace-binding` header gets `own` visibility over that
//! workspace's uncommitted state (`super::handler`). Until this route existed,
//! the only issuer was `DaemonWorkspaceBindingAuthority` during managed harness
//! worker spawn, so an operator standing at a checkout could not exercise any
//! provisional-lane behaviour against a dev daemon.
//!
//! # What this is not
//!
//! This is **operator authority, not agent self-service**. It is an HTTP route
//! on the daemon's admin plane and is deliberately absent from the MCP tool
//! catalog, matching the onboarding non-goal in
//! `design/daemon-runtime/remote-project-onboarding.md` (no agent self-service
//! registration, no MCP-triggered checkout authority). Its auth posture is the
//! same as every other `/admin/*` route: the daemon's loopback bind is the
//! trust boundary, so exposing the listener beyond loopback exposes this mint.
//!
//! # What the daemon can and cannot verify about the presented checkout
//!
//! The daemon **cannot** verify the presented checkout path, and deliberately
//! does not try. Catalog runtime code may reach a checkout root only through a
//! capability lease (the clause 2 ownership proof in
//! `super::catalog_ownership_scan`), and every lease kind that could resolve
//! one is closed for exactly the projects this mint serves: the knowledge
//! transport cutover shuts `PublisherConfigTreeRead` down, and lease
//! acquisition goes through it. So the path is operator-declared context. It is
//! logged and echoed back; it is never evidence.
//!
//! What IS verified, from durable catalog state alone (no checkout read, no
//! checkout write, no Git invocation, no path resolution):
//!
//! - that the claimed published scope is a scope the daemon's project authority
//!   actually knows, and that it resolves to exactly one catalog project;
//! - that the project has a live `Attached` attachment whose `validated_scope`
//!   is the scope being claimed;
//! - that the workspace identity the caller presents is the `checkout_id` that
//!   attachment records.
//!
//! The workspace identity, not the path, is what a binding actually binds: the
//! provisional store keys every generation by `workspace_id`, and a binding
//! carrying an identity the catalog never recorded can select nothing. That is
//! the same posture as the managed path, which trusts the worker host's
//! reported `WorkerWorkspaceIdentity` and validates only the scope against
//! catalog authority.
//!
//! Not verifiable, and refused rather than guessed:
//!
//! - anything about a project with no live local attachment (a remote or
//!   catalog-only project). There is no recorded checkout identity to bind, so
//!   the mint refuses instead of accepting the caller's identity on trust.
//! - that the directory on disk at the declared path is the checkout that
//!   identity belongs to. The caller reads the identity marker out of the
//!   checkout it is standing in (`bro workspace-binding mint` does exactly
//!   that), and the daemon confirms the catalog agrees that identity exists.
//!
//! The mint itself is not forked: it calls
//! `super::knowledge_source::mint_workspace_binding`, the same function the
//! managed spawn path calls, so the token shape (64 lowercase hex), the grant
//! fields, the fixed TTL, the replacement semantics, and the provisional-lease
//! renewal loop are identical.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bbox_code_source::ErrorResponse;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::AttachmentStatus;
use bro_core::WorkspaceId;
use serde::{Deserialize, Serialize};

use super::SharedState;

/// Prefix for the synthetic task/session id an operator lease carries. The
/// managed path keys bindings by `(task_id, session_id)`; an operator lease has
/// neither, so it keys by workspace identity under a reserved prefix. Minting
/// twice for one checkout therefore replaces the earlier binding rather than
/// accumulating live tokens.
const OPERATOR_LEASE_PREFIX: &str = "operator-workspace-binding";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MintWorkspaceBindingRequest {
    /// Absolute path of the checkout root the operator is standing in.
    pub checkout_path: String,
    /// The published scope that checkout claims.
    pub scope: MintScope,
    /// Workspace identity the caller read from the checkout it is standing in
    /// (`.bbox/local/checkout-id`). The daemon requires the catalog to record
    /// this identity for a live attachment of the claimed scope. This, not the
    /// path, is what the binding binds.
    pub workspace_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MintScope {
    pub repo_id: String,
    pub bbox_root_relpath: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct MintWorkspaceBindingResponse {
    pub status: &'static str,
    /// The binding secret, returned exactly once and never persisted daemon
    /// side in recoverable form.
    pub token: String,
    pub project_id: String,
    pub scope: PublishedScope,
    pub workspace_id: String,
    /// Echoed back exactly as declared. The daemon did not resolve or verify
    /// it; see the module documentation.
    pub declared_checkout_path: String,
    pub attachment_id: String,
    pub lease_id: String,
    pub expires_unix_secs: u64,
    /// Whether the provisional capture lane will currently accept this binding.
    /// A binding minted while the knowledge transport is disabled still grants
    /// `own` visibility but cannot capture.
    pub provisional_capture_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MintRefusal {
    ScopeInvalid,
    CheckoutPathInvalid,
    CatalogUnavailable,
    ScopeUnknown,
    ScopeAmbiguous,
    AttachmentUnknown,
    WorkspaceIdentityMismatch,
    WorkspaceIdentityInvalid,
    MintFailed,
}

impl MintRefusal {
    fn code(self) -> &'static str {
        match self {
            Self::ScopeInvalid => "error.workspace_binding_scope_invalid",
            Self::CheckoutPathInvalid => "error.workspace_binding_checkout_path_invalid",
            Self::CatalogUnavailable => "error.workspace_binding_catalog_unavailable",
            Self::ScopeUnknown => "error.workspace_binding_scope_unknown",
            Self::ScopeAmbiguous => "error.workspace_binding_scope_ambiguous",
            Self::AttachmentUnknown => "error.workspace_binding_attachment_unknown",
            Self::WorkspaceIdentityMismatch => "error.workspace_binding_workspace_id_mismatch",
            Self::WorkspaceIdentityInvalid => "error.workspace_binding_workspace_id_invalid",
            Self::MintFailed => "error.workspace_binding_mint_failed",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::ScopeInvalid | Self::CheckoutPathInvalid | Self::WorkspaceIdentityInvalid => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::ScopeUnknown => StatusCode::NOT_FOUND,
            Self::WorkspaceIdentityMismatch => StatusCode::FORBIDDEN,
            Self::ScopeAmbiguous | Self::AttachmentUnknown | Self::CatalogUnavailable => {
                StatusCode::CONFLICT
            }
            Self::MintFailed => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::ScopeInvalid => "published scope is not a valid scope",
            Self::CheckoutPathInvalid => "declared checkout path must be a confined absolute path",
            Self::CatalogUnavailable => {
                "workspace binding mint requires the project catalog authority"
            }
            Self::ScopeUnknown => "published scope is not registered with this daemon",
            Self::ScopeAmbiguous => "published scope resolves to more than one catalog project",
            Self::AttachmentUnknown => {
                "project has no live local checkout attachment, so there is no recorded workspace \
                 identity to bind"
            }
            Self::WorkspaceIdentityMismatch => {
                "no live attachment for the claimed scope records this workspace identity"
            }
            Self::WorkspaceIdentityInvalid => "workspace identity is malformed",
            Self::MintFailed => "workspace binding could not be minted",
        }
    }
}

impl IntoResponse for MintRefusal {
    fn into_response(self) -> Response {
        (
            self.status(),
            axum::Json(ErrorResponse {
                code: self.code().to_string(),
                message: self.message().to_string(),
            }),
        )
            .into_response()
    }
}

/// `POST /admin/workspace-binding/mint`.
///
/// Operator authority: the loopback bind is the gate, exactly as for every
/// other `/admin/*` route. Never expose this as an MCP tool.
pub(crate) async fn admin_workspace_binding_mint(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(request): axum::Json<MintWorkspaceBindingRequest>,
) -> Response {
    // The catalog snapshot read is blocking, and the mint spawns its renewal
    // task, which needs the runtime handle a blocking pool thread still carries.
    match tokio::task::spawn_blocking(move || mint_operator_workspace_binding(&state, &request))
        .await
    {
        Ok(Ok(response)) => axum::Json(response).into_response(),
        Ok(Err(refusal)) => refusal.into_response(),
        Err(_) => MintRefusal::MintFailed.into_response(),
    }
}

pub(crate) fn mint_operator_workspace_binding(
    state: &Arc<SharedState>,
    request: &MintWorkspaceBindingRequest,
) -> Result<MintWorkspaceBindingResponse, MintRefusal> {
    let scope = PublishedScope::try_new(
        request.scope.repo_id.clone(),
        request.scope.bbox_root_relpath.clone(),
    )
    .map_err(|_| MintRefusal::ScopeInvalid)?;
    // Lexical hygiene only. The daemon never resolves this path: doing so would
    // reach a checkout root outside a capability lease, which the clause 2
    // ownership proof forbids for catalog runtime code.
    if !confined_absolute_path(&request.checkout_path) {
        return Err(MintRefusal::CheckoutPathInvalid);
    }
    let workspace_id = WorkspaceId::parse(request.workspace_id.clone())
        .map_err(|_| MintRefusal::WorkspaceIdentityInvalid)?;

    let project_id = super::knowledge_source::project_id_for_scope(state, &scope)
        .map_err(|_| MintRefusal::ScopeAmbiguous)?
        .ok_or(MintRefusal::ScopeUnknown)?;

    let store = state
        .project_authority
        .catalog_store()
        .ok_or(MintRefusal::CatalogUnavailable)?;
    let snapshot = store
        .snapshot()
        .map_err(|_| MintRefusal::CatalogUnavailable)?;

    let mut live_attachments = 0_usize;
    let mut attachment_id = None;
    for (candidate_id, attachment) in &snapshot.attachments().attachments {
        if attachment.project_id.as_str() != project_id
            || attachment.status != AttachmentStatus::Attached
        {
            continue;
        }
        live_attachments += 1;
        if attachment.validated_scope.as_ref() != Some(&scope)
            || attachment.checkout_id != workspace_id.as_str()
        {
            continue;
        }
        // Two live attachments of one scope sharing a checkout identity would
        // be a catalog integrity failure, not an operator input error; take the
        // first and stay deterministic through the BTreeMap ordering.
        attachment_id.get_or_insert_with(|| candidate_id.as_str().to_string());
    }
    let attachment_id = match attachment_id {
        Some(attachment_id) => attachment_id,
        None if live_attachments == 0 => return Err(MintRefusal::AttachmentUnknown),
        None => return Err(MintRefusal::WorkspaceIdentityMismatch),
    };

    let identity = bro_protocol::WorkerWorkspaceIdentity {
        workspace_id: workspace_id.clone(),
        scope: bro_protocol::WorkerWorkspaceScope::try_new(
            scope.repo_id().to_string(),
            scope.bbox_root_relpath().to_string(),
        )
        .map_err(|_| MintRefusal::ScopeInvalid)?,
    };
    let lease_id = format!("{OPERATOR_LEASE_PREFIX}:{workspace_id}");
    let minted =
        super::knowledge_source::mint_workspace_binding(state, &lease_id, &lease_id, &identity)
            .map_err(|error| {
                tracing::warn!(
                    project_id = %project_id,
                    error = %error,
                    "operator workspace binding mint failed"
                );
                MintRefusal::MintFailed
            })?;
    let expires_unix_secs = state
        .knowledge_sources
        .authenticate_workspace_binding_now(minted.token.expose_secret())
        .map(|grant| grant.expires_unix_secs)
        .ok_or(MintRefusal::MintFailed)?;

    tracing::info!(
        project_id = %project_id,
        workspace_id = %workspace_id,
        attachment_id = %attachment_id,
        lease_id = %lease_id,
        declared_checkout_path = %request.checkout_path,
        "minted operator workspace binding"
    );

    Ok(MintWorkspaceBindingResponse {
        status: "minted",
        token: minted.token.expose_secret().to_string(),
        project_id,
        scope,
        workspace_id: workspace_id.as_str().to_string(),
        declared_checkout_path: request.checkout_path.clone(),
        attachment_id,
        lease_id,
        expires_unix_secs,
        provisional_capture_enabled: state
            .code_sources
            .producer_auth()
            .knowledge_transport_enabled(),
    })
}

/// Purely lexical: absolute, non-empty, no `..` traversal, no NUL. This never
/// touches the filesystem, so it makes no claim that the path exists or is the
/// checkout it says it is.
fn confined_absolute_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !value.contains('\0')
        && path.is_absolute()
        && path
            .components()
            .all(|component| component != std::path::Component::ParentDir)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use bbox_corpus_core::project_catalog::CatalogSnapshotV2;
    use bro_rpc::ServiceToken;
    use tower::ServiceExt;

    use super::*;
    use crate::server::BlackboxServer;
    use crate::server::producer_auth::{ProducerAuthRuntime, ProducerGrant};
    use crate::server::state::catalog_fixture::{CatalogFixture, gap_note, knowledge_entry};

    const PROJECT: &str = "p_00000000000000000000000000000901";
    const ATTACHMENT: &str = "att_00000000000000000000000000000901";
    const CHECKOUT_ID: &str = "0123456789abcdef0123456789abcdef";

    fn app_for(state: &Arc<SharedState>) -> axum::Router {
        let config = state.config.read().clone();
        crate::server::mcp::build_http_app(
            state.clone(),
            &config,
            &tokio_util::sync::CancellationToken::new(),
        )
    }

    fn mint_request(checkout_path: &str, scope: &PublishedScope) -> serde_json::Value {
        mint_request_as(checkout_path, scope, CHECKOUT_ID)
    }

    fn mint_request_as(
        checkout_path: &str,
        scope: &PublishedScope,
        workspace_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "checkout_path": checkout_path,
            "scope": {
                "repo_id": scope.repo_id(),
                "bbox_root_relpath": scope.bbox_root_relpath(),
            },
            "workspace_id": workspace_id,
        })
    }

    async fn post_mint(
        state: &Arc<SharedState>,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app_for(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/workspace-binding/mint")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// One published project with one live attachment whose recorded checkout
    /// directory is a real (empty) directory on disk.
    fn attached_fixture() -> (CatalogFixture, PublishedScope, PathBuf) {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let checkout = fixture.root().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let checkout = checkout.canonicalize().unwrap();
        fixture.attach_overlay_checkout(PROJECT, &scope, &checkout, ATTACHMENT, CHECKOUT_ID, true);
        (fixture, scope, checkout)
    }

    #[tokio::test]
    async fn mint_refuses_a_scope_this_daemon_does_not_know() {
        let (fixture, _scope, checkout) = attached_fixture();
        let state = fixture.server().state.clone();
        let stranger = PublishedScope::try_new("repo_not_registered", ".").unwrap();
        let (status, body) =
            post_mint(&state, mint_request(&checkout.to_string_lossy(), &stranger)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "error.workspace_binding_scope_unknown");
    }

    #[tokio::test]
    async fn mint_refuses_a_project_with_no_live_local_attachment() {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        fixture.add_published_project(PROJECT, &scope);
        let checkout = fixture.root().join("unattached");
        std::fs::create_dir_all(&checkout).unwrap();
        let state = fixture.server().state.clone();

        let (status, body) = post_mint(
            &state,
            mint_request(&checkout.canonicalize().unwrap().to_string_lossy(), &scope),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "error.workspace_binding_attachment_unknown");
    }

    #[tokio::test]
    async fn mint_refuses_a_detached_attachment() {
        let (fixture, scope, checkout) = attached_fixture();
        fixture.detach(ATTACHMENT);
        let state = fixture.server().state.clone();

        let (status, body) =
            post_mint(&state, mint_request(&checkout.to_string_lossy(), &scope)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "error.workspace_binding_attachment_unknown");
    }

    #[tokio::test]
    async fn mint_refuses_a_workspace_identity_the_catalog_does_not_record() {
        let (fixture, scope, checkout) = attached_fixture();
        let state = fixture.server().state.clone();
        let body = mint_request_as(
            &checkout.to_string_lossy(),
            &scope,
            "fedcba9876543210fedcba9876543210",
        );

        let (status, body) = post_mint(&state, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["code"],
            "error.workspace_binding_workspace_id_mismatch"
        );
    }

    #[tokio::test]
    async fn mint_refuses_an_unconfined_declared_checkout_path() {
        let (fixture, scope, _checkout) = attached_fixture();
        let state = fixture.server().state.clone();
        for declared in ["checkout", "/repo/../escape", ""] {
            let (status, body) = post_mint(&state, mint_request(declared, &scope)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{declared}");
            assert_eq!(
                body["code"],
                "error.workspace_binding_checkout_path_invalid"
            );
        }
    }

    /// The declared path is context, not evidence. A wrong path with a right
    /// identity still mints, and the response name says the daemon only echoed
    /// what the operator declared.
    #[tokio::test]
    async fn mint_treats_the_declared_checkout_path_as_unverified_context() {
        let (fixture, scope, _checkout) = attached_fixture();
        let state = fixture.server().state.clone();
        let (status, body) = post_mint(&state, mint_request("/not/the/checkout", &scope)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["declared_checkout_path"], "/not/the/checkout");
        assert!(body.get("checkout_path").is_none());
    }

    /// The minted binding must be indistinguishable from a managed one: same
    /// token shape, same grant fields, authenticating through the same
    /// provisional-lane entry point.
    #[tokio::test]
    async fn minted_binding_authenticates_exactly_like_a_managed_binding() {
        let (fixture, scope, checkout) = attached_fixture();
        let state = fixture.server().state.clone();

        let (status, body) =
            post_mint(&state, mint_request(&checkout.to_string_lossy(), &scope)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 64);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert!(
            bro_protocol::WorkspaceBindingToken::parse(token.clone()).is_ok(),
            "operator mint must produce the managed token shape"
        );
        assert_eq!(body["project_id"], PROJECT);
        assert_eq!(body["workspace_id"], CHECKOUT_ID);
        assert_eq!(body["attachment_id"], ATTACHMENT);
        assert_eq!(
            body["lease_id"],
            format!("operator-workspace-binding:{CHECKOUT_ID}")
        );

        let grant = state
            .knowledge_sources
            .authenticate_workspace_binding_now(&token)
            .expect("minted binding authenticates the provisional lane");
        assert_eq!(grant.project_id, PROJECT);
        assert_eq!(grant.scope, scope);
        assert_eq!(grant.workspace_id.as_str(), CHECKOUT_ID);
        assert!(grant.is_live_now());

        // Re-minting for the same checkout replaces rather than accumulates,
        // matching the managed replacement rule keyed on task/session id.
        let (status, second) =
            post_mint(&state, mint_request(&checkout.to_string_lossy(), &scope)).await;
        assert_eq!(status, StatusCode::OK);
        let replacement = second["token"].as_str().unwrap();
        assert_ne!(replacement, token);
        assert!(
            state
                .knowledge_sources
                .authenticate_workspace_binding_now(&token)
                .is_none(),
            "the superseded operator binding must stop authenticating"
        );
        assert!(
            state
                .knowledge_sources
                .authenticate_workspace_binding_now(replacement)
                .is_some()
        );
    }

    /// Operator authority, never agent self-service: no MCP tool may reach the
    /// mint (design/daemon-runtime/remote-project-onboarding.md non-goals).
    #[test]
    fn workspace_binding_mint_is_absent_from_the_mcp_tool_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let server = crate::server::BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let names = server
            .tool_router
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(!names.is_empty(), "the tool catalog must not be empty");
        for name in &names {
            let name = name.to_ascii_lowercase();
            assert!(
                !(name.contains("workspace_binding") || name.contains("workspace-binding")),
                "workspace binding minting must stay off the MCP surface: {name}"
            );
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git is available");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git is available");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// A governance-record vertex that exists ONLY in the working tree, so a
    /// bound session seeing it proves the provisional graph lane and not a
    /// published read.
    const UNCOMMITTED_VERTEX: &str = r#"{"id":"record/case@3","type":"gov:Record","label":"Uncommitted case record version 3","properties":{"status":"draft","version":3,"summary":"Uncommitted governance record"}}"#;

    const GRAPH_SOURCES: [(&str, &[u8]); 4] = [
        (
            "schema.json",
            include_bytes!(
                "../../crates/bbox-project-graph/tests/fixtures/governance-record/schema.json"
            )
            .as_slice(),
        ),
        (
            "vertices.jsonl",
            include_bytes!(
                "../../crates/bbox-project-graph/tests/fixtures/governance-record/vertices.jsonl"
            )
            .as_slice(),
        ),
        (
            "edges.jsonl",
            include_bytes!(
                "../../crates/bbox-project-graph/tests/fixtures/governance-record/edges.jsonl"
            )
            .as_slice(),
        ),
        (
            "graph.json",
            include_bytes!(
                "../../crates/bbox-project-graph/tests/fixtures/governance-record/graph.json"
            )
            .as_slice(),
        ),
    ];

    /// One published project whose accepted content is committed in a real
    /// checkout, attached, with producer auth carrying knowledge transport and
    /// the daemon serving on loopback. The working tree is left clean: each
    /// test writes its own uncommitted edits before minting.
    struct LiveCapture {
        /// Owns the tempdir the checkout and every store live in; dropping it
        /// early would delete the fixture out from under the serving daemon.
        #[allow(dead_code)]
        fixture: CatalogFixture,
        scope: PublishedScope,
        checkout: PathBuf,
        server: BlackboxServer,
        base_url: String,
        serving: tokio::task::JoinHandle<()>,
    }

    async fn live_capture_fixture() -> LiveCapture {
        let fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let checkout = fixture.root().join("live-checkout");
        std::fs::create_dir_all(checkout.join(".bbox/knowledge")).unwrap();
        std::fs::create_dir_all(checkout.join(".bbox/gaps")).unwrap();
        std::fs::create_dir_all(checkout.join(".bbox/graphs/governance-record")).unwrap();
        let checkout = checkout.canonicalize().unwrap();

        let published_knowledge = knowledge_entry("k-operator-mint", "published content");
        let published_gap = gap_note("gap-0000ab01", "published title");
        std::fs::write(
            checkout.join(".bbox/knowledge/k-operator-mint.json"),
            bbox_knowledge::knowledge::committed_knowledge_entry_bytes(&published_knowledge)
                .unwrap(),
        )
        .unwrap();
        std::fs::write(
            checkout.join(".bbox/gaps/gap-0000ab01.json"),
            bbox_gaps::gaps::committed_gap_note_bytes(&published_gap).unwrap(),
        )
        .unwrap();
        for (name, bytes) in GRAPH_SOURCES {
            std::fs::write(
                checkout.join(".bbox/graphs/governance-record").join(name),
                bytes,
            )
            .unwrap();
        }
        git(&checkout, &["init", "-q", "-b", "main"]);
        git(&checkout, &["config", "user.email", "test@example.com"]);
        git(&checkout, &["config", "user.name", "Test"]);
        git(&checkout, &["config", "commit.gpgsign", "false"]);
        git(&checkout, &["add", ".bbox"]);
        git(&checkout, &["commit", "-q", "-m", "accepted content"]);
        let accepted_commit = git_stdout(&checkout, &["rev-parse", "HEAD"]);

        fixture.add_published_project(PROJECT, &scope);
        fixture.attach_overlay_checkout(PROJECT, &scope, &checkout, ATTACHMENT, CHECKOUT_ID, true);
        fixture.install_publication(
            PROJECT,
            &scope,
            &accepted_commit,
            &[published_knowledge],
            &[published_gap],
        );

        let server = fixture.server();
        let state = server.state.clone();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        for (project_id, project) in &fixture
            .store()
            .snapshot()
            .unwrap()
            .catalog()
            .projects
            .clone()
        {
            catalog.projects.insert(project_id.clone(), project.clone());
        }
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    ServiceToken::parse("1".repeat(64)).unwrap(),
                    ProducerGrant {
                        producer_id: "operator-mint-producer".to_string(),
                        projects: BTreeMap::from([(scope.clone(), PROJECT.to_string())]),
                    },
                )],
                &catalog,
            )));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let serving = tokio::spawn({
            let app = app_for(&state);
            async move { axum::serve(listener, app).await.unwrap() }
        });

        LiveCapture {
            fixture,
            scope,
            checkout,
            server,
            base_url: format!("http://{address}"),
            serving,
        }
    }

    async fn mint_over_http(live: &LiveCapture) -> String {
        let minted: serde_json::Value = reqwest::Client::new()
            .post(format!("{}/admin/workspace-binding/mint", live.base_url))
            .json(&mint_request(&live.checkout.to_string_lossy(), &live.scope))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = minted["token"].as_str().expect("minted token").to_string();
        assert_eq!(minted["workspace_id"], CHECKOUT_ID);
        assert_eq!(minted["provisional_capture_enabled"], true);
        token
    }

    async fn capture_workspace(
        live: &LiveCapture,
        token: &str,
    ) -> bbox_knowledge_source_client::CaptureOutcome {
        bbox_knowledge_source_client::WorkspaceCaptureClient::new(
            &live.base_url,
            bro_protocol::WorkspaceBindingToken::parse(token.to_string()).unwrap(),
            live.checkout.clone(),
            live.checkout.clone(),
            WorkspaceId::parse(CHECKOUT_ID.to_string()).unwrap(),
            live.scope.clone(),
        )
        .unwrap()
        .sync_once()
        .await
        .expect("provisional capture")
    }

    fn append_working_vertex(checkout: &Path, row: &str) {
        let vertices = checkout.join(".bbox/graphs/governance-record/vertices.jsonl");
        std::fs::write(
            &vertices,
            format!("{}{row}\n", std::fs::read_to_string(&vertices).unwrap()),
        )
        .unwrap();
    }

    /// End to end: an operator mints a binding through the route, the real
    /// capture client uses it to publish a provisional workspace, and an MCP
    /// session presenting the same binding reads its own uncommitted state
    /// through the knowledge, gap, and project-graph read services.
    #[tokio::test(flavor = "multi_thread")]
    async fn minted_binding_drives_a_real_capture_and_own_visibility() {
        let live = live_capture_fixture().await;
        let checkout = live.checkout.clone();

        // The uncommitted working change is exactly what `own` visibility must
        // surface and `published` must not.
        std::fs::write(
            checkout.join(".bbox/knowledge/k-operator-mint.json"),
            bbox_knowledge::knowledge::committed_knowledge_entry_bytes(&knowledge_entry(
                "k-operator-mint",
                "uncommitted workspace content",
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            checkout.join(".bbox/gaps/gap-0000ab01.json"),
            bbox_gaps::gaps::committed_gap_note_bytes(&gap_note(
                "gap-0000ab01",
                "uncommitted workspace title",
            ))
            .unwrap(),
        )
        .unwrap();
        append_working_vertex(&checkout, UNCOMMITTED_VERTEX);

        let server = live.server.clone();
        let state = server.state.clone();
        let serving = &live.serving;
        let token = mint_over_http(&live).await;
        let outcome = capture_workspace(&live, &token).await;
        assert!(!outcome.reused);

        // Present the same minted binding as an MCP session authority.
        let grant = state
            .knowledge_sources
            .authenticate_workspace_binding_now(&token)
            .expect("minted binding authenticates");
        assert!(
            server
                .session_workspace_binding
                .set(Some(Arc::new(grant)))
                .is_ok()
        );

        let own_knowledge = server.session_knowledge_view(None, Some("own")).unwrap();
        let row = own_knowledge
            .items
            .iter()
            .find(|item| item.entry.id == "k-operator-mint")
            .expect("own visibility surfaces the captured entry");
        assert_eq!(row.entry.content, "uncommitted workspace content");
        assert_eq!(row.entry.project_id.as_deref(), Some(PROJECT));

        let own_gaps = server.session_gap_view(None, Some("own")).unwrap();
        let gap = own_gaps
            .gaps
            .all()
            .iter()
            .find(|gap| gap.id == "gap-0000ab01")
            .expect("own visibility surfaces the captured gap");
        assert_eq!(gap.title, "uncommitted workspace title");

        let own_graphs = server
            .project_graph_list_domain(None, Some("own"))
            .expect("own visibility reaches the project graph read service");
        assert!(
            own_graphs
                .iter()
                .any(|graph| graph.graph_id == "governance-record"),
            "captured project graph must be visible to the bound session: {own_graphs:?}"
        );
        let uncommitted_vertex = bbox_corpus_core::entity_ref::EntityRef::parse(&format!(
            "project_graph_vertex:{PROJECT}:governance-record:record/case@3"
        ))
        .unwrap();
        let resolved = server
            .resolve_project_graph_vertex(&uncommitted_vertex, Some("own"))
            .expect("uncommitted graph vertex resolves for the bound session");
        assert!(resolved.provisional);
        assert_eq!(
            resolved.checkout_id.as_ref().map(|id| id.as_str()),
            Some(CHECKOUT_ID)
        );
        assert!(
            server
                .resolve_project_graph_vertex(&uncommitted_vertex, Some("published"))
                .is_err(),
            "the uncommitted vertex must not leak into the published lane"
        );

        let published_knowledge_view = server
            .session_knowledge_view(None, Some("published"))
            .unwrap();
        assert!(
            published_knowledge_view.items.iter().any(|item| {
                item.entry.id == "k-operator-mint" && item.entry.content == "published content"
            }),
            "the published lane must still serve accepted content"
        );

        serving.abort();
    }

    /// A second working vertex, so the "captured again" leg reads a generation
    /// that is newer than the one the first capture installed.
    const SECOND_UNCOMMITTED_VERTEX: &str = r#"{"id":"record/case@5","type":"gov:Record","label":"Uncommitted case record version 5","properties":{"status":"draft","version":5,"summary":"Second uncommitted governance record"}}"#;

    /// A finalized capture installs its own graph views at the finalize
    /// chokepoint, the mirror of what advance does for the published lane.
    ///
    /// Cold is the whole point: this session never reads knowledge or gaps, so
    /// nothing recomputes the overlay pair. Before the finalize-time refresh,
    /// the first read answered from the published lane and the read after a
    /// second capture answered from the first capture's generation.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_finalized_capture_refreshes_own_graph_views_without_a_knowledge_read() {
        let live = live_capture_fixture().await;
        let checkout = live.checkout.clone();
        let server = live.server.clone();
        let state = server.state.clone();

        append_working_vertex(&checkout, UNCOMMITTED_VERTEX);
        let token = mint_over_http(&live).await;
        let first = capture_workspace(&live, &token).await;
        assert!(!first.reused);

        let grant = state
            .knowledge_sources
            .authenticate_workspace_binding_now(&token)
            .expect("minted binding authenticates");
        assert!(
            server
                .session_workspace_binding
                .set(Some(Arc::new(grant)))
                .is_ok()
        );

        let cold = server
            .project_graph_list_domain(None, Some("own"))
            .expect("own visibility reaches the project graph read service");
        let row = cold
            .iter()
            .find(|graph| graph.graph_id == "governance-record")
            .expect("the captured graph is visible without a knowledge read");
        assert_eq!(row.source, "provisional", "{cold:?}");
        assert_eq!(row.checkout_id.as_deref(), Some(CHECKOUT_ID), "{cold:?}");
        let first_hash = row.content_hash.clone();

        let first_vertex = bbox_corpus_core::entity_ref::EntityRef::parse(&format!(
            "project_graph_vertex:{PROJECT}:governance-record:record/case@3"
        ))
        .unwrap();
        let resolved = server
            .resolve_project_graph_vertex(&first_vertex, Some("own"))
            .expect("the first capture's vertex resolves on a cold read");
        assert!(resolved.provisional);

        // Capture again. A cold read must move to the newer generation rather
        // than keep serving the one the first capture installed.
        append_working_vertex(&checkout, SECOND_UNCOMMITTED_VERTEX);
        let second = capture_workspace(&live, &token).await;
        assert!(!second.reused);
        assert_ne!(second.source_generation_id, first.source_generation_id);

        let colder = server
            .project_graph_list_domain(None, Some("own"))
            .expect("own visibility still reaches the read service");
        let row = colder
            .iter()
            .find(|graph| graph.graph_id == "governance-record")
            .expect("the second captured graph is visible without a knowledge read");
        assert_eq!(row.source, "provisional", "{colder:?}");
        assert_ne!(
            row.content_hash, first_hash,
            "a cold own read must serve the newest captured generation: {colder:?}"
        );

        let second_vertex = bbox_corpus_core::entity_ref::EntityRef::parse(&format!(
            "project_graph_vertex:{PROJECT}:governance-record:record/case@5"
        ))
        .unwrap();
        let resolved = server
            .resolve_project_graph_vertex(&second_vertex, Some("own"))
            .expect("the second capture's vertex resolves on a cold read");
        assert!(resolved.provisional);
        assert_eq!(
            resolved.checkout_id.as_ref().map(|id| id.as_str()),
            Some(CHECKOUT_ID)
        );

        live.serving.abort();
    }
}
