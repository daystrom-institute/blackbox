use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::hub::SystemEventDraft;
use super::identity::ExternalIdentity;
use super::types::{ActionOutcome, ActionStatus, SystemEvent, SystemEventKind};
use crate::server::state::SharedState;
use blackbox::secrets::{SecretSources, resolve_with_sources, write_file_secret};

const BUILTIN_REF: &str = "forgejo-ensure-user@v1";
const BUILTIN_REF_FULL: &str = "atom:forgejo-ensure-user@v1";
const TOKEN_NAME: &str = "blackbox-bro";

pub(super) fn is_builtin_atom(atom: &str) -> bool {
    atom == BUILTIN_REF || atom == BUILTIN_REF_FULL
}

struct ForgejoArgs {
    instance: String,
    bro: String,
    provider: String,
    model: String,
    username: String,
    display_name: String,
    email: String,
    token_secret_name: String,
    admin_token_ref: String,
    base_url: Option<String>,
    base_url_env: Option<String>,
}

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("forgejo-ensure-user: missing required arg '{key}'"))
}

fn parse_args(args: &Value) -> Result<ForgejoArgs> {
    Ok(ForgejoArgs {
        instance: req_str(args, "instance")?,
        bro: req_str(args, "bro")?,
        provider: req_str(args, "provider")?,
        model: req_str(args, "model")?,
        username: req_str(args, "username")?,
        display_name: req_str(args, "display_name")?,
        email: req_str(args, "email")?,
        token_secret_name: req_str(args, "token_secret_name")?,
        admin_token_ref: req_str(args, "admin_token_ref")?,
        base_url: args
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        base_url_env: args
            .get("base_url_env")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn resolve_base_url(args: &ForgejoArgs) -> Result<String> {
    if let Some(ref url) = args.base_url {
        if !url.is_empty() {
            return Ok(url.trim_end_matches('/').to_string());
        }
    }
    if let Some(ref env_name) = args.base_url_env {
        if !env_name.is_empty() {
            let val = std::env::var(env_name)
                .with_context(|| format!("base_url_env '{}' not set", env_name))?;
            if !val.is_empty() {
                return Ok(val.trim_end_matches('/').to_string());
            }
        }
    }
    bail!("forgejo-ensure-user: base_url or base_url_env required")
}

fn resolve_admin_token(admin_token_ref: &str, sources: &SecretSources) -> Result<String> {
    let name = admin_token_ref.strip_prefix("secret:").ok_or_else(|| {
        anyhow::anyhow!("admin_token_ref must use 'secret:' prefix, got '{admin_token_ref}'")
    })?;
    Ok(resolve_with_sources(name, sources.clone())
        .with_context(|| format!("resolving admin token '{name}'"))?
        .expose()
        .to_string())
}

fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

async fn http_get_json(url: &str, auth_token: &str) -> Result<(u16, Value)> {
    let client = build_client()?;
    let resp = client
        .get(url)
        .header("Authorization", format!("token {auth_token}"))
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .with_context(|| format!("reading body GET {url}"))?;
    let value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body).unwrap_or(Value::String(body))
    };
    Ok((status, value))
}

async fn http_post_json(url: &str, auth_token: &str, body: &Value) -> Result<(u16, Value)> {
    let client = build_client()?;
    let resp = client
        .post(url)
        .header("Authorization", format!("token {auth_token}"))
        .header("Content-Type", "application/json")
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status().as_u16();
    let body_text = resp
        .text()
        .await
        .with_context(|| format!("reading body POST {url}"))?;
    let value = if body_text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&body_text).unwrap_or(Value::String(body_text))
    };
    Ok((status, value))
}

async fn http_delete(url: &str, auth_token: &str) -> Result<u16> {
    let client = build_client()?;
    let resp = client
        .delete(url)
        .header("Authorization", format!("token {auth_token}"))
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    Ok(resp.status().as_u16())
}

/// Returns true when the token can authenticate against Forgejo.
async fn verify_token(base_url: &str, token: &str) -> bool {
    matches!(
        http_get_json(&format!("{base_url}/api/v1/user"), token).await,
        Ok((200, _))
    )
}

/// Returns the Forgejo user_id as a string. Creates the user if it doesn't exist.
async fn find_or_create_user(
    base_url: &str,
    admin_token: &str,
    args: &ForgejoArgs,
) -> Result<String> {
    let (status, body) = http_get_json(
        &format!("{base_url}/api/v1/users/{}", args.username),
        admin_token,
    )
    .await?;

    match status {
        200 => {
            let id = body["id"]
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("GET user: missing 'id' field"))?;
            Ok(id.to_string())
        }
        404 => {
            let create_body = json!({
                "email": args.email,
                "full_name": args.display_name,
                "login_name": args.username,
                "must_change_password": false,
                "password": format!("Bro-{}!", uuid::Uuid::new_v4().simple()),
                "send_notify": false,
                "source_id": 0,
                "username": args.username,
                "visibility": "private",
            });
            let (cs, cb) = http_post_json(
                &format!("{base_url}/api/v1/admin/users"),
                admin_token,
                &create_body,
            )
            .await?;
            if cs == 201 {
                let id = cb["id"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("create user: missing 'id' field"))?;
                Ok(id.to_string())
            } else if cs == 422 || cs == 409 {
                // Race: another caller created the user between our GET and POST.
                let (fs, fb) = http_get_json(
                    &format!("{base_url}/api/v1/users/{}", args.username),
                    admin_token,
                )
                .await?;
                if fs == 200 {
                    let id = fb["id"]
                        .as_i64()
                        .ok_or_else(|| anyhow::anyhow!("GET user after race: missing 'id'"))?;
                    Ok(id.to_string())
                } else {
                    bail!("create user HTTP {cs} then re-fetch HTTP {fs}")
                }
            } else if (500..600).contains(&cs) {
                bail!("Forgejo server error creating user: HTTP {cs}")
            } else {
                bail!("create user HTTP {cs}")
            }
        }
        s if (500..600).contains(&s) => bail!("Forgejo server error getting user: HTTP {s}"),
        s => bail!("unexpected HTTP {s} getting user '{}'", args.username),
    }
}

/// Returns the Forgejo token id for the existing TOKEN_NAME token, if any.
async fn find_token_id(base_url: &str, admin_token: &str, username: &str) -> Result<Option<u64>> {
    let (status, body) = http_get_json(
        &format!("{base_url}/api/v1/users/{username}/tokens"),
        admin_token,
    )
    .await?;

    if (500..600).contains(&status) {
        bail!("Forgejo server error listing tokens: HTTP {status}");
    }
    if status != 200 {
        return Ok(None);
    }
    if let Some(arr) = body.as_array() {
        for tok in arr {
            if tok["name"].as_str() == Some(TOKEN_NAME) {
                if let Some(id) = tok["id"].as_u64() {
                    return Ok(Some(id));
                }
            }
        }
    }
    Ok(None)
}

/// Delete the existing TOKEN_NAME token if present, then create a fresh one.
/// Returns the SHA1 token value (available only at creation time).
async fn create_token(base_url: &str, admin_token: &str, username: &str) -> Result<String> {
    if let Some(id) = find_token_id(base_url, admin_token, username).await? {
        let del_status = http_delete(
            &format!("{base_url}/api/v1/users/{username}/tokens/{id}"),
            admin_token,
        )
        .await?;
        if (500..600).contains(&del_status) {
            bail!("Forgejo server error deleting old token: HTTP {del_status}");
        }
    }

    let body = json!({"name": TOKEN_NAME});
    let (cs, cb) = http_post_json(
        &format!("{base_url}/api/v1/users/{username}/tokens"),
        admin_token,
        &body,
    )
    .await?;

    if cs == 201 {
        let sha1 = cb["sha1"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("create token: response missing 'sha1'"))?
            .to_string();
        Ok(sha1)
    } else if (500..600).contains(&cs) {
        bail!("Forgejo server error creating token: HTTP {cs}")
    } else {
        bail!("create token HTTP {cs}")
    }
}

fn is_retryable(msg: &str) -> bool {
    msg.contains("HTTP 5")
        || msg.contains("server error")
        || msg.contains("connection refused")
        || msg.contains("timed out")
        || msg.contains("os error 111")
        || msg.contains("connect")
}

async fn emit_provision_failed(event: &SystemEvent, state: &Arc<SharedState>, reason: &str) {
    let draft = SystemEventDraft {
        kind: SystemEventKind::BroIdentityProvisionFailed,
        producer: "system_events.forgejo".to_string(),
        project: event.project.clone(),
        principal: event.principal.clone(),
        subject: event.subject.clone(),
        correlation: event.correlation.clone(),
        causation_id: Some(event.id.clone()),
        payload: json!({ "reason": &reason[..reason.len().min(1024)] }),
    };
    if let Err(e) = state.system_events.emit(draft).await {
        tracing::warn!("emit bro.identity.provision_failed: {e:#}");
    }
}

async fn upsert_and_emit_provisioned(
    args: &ForgejoArgs,
    user_id: &str,
    token_ref: &str,
    event: &SystemEvent,
    state: &Arc<SharedState>,
) -> Result<()> {
    let identity = ExternalIdentity {
        scope: "forgejo".to_string(),
        instance: args.instance.clone(),
        subject: format!("bro:{}", args.bro),
        provider: args.provider.clone(),
        model: args.model.clone(),
        external_user_id: user_id.to_string(),
        username: args.username.clone(),
        token_ref: token_ref.to_string(),
        created_at: crate::util::now_iso(),
        last_verified_at: Some(crate::util::now_iso()),
    };
    state
        .system_events
        .identity_registry()
        .upsert(&identity)
        .context("upserting identity")?;

    let draft = SystemEventDraft {
        kind: SystemEventKind::BroIdentityProvisioned,
        producer: "system_events.forgejo".to_string(),
        project: event.project.clone(),
        principal: event.principal.clone(),
        subject: event.subject.clone(),
        correlation: event.correlation.clone(),
        causation_id: Some(event.id.clone()),
        payload: json!({
            "scope": "forgejo",
            "instance": args.instance,
            "username": args.username,
            "external_user_id": user_id,
            "token_ref": token_ref,
        }),
    };
    if let Err(e) = state.system_events.emit(draft).await {
        tracing::warn!("emit bro.identity.provisioned: {e:#}");
    }
    Ok(())
}

async fn do_ensure_user(
    atom_args: &Value,
    event: &SystemEvent,
    state: &Arc<SharedState>,
    secret_sources: &SecretSources,
) -> Result<Value> {
    let args = parse_args(atom_args).map_err(|e| anyhow::anyhow!("permanent:{e}"))?;

    let base_url = resolve_base_url(&args).map_err(|e| anyhow::anyhow!("permanent:{e}"))?;

    let admin_token = resolve_admin_token(&args.admin_token_ref, secret_sources)
        .map_err(|e| anyhow::anyhow!("permanent:{e}"))?;

    // If we already have a valid secret, verify it against Forgejo.
    // On success, re-upsert (refresh last_verified_at) and return early.
    let existing_token = resolve_with_sources(&args.token_secret_name, secret_sources.clone()).ok();
    if let Some(ref tok) = existing_token {
        if verify_token(&base_url, tok.expose()).await {
            let (status, body) = http_get_json(
                &format!("{base_url}/api/v1/users/{}", args.username),
                &admin_token,
            )
            .await?;
            if status == 200 {
                let user_id = body["id"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("GET user: missing 'id'"))?
                    .to_string();
                let token_ref = format!("secret:{}", args.token_secret_name);
                upsert_and_emit_provisioned(&args, &user_id, &token_ref, event, state).await?;
                return Ok(json!({
                    "action": "verified_existing",
                    "username": args.username,
                    "user_id": user_id,
                    "token_ref": token_ref,
                }));
            }
        }
    }

    let user_id = find_or_create_user(&base_url, &admin_token, &args).await?;
    let token_value = create_token(&base_url, &admin_token, &args.username).await?;

    write_file_secret(&args.token_secret_name, &token_value, secret_sources)
        .with_context(|| format!("storing token secret '{}'", args.token_secret_name))
        .map_err(|e| anyhow::anyhow!("permanent:{e}"))?;

    let token_ref = format!("secret:{}", args.token_secret_name);
    upsert_and_emit_provisioned(&args, &user_id, &token_ref, event, state).await?;

    Ok(json!({
        "action": "provisioned",
        "username": args.username,
        "user_id": user_id,
        "token_ref": token_ref,
    }))
}

pub(super) async fn run_forgejo_ensure_user(
    atom_args: &Value,
    event: &SystemEvent,
    state: &Arc<SharedState>,
) -> ActionOutcome {
    run_forgejo_ensure_user_with_sources(atom_args, event, state, SecretSources::default()).await
}

pub(super) async fn run_forgejo_ensure_user_with_sources(
    atom_args: &Value,
    event: &SystemEvent,
    state: &Arc<SharedState>,
    secret_sources: SecretSources,
) -> ActionOutcome {
    match do_ensure_user(atom_args, event, state, &secret_sources).await {
        Ok(summary) => ActionOutcome {
            status: ActionStatus::Succeeded,
            response_summary: summary,
        },
        Err(e) => {
            let msg = format!("{e:#}");
            let status = if msg.contains("permanent:") || !is_retryable(&msg) {
                ActionStatus::PermanentFailure
            } else {
                ActionStatus::RetryableFailure
            };
            emit_provision_failed(event, state, &msg).await;
            ActionOutcome {
                status,
                response_summary: super::gate::redact_values(
                    json!({"error": &msg[..msg.len().min(4096)]}),
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::{delete, get, post};
    use serde_json::json;

    use crate::server::state::SharedState;
    use crate::system_events::types::{SystemEvent, SystemEventKind};

    fn test_state_in(dir: &std::path::Path) -> Arc<SharedState> {
        Arc::new(SharedState::for_test(dir))
    }

    fn fake_event() -> SystemEvent {
        SystemEvent {
            id: "evt-forgejo-test".to_string(),
            kind: SystemEventKind::BroIdentityRequired,
            occurred_at: "2026-05-13T00:00:00Z".to_string(),
            producer: "test".to_string(),
            project: None,
            principal: Some(crate::system_events::types::EventPrincipal {
                kind: "bro".to_string(),
                bro: Some("keystone-review".to_string()),
                provider: Some("claude".to_string()),
                model: Some("haiku-4.5".to_string()),
                effort: None,
            }),
            subject: Some(crate::system_events::types::EventSubject {
                kind: "bro".to_string(),
                id: "bro:keystone-review".to_string(),
            }),
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({
                "identity_scope": "forgejo",
                "instance": "test-forgejo",
                "bro": "keystone-review",
                "provider": "claude",
                "model": "haiku-4.5",
                "username": "bro-keystone-review-claude-haiku45",
                "display_name": "keystone-review / claude haiku-4.5",
                "email": "bro-keystone-review@blackbox.local",
            }),
        }
    }

    fn base_args(base_url: &str, token_secret_name: &str) -> Value {
        json!({
            "instance": "test-forgejo",
            "bro": "keystone-review",
            "provider": "claude",
            "model": "haiku-4.5",
            "username": "bro-keystone-review-claude-haiku45",
            "display_name": "keystone-review / claude haiku-4.5",
            "email": "bro-keystone-review@blackbox.local",
            "token_secret_name": token_secret_name,
            "admin_token_ref": "secret:forgejo-admin-token",
            "base_url": base_url,
        })
    }

    async fn spawn_fake_forgejo(
        user_status: u16,
        create_user_status: u16,
        token_list: Vec<Value>,
        create_token_response: Value,
        create_token_status: u16,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use std::sync::{Arc, Mutex};

        let token_list = Arc::new(Mutex::new(token_list));
        let token_list_for_delete = Arc::clone(&token_list);

        let app = Router::new()
            .route(
                "/api/v1/user",
                get(|| async { axum::Json(json!({"login": "admin"})) }),
            )
            .route(
                "/api/v1/users/{username}",
                get(
                    move |axum::extract::Path(username): axum::extract::Path<String>| {
                        let s = user_status;
                        async move {
                            if s == 200 {
                                (
                                    StatusCode::from_u16(s).unwrap(),
                                    axum::Json(json!({"id": 42, "login": username})),
                                )
                            } else {
                                (
                                    StatusCode::from_u16(s).unwrap(),
                                    axum::Json(json!({"message": "Not Found"})),
                                )
                            }
                        }
                    },
                ),
            )
            .route(
                "/api/v1/admin/users",
                post(move || {
                    let s = create_user_status;
                    async move {
                        (
                            StatusCode::from_u16(s).unwrap(),
                            axum::Json(
                                json!({"id": 42, "login": "bro-keystone-review-claude-haiku45"}),
                            ),
                        )
                    }
                }),
            )
            .route(
                "/api/v1/users/{username}/tokens",
                get({
                    let tl = Arc::clone(&token_list);
                    move |_: axum::extract::Path<String>| {
                        let tl = Arc::clone(&tl);
                        async move { axum::Json(tl.lock().unwrap().clone()) }
                    }
                })
                .post(move |_: axum::extract::Path<String>| {
                    let s = create_token_status;
                    let resp = create_token_response.clone();
                    async move { (StatusCode::from_u16(s).unwrap(), axum::Json(resp)) }
                }),
            )
            .route(
                "/api/v1/users/{username}/tokens/{token_id}",
                delete({
                    move |_: axum::extract::Path<(String, String)>| {
                        let tl = Arc::clone(&token_list_for_delete);
                        async move {
                            tl.lock().unwrap().clear();
                            StatusCode::NO_CONTENT
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    fn test_secret_sources(dir: &std::path::Path) -> SecretSources {
        SecretSources {
            credentials_dir: None,
            secrets_dir: dir.join("secrets"),
            env_prefix: "FORGEJO_TEST_SECRET_".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // 1. Reaction example parses correctly with op=atom_invoke
    // -----------------------------------------------------------------------

    #[test]
    fn reaction_example_parses_with_atom_invoke_op() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/forgejo/reactions/ensure-bro-user.json"
        );
        let content = std::fs::read_to_string(path)
            .expect("examples/forgejo/reactions/ensure-bro-user.json should exist");
        let spec: crate::system_events::types::ReactionSpec =
            serde_json::from_str(&content).expect("should parse as ReactionSpec");
        assert_eq!(spec.name, "forgejo-ensure-bro-user");
        assert!(
            matches!(
                spec.action,
                crate::system_events::types::ReactionAction::AtomInvoke { .. }
            ),
            "action should be AtomInvoke (op=atom_invoke)"
        );
        assert!(
            spec.idempotency_key.is_some(),
            "reaction must have an idempotency_key"
        );
        assert!(
            spec.event_kinds
                .iter()
                .any(|k| k == "bro.identity.required"),
            "reaction should fire on bro.identity.required"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Fake HTTP success: stores secret, upserts identity, emits provisioned,
    //    no token material in identity JSON
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_fake_success_stores_secret_and_emits() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();

        let (base_url, _handle) = spawn_fake_forgejo(
            404,
            201,
            vec![],
            json!({"id": 1, "name": "blackbox-bro", "sha1": "fake-sha1-token-value"}),
            201,
        )
        .await;

        // Set up admin token secret
        let sources = test_secret_sources(dir.path());
        write_file_secret("forgejo-admin-token", "admin-token-value", &sources).unwrap();

        let args = base_args(&base_url, "forgejo-bro-bro-keystone-review-claude-haiku45");
        let outcome =
            run_forgejo_ensure_user_with_sources(&args, &event, &state, sources.clone()).await;

        assert_eq!(
            outcome.status,
            ActionStatus::Succeeded,
            "should succeed: {:?}",
            outcome.response_summary
        );
        assert_eq!(outcome.response_summary["action"], "provisioned");
        assert_eq!(outcome.response_summary["user_id"], "42");

        // Secret should be stored
        let stored = blackbox::secrets::resolve_with_sources(
            "forgejo-bro-bro-keystone-review-claude-haiku45",
            sources,
        )
        .unwrap();
        assert_eq!(stored.expose(), "fake-sha1-token-value");

        // Identity should be upserted
        let reg = state.system_events.identity_registry();
        let id = reg
            .lookup(
                "forgejo",
                "test-forgejo",
                "bro:keystone-review",
                "claude",
                "haiku-4.5",
            )
            .expect("identity should be upserted");
        assert_eq!(
            id.token_ref,
            "secret:forgejo-bro-bro-keystone-review-claude-haiku45"
        );
        // Token value must NOT be in the identity JSON
        let id_json = serde_json::to_string(&id).unwrap();
        assert!(
            !id_json.contains("fake-sha1-token-value"),
            "identity JSON must not contain token value: {id_json}"
        );

        // bro.identity.provisioned should have been emitted
        let events = state
            .system_events
            .list_events(None, Some("bro.identity.provisioned"), None, None)
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one bro.identity.provisioned event expected"
        );
        assert_eq!(events[0].causation_id.as_deref(), Some("evt-forgejo-test"));
        let payload = &events[0].payload;
        assert_eq!(payload["username"], "bro-keystone-review-claude-haiku45");
        assert!(
            !payload.to_string().contains("fake-sha1-token-value"),
            "token value must not appear in emitted event payload"
        );
    }

    // -----------------------------------------------------------------------
    // 3. Idempotent: second run with valid existing token skips user/token creation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_fake_success_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();

        // First run: 404 on user, creates user (201) and token (201)
        let (base_url, _handle) = spawn_fake_forgejo(
            404,
            201,
            vec![],
            json!({"id": 1, "name": "blackbox-bro", "sha1": "idempotent-token"}),
            201,
        )
        .await;

        let sources = test_secret_sources(dir.path());
        write_file_secret("forgejo-admin-token", "admin-token-value", &sources).unwrap();
        let secret_name = "forgejo-bro-bro-keystone-review-claude-haiku45";
        let args = base_args(&base_url, secret_name);

        let o1 = run_forgejo_ensure_user_with_sources(&args, &event, &state, sources.clone()).await;
        assert_eq!(o1.status, ActionStatus::Succeeded);

        // Second run: same fake server now returns 200 for GET /api/v1/users/{username}
        // and GET /api/v1/user (token verify) also returns 200 (already stored)
        let (base_url2, _h2) = spawn_fake_forgejo(
            200, // GET user now 200
            201,
            vec![json!({"id": 1, "name": "blackbox-bro"})],
            json!({}),
            500, // token creation would fail — should NOT be called
        )
        .await;
        // The stored token "idempotent-token" will hit GET /api/v1/user → 200 on server2
        let args2 = base_args(&base_url2, secret_name);

        let o2 =
            run_forgejo_ensure_user_with_sources(&args2, &event, &state, sources.clone()).await;
        assert_eq!(
            o2.status,
            ActionStatus::Succeeded,
            "second run should succeed: {:?}",
            o2.response_summary
        );
        assert_eq!(o2.response_summary["action"], "verified_existing");

        // Only one identity record should exist
        let all = state.system_events.identity_registry().list_all();
        assert_eq!(all.len(), 1, "no duplicate identity records: {all:?}");
    }

    // -----------------------------------------------------------------------
    // 4. Unreachable server → RetryableFailure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_server_unreachable_returns_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();
        let sources = test_secret_sources(dir.path());
        write_file_secret("forgejo-admin-token", "admin-token-value", &sources).unwrap();

        // Port 1 refuses connections on Linux without needing an actual server
        let args = base_args("http://127.0.0.1:1", "secret-not-used");
        let outcome = run_forgejo_ensure_user_with_sources(&args, &event, &state, sources).await;
        assert_eq!(
            outcome.status,
            ActionStatus::RetryableFailure,
            "unreachable server should be retryable: {:?}",
            outcome.response_summary
        );

        // provision_failed event should have been emitted best-effort
        let failed = state
            .system_events
            .list_events(None, Some("bro.identity.provision_failed"), None, None)
            .unwrap();
        assert_eq!(failed.len(), 1, "provision_failed should be emitted");
    }

    // -----------------------------------------------------------------------
    // 5. Fake 500 → RetryableFailure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_server_500_returns_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();

        let (base_url, _handle) = spawn_fake_forgejo(500, 500, vec![], json!({}), 500).await;
        let sources = test_secret_sources(dir.path());
        write_file_secret("forgejo-admin-token", "admin-token-value", &sources).unwrap();

        let args = base_args(&base_url, "ts-500");
        let outcome = run_forgejo_ensure_user_with_sources(&args, &event, &state, sources).await;
        assert_eq!(
            outcome.status,
            ActionStatus::RetryableFailure,
            "HTTP 500 should be retryable: {:?}",
            outcome.response_summary
        );
    }

    // -----------------------------------------------------------------------
    // 6. Missing required arg → PermanentFailure
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_missing_required_arg_permanent() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();
        let sources = test_secret_sources(dir.path());

        // Missing "username"
        let args = json!({
            "instance": "x",
            "bro": "x",
            "provider": "claude",
            "model": "haiku-4.5",
            "display_name": "x",
            "email": "x@x",
            "token_secret_name": "x",
            "admin_token_ref": "secret:x",
            "base_url": "http://127.0.0.1:1",
        });
        let outcome = run_forgejo_ensure_user_with_sources(&args, &event, &state, sources).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
    }

    // -----------------------------------------------------------------------
    // 7. Token material never leaks into identity record or response_summary
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ensure_user_no_token_material_in_identity_or_summary() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state_in(dir.path());
        let event = fake_event();

        let (base_url, _handle) = spawn_fake_forgejo(
            404,
            201,
            vec![],
            json!({"id": 9, "name": "blackbox-bro", "sha1": "super-secret-raw-token"}),
            201,
        )
        .await;

        let sources = test_secret_sources(dir.path());
        write_file_secret("forgejo-admin-token", "admin", &sources).unwrap();

        let secret_name = "forgejo-bro-test-no-leak";
        let args = base_args(&base_url, secret_name);
        let outcome = run_forgejo_ensure_user_with_sources(&args, &event, &state, sources).await;
        assert_eq!(outcome.status, ActionStatus::Succeeded);

        // response_summary must not contain the raw token
        let summary_str = serde_json::to_string(&outcome.response_summary).unwrap();
        assert!(
            !summary_str.contains("super-secret-raw-token"),
            "response_summary must not contain raw token: {summary_str}"
        );

        // identity JSON must not contain the raw token
        let all = state.system_events.identity_registry().list_all();
        let id_json = serde_json::to_string(&all).unwrap();
        assert!(
            !id_json.contains("super-secret-raw-token"),
            "identity JSON must not contain raw token: {id_json}"
        );

        // emitted event payload must not contain the raw token
        let evts = state
            .system_events
            .list_events(None, Some("bro.identity.provisioned"), None, None)
            .unwrap();
        let evt_str = serde_json::to_string(&evts).unwrap();
        assert!(
            !evt_str.contains("super-secret-raw-token"),
            "emitted event must not contain raw token: {evt_str}"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 8: example artifacts parse + packet compiles
    // -----------------------------------------------------------------------

    #[test]
    fn example_noop_task_completed_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/system-events/reactions/noop-task-completed.json"
        );
        let content = std::fs::read_to_string(path).expect("noop-task-completed.json must exist");
        let spec: crate::system_events::types::ReactionSpec =
            serde_json::from_str(&content).expect("must parse as ReactionSpec");
        assert_eq!(spec.name, "noop-task-completed");
        assert!(matches!(
            spec.action,
            crate::system_events::types::ReactionAction::EmitEvent { .. }
        ));
        assert!(
            spec.idempotency_key.is_none(),
            "emit_event reactions do not require idempotency_key"
        );
        assert!(spec.event_kinds.iter().any(|k| k == "task.completed"));
    }

    #[test]
    fn example_forgejo_ensure_bro_user_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/system-events/reactions/forgejo-ensure-bro-user.json"
        );
        let content =
            std::fs::read_to_string(path).expect("forgejo-ensure-bro-user.json must exist");
        let spec: crate::system_events::types::ReactionSpec =
            serde_json::from_str(&content).expect("must parse as ReactionSpec");
        assert_eq!(spec.name, "forgejo-ensure-bro-user");
        assert!(matches!(
            spec.action,
            crate::system_events::types::ReactionAction::AtomInvoke { .. }
        ));
        assert!(
            spec.idempotency_key.is_some(),
            "non-emit_event reactions must declare an idempotency_key"
        );
        assert_eq!(
            spec.when.as_deref(),
            Some("domain:system-event/forgejo-identity-required"),
            "must reference the gate packet"
        );
    }

    #[test]
    fn example_forgejo_identity_required_packet_compiles() {
        let src =
            include_str!("../../examples/system-events/packets/forgejo-identity-required.json");
        let params: crate::packets::CompileParams =
            serde_json::from_str(src).expect("packet must parse as CompileParams");
        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        packets
            .compile(&params)
            .expect("forgejo-identity-required packet must compile");
        let loaded = packets
            .load("domain:system-event/forgejo-identity-required")
            .expect("compiled packet must load");

        let allow = crate::packets::apply(
            &loaded,
            &serde_json::json!({"payload": {"identity_scope": "forgejo"}}),
        )
        .expect("forgejo scope should match");
        assert_eq!(allow.classification, "allow");

        let skip = crate::packets::apply(
            &loaded,
            &serde_json::json!({"payload": {"identity_scope": "github"}}),
        )
        .expect("non-forgejo scope should match a rule");
        assert_eq!(skip.classification, "skip");
    }

    // -----------------------------------------------------------------------
    // Phase 8: redaction audit — dry-run output never leaks token material
    // -----------------------------------------------------------------------

    #[test]
    fn dry_run_replay_redacts_authorization_and_secret_refs() {
        use crate::system_events::types::*;
        use serde_json::json;

        let reaction = ReactionSpec {
            contract: "reaction/v1".to_string(),
            name: "leaky-reaction".to_string(),
            version: 1,
            enabled: true,
            event_kinds: vec!["task.completed".to_string()],
            when: None,
            idempotency_key: Some("k:${event.id}".to_string()),
            action: ReactionAction::HttpJson {
                args: json!({
                    "method": "POST",
                    "url": "https://example.com/api",
                    "headers": {
                        "Authorization": "Bearer THIS_VALUE_MUST_BE_REDACTED"
                    },
                    "secret_headers": {
                        "X-Bro-Token": "token secret:forgejo-bro-foo"
                    },
                    "body": {
                        "mcp_credential": "secret:mcp-private-key",
                        "echo": "${event.id}"
                    }
                }),
            },
            retry: RetryPolicy::default(),
            on_failure: FailurePolicy::DeadLetter,
        };

        let event = SystemEvent {
            id: "evt-redact-test".to_string(),
            kind: SystemEventKind::TaskCompleted,
            occurred_at: "2026-05-13T12:00:00Z".to_string(),
            producer: "test".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
        };

        let tmp = tempfile::tempdir().unwrap();
        let packets = crate::packets::Packets::open(tmp.path()).unwrap();
        let result = crate::system_events::dry_run_replay(&reaction, &event, &packets, &[])
            .expect("dry_run_replay must succeed");

        let json = serde_json::to_string(&result.rendered_action_args).unwrap();

        // Raw Authorization header value must never appear.
        assert!(
            !json.contains("THIS_VALUE_MUST_BE_REDACTED"),
            "Authorization value must be redacted: {json}"
        );
        // secret: references must never appear as resolved values; they get redacted to [REDACTED].
        assert!(
            !json.contains("forgejo-bro-foo"),
            "secret: reference must be redacted: {json}"
        );
        assert!(
            !json.contains("mcp-private-key"),
            "MCP credential secret name must be redacted: {json}"
        );
        assert!(
            json.contains("[REDACTED]"),
            "redaction marker must be present: {json}"
        );
    }

    #[test]
    fn outbox_records_never_contain_raw_secrets() {
        // Compose a record with a deliberately suspect last_error / response_summary
        // and assert the redaction helpers strip the material before it reaches disk.
        use crate::system_events::types::OutboxStatus;

        let mut record = crate::system_events::types::OutboxRecord::new(
            "evt-1",
            "test",
            Some("idem".to_string()),
        );
        // Worker code routes error strings through redact_string before storing them.
        let raw_error = "Forgejo POST failed: Bearer THIS_TOKEN_SHOULD_NOT_LEAK \
            secret:forgejo-bro-leak";
        let redacted = super::super::worker::redact_string_for_test(raw_error);
        record.status = OutboxStatus::DeadLettered;
        record.last_error = Some(redacted.clone());

        let dir = tempfile::tempdir().unwrap();
        let store = crate::system_events::OutboxStore::new(dir.path().to_path_buf()).unwrap();
        store.append(&record).unwrap();

        let raw_jsonl =
            std::fs::read_to_string(dir.path().join("current.jsonl")).expect("must read jsonl");
        assert!(
            !raw_jsonl.contains("THIS_TOKEN_SHOULD_NOT_LEAK"),
            "raw bearer token must not reach disk: {raw_jsonl}"
        );
        assert!(
            !raw_jsonl.contains("forgejo-bro-leak"),
            "secret name must not reach disk after redaction: {raw_jsonl}"
        );
        assert!(
            raw_jsonl.contains("[REDACTED]"),
            "redaction marker must reach disk: {raw_jsonl}"
        );
    }
}
