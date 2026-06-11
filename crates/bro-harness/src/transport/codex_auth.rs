//! Cooperative ChatGPT-OAuth handling for the Responses transport, so the
//! harness is self-sufficient (usable without the blackbox daemon) yet shares
//! credential state with the Codex CLI.
//!
//! Reads `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`). If the access
//! token is within a skew window of expiry, refreshes it against the OpenAI
//! OAuth endpoint (`grant_type=refresh_token`) and writes the rotated tokens
//! back to the same file — under an advisory file lock, atomically — exactly
//! as the Codex CLI does. Refresh is rare (access tokens last ~days).
//!
//! Endpoint + client id are the Codex public OAuth client, overridable via
//! `CODEX_OAUTH_TOKEN_URL` / `CODEX_OAUTH_CLIENT_ID`. Skew via
//! `CODEX_OAUTH_REFRESH_SKEW_SECS` (default 300; set huge to force refresh).

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use fs2::FileExt;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_SKEW_SECS: i64 = 300;

pub struct ChatGptAuth {
    pub access_token: String,
    pub account_id: String,
}

fn codex_home() -> PathBuf {
    super::session_var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
}

/// Load auth, refreshing the access token if near expiry, and return the
/// (possibly refreshed) access token + account id.
pub async fn load_fresh(http: &reqwest::Client) -> Result<ChatGptAuth> {
    with_auth_lock(|auth_path| load_and_maybe_refresh(http, auth_path, /*force*/ false)).await
}

/// Force a token refresh regardless of expiry skew, and return the rotated
/// credentials. Called by the transport after a `401` so a token the proactive
/// skew check thought was still valid (clock skew, server-side revocation) can
/// recover in-band instead of failing the turn. Mirrors codex's
/// `UnauthorizedRecovery` reload→refresh step.
pub async fn force_refresh(http: &reqwest::Client) -> Result<ChatGptAuth> {
    with_auth_lock(|auth_path| load_and_maybe_refresh(http, auth_path, /*force*/ true)).await
}

/// Run `f` under an exclusive advisory lock on the auth.json sidecar so
/// concurrent harness/codex refreshes serialize without contending on the file
/// we rewrite.
async fn with_auth_lock<F, Fut>(f: F) -> Result<ChatGptAuth>
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<ChatGptAuth>>,
{
    let dir = codex_home();
    let auth_path = dir.join("auth.json");
    let lock_path = dir.join("auth.json.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))?;
    lock.lock_exclusive().context("acquire auth lock")?;
    let result = f(auth_path.clone()).await;
    let _ = FileExt::unlock(&lock);
    result
}

// rare token-refresh read; offload tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
async fn load_and_maybe_refresh(
    http: &reqwest::Client,
    auth_path: std::path::PathBuf,
    force: bool,
) -> Result<ChatGptAuth> {
    let body = std::fs::read_to_string(&auth_path)
        .with_context(|| format!("read {}", auth_path.display()))?;
    let mut v: Value = serde_json::from_str(&body).context("parse auth.json")?;

    let account_id = account_id_from(&v).context(
        "auth.json missing tokens.account_id and id_token carried no chatgpt_account_id claim",
    )?;
    let access_token = v["tokens"]["access_token"]
        .as_str()
        .context("auth.json missing tokens.access_token")?
        .to_string();

    if !force && !needs_refresh(&access_token) {
        return Ok(ChatGptAuth {
            access_token,
            account_id,
        });
    }

    let refresh_token = v["tokens"]["refresh_token"]
        .as_str()
        .context("token near expiry but auth.json has no refresh_token")?
        .to_string();

    tracing::info!(force, "refreshing codex access token");
    let refreshed = refresh(http, &refresh_token).await?;
    merge_refresh(&mut v, &refreshed);
    write_back(&auth_path, &v)?;

    let access_token = v["tokens"]["access_token"]
        .as_str()
        .context("refreshed auth.json missing access_token")?
        .to_string();
    Ok(ChatGptAuth {
        access_token,
        account_id,
    })
}

/// True if the JWT `exp` is missing/within skew of now. A non-JWT (opaque)
/// token returns false — we use it as-is and let a 401 trigger re-auth.
fn needs_refresh(access_token: &str) -> bool {
    let skew = std::env::var("CODEX_OAUTH_REFRESH_SKEW_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SKEW_SECS);
    match jwt_exp(access_token) {
        Some(exp) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            exp - now <= skew
        }
        None => false,
    }
}

/// Decode a JWT's `exp` claim without verifying the signature.
fn jwt_exp(token: &str) -> Option<i64> {
    decode_jwt_claims(token)?["exp"].as_i64()
}

/// Decode a JWT's payload claims without verifying the signature.
fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Resolve the ChatGPT account id: prefer the explicit `tokens.account_id`
/// field, else decode it from the `id_token` JWT's
/// `https://api.openai.com/auth.chatgpt_account_id` claim — the same fallback
/// codex uses (`login/src/token_data.rs`), so a stripped/older auth.json that
/// omits the bare field still resolves.
fn account_id_from(v: &Value) -> Option<String> {
    if let Some(id) = v["tokens"]["account_id"].as_str() {
        return Some(id.to_string());
    }
    let id_token = v["tokens"]["id_token"].as_str()?;
    let claims = decode_jwt_claims(id_token)?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(str::to_string)
}

#[derive(Debug)]
struct Refreshed {
    access_token: String,
    id_token: Option<String>,
    refresh_token: Option<String>,
}

async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<Refreshed> {
    let token_url =
        std::env::var("CODEX_OAUTH_TOKEN_URL").unwrap_or_else(|_| DEFAULT_TOKEN_URL.to_string());
    let client_id =
        std::env::var("CODEX_OAUTH_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string());

    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", client_id.as_str()),
        ("refresh_token", refresh_token),
    ];
    let resp = http
        .post(&token_url)
        .form(&params)
        .send()
        .await
        .context("oauth refresh request")?;
    let status = resp.status();
    let text = resp.text().await.context("read refresh body")?;
    if !status.is_success() {
        anyhow::bail!("oauth refresh {status}: {text}");
    }
    let v: Value = serde_json::from_str(&text).context("parse refresh response")?;
    let access_token = v["access_token"]
        .as_str()
        .context("refresh response missing access_token")?
        .to_string();
    Ok(Refreshed {
        access_token,
        id_token: v["id_token"].as_str().map(str::to_string),
        refresh_token: v["refresh_token"].as_str().map(str::to_string),
    })
}

/// Merge refreshed tokens into the auth.json value, preserving every other
/// field (account_id, OPENAI_API_KEY, etc.). A refresh response that omits
/// `refresh_token`/`id_token` keeps the existing ones.
fn merge_refresh(v: &mut Value, r: &Refreshed) {
    let tokens = v["tokens"].as_object_mut();
    if let Some(t) = tokens {
        t.insert("access_token".into(), Value::String(r.access_token.clone()));
        if let Some(id) = &r.id_token {
            t.insert("id_token".into(), Value::String(id.clone()));
        }
        if let Some(rt) = &r.refresh_token {
            t.insert("refresh_token".into(), Value::String(rt.clone()));
        }
    }
    v["last_refresh"] = Value::String(chrono::Utc::now().to_rfc3339());
}

/// Atomic write (temp + rename) with 0600 perms.
// rare token-refresh write; offload tracked in thread-935b467d.
#[allow(clippy::disallowed_methods)]
fn write_back(auth_path: &std::path::Path, v: &Value) -> Result<()> {
    let body = serde_json::to_string_pretty(v).context("serialize auth.json")?;
    let tmp = auth_path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, auth_path).context("rename auth.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jwt_exp_decodes() {
        // {"exp": 9999999999}
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":9999999999}"#);
        let token = format!("h.{payload}.sig");
        assert_eq!(jwt_exp(&token), Some(9999999999));
        assert_eq!(jwt_exp("not-a-jwt"), None);
    }

    #[test]
    fn account_id_prefers_field_then_falls_back_to_id_token() {
        // Explicit field wins.
        let v = json!({"tokens": {"account_id": "acct-field"}});
        assert_eq!(account_id_from(&v).as_deref(), Some("acct-field"));

        // No field → decode from id_token's chatgpt_account_id claim.
        let claims = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-jwt"}}"#);
        let id_token = format!("h.{claims}.sig");
        let v = json!({"tokens": {"id_token": id_token}});
        assert_eq!(account_id_from(&v).as_deref(), Some("acct-jwt"));

        // Neither → None.
        assert_eq!(account_id_from(&json!({"tokens": {}})), None);
    }

    #[test]
    fn far_future_token_does_not_refresh() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":9999999999}"#);
        let token = format!("h.{payload}.sig");
        assert!(!needs_refresh(&token));
    }

    #[test]
    fn merge_preserves_other_fields_and_keeps_missing_tokens() {
        let mut v = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": "old_a",
                "id_token": "old_id",
                "refresh_token": "old_rt",
                "account_id": "acct-1"
            }
        });
        // refresh response without a new refresh_token
        merge_refresh(
            &mut v,
            &Refreshed {
                access_token: "new_a".into(),
                id_token: None,
                refresh_token: None,
            },
        );
        assert_eq!(v["tokens"]["access_token"], "new_a");
        assert_eq!(v["tokens"]["id_token"], "old_id"); // preserved
        assert_eq!(v["tokens"]["refresh_token"], "old_rt"); // preserved
        assert_eq!(v["tokens"]["account_id"], "acct-1"); // preserved
        assert_eq!(v["auth_mode"], "chatgpt"); // preserved
        assert!(v["last_refresh"].is_string());
    }
}
