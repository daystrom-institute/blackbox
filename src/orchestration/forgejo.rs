//! Forgejo API client used by HookOps for issue/PR fetching, PR
//! creation, and PR commenting.
//!
//! Targets Forgejo (Gitea-compatible) v1 REST API:
//!   GET    /api/v1/repos/{owner}/{repo}/issues/{index}
//!   GET    /api/v1/repos/{owner}/{repo}/issues
//!   POST   /api/v1/repos/{owner}/{repo}/pulls
//!   POST   /api/v1/repos/{owner}/{repo}/issues/{index}/comments
//!
//! Auth: bearer token from `FORGEJO_TOKEN` env var (per-arc override
//! via args.token_env). Base URL from `FORGEJO_BASE_URL` env var
//! (typically `http://forgejo:3000` in the docker-compose network or
//! `http://localhost:3000` from the host).
//!
//! These ops return the raw API response as the var value when
//! `into_var` is set, so downstream nodes can inspect fields like
//! `${vars.issue.title}`, `${vars.pr.number}`, etc.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

use crate::workflow::ops::OpEffect;

fn base_url(args: &Value) -> Result<String> {
    if let Some(b) = args.get("base_url").and_then(|v| v.as_str()) {
        return Ok(b.trim_end_matches('/').to_string());
    }
    let env_var = args
        .get("base_url_env")
        .and_then(|v| v.as_str())
        .unwrap_or("FORGEJO_BASE_URL");
    std::env::var(env_var)
        .map(|s| s.trim_end_matches('/').to_string())
        .map_err(|_| anyhow!("forgejo: ${{{env_var}}} not set and args.base_url not given"))
}

fn token(args: &Value) -> Result<String> {
    let env_var = args
        .get("token_env")
        .and_then(|v| v.as_str())
        .unwrap_or("FORGEJO_TOKEN");
    std::env::var(env_var)
        .map_err(|_| anyhow!("forgejo: ${{{env_var}}} not set"))
}

fn client() -> Result<Client> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| anyhow!("forgejo client build: {e}"))
}

fn into_effect(into_var: Option<&str>, value: Value) -> Result<OpEffect> {
    match into_var {
        Some(k) => Ok(OpEffect::SetVar {
            key: k.to_string(),
            value,
        }),
        None => Ok(OpEffect::None),
    }
}

pub async fn issue_fetch(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoIssueFetch requires args.owner"))?;
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoIssueFetch requires args.repo"))?;
    let index = args
        .get("index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("ForgejoIssueFetch requires args.index (int)"))?;
    let url = format!(
        "{}/api/v1/repos/{owner}/{repo}/issues/{index}",
        base_url(args)?
    );
    let resp = client()?
        .get(&url)
        .bearer_auth(token(args)?)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("forgejo issue_fetch {status}: {body}");
    }
    let body: Value = resp.json().await.context("issue_fetch json parse")?;
    into_effect(into_var, body)
}

pub async fn issue_list(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoIssueList requires args.owner"))?;
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoIssueList requires args.repo"))?;
    let state = args
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("open");
    let sort = args
        .get("sort")
        .and_then(|v| v.as_str())
        .unwrap_or("oldest");
    let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10);
    let url = format!(
        "{}/api/v1/repos/{owner}/{repo}/issues?type=issues&state={state}&sort={sort}&limit={limit}",
        base_url(args)?
    );
    let resp = client()?
        .get(&url)
        .bearer_auth(token(args)?)
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("forgejo issue_list {status}: {body}");
    }
    let body: Value = resp.json().await.context("issue_list json parse")?;
    into_effect(into_var, body)
}

pub async fn pr_create(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrCreate requires args.owner"))?;
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrCreate requires args.repo"))?;
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrCreate requires args.title"))?;
    let head = args
        .get("head")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrCreate requires args.head (branch)"))?;
    let base = args
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("main");
    let body_text = args
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let url = format!("{}/api/v1/repos/{owner}/{repo}/pulls", base_url(args)?);
    let payload = json!({
        "title": title,
        "head": head,
        "base": base,
        "body": body_text,
    });
    let resp = client()?
        .post(&url)
        .bearer_auth(token(args)?)
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("forgejo pr_create {status}: {body}");
    }
    let body: Value = resp.json().await.context("pr_create json parse")?;
    into_effect(into_var, body)
}

pub async fn pr_comment(args: &Value, into_var: Option<&str>) -> Result<OpEffect> {
    let owner = args
        .get("owner")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrComment requires args.owner"))?;
    let repo = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrComment requires args.repo"))?;
    let index = args
        .get("index")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("ForgejoPrComment requires args.index (int)"))?;
    let body_text = args
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("ForgejoPrComment requires args.body (string)"))?;
    let url = format!(
        "{}/api/v1/repos/{owner}/{repo}/issues/{index}/comments",
        base_url(args)?
    );
    let resp = client()?
        .post(&url)
        .bearer_auth(token(args)?)
        .header("Accept", "application/json")
        .json(&json!({"body": body_text}))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("forgejo pr_comment {status}: {body}");
    }
    let body: Value = resp.json().await.context("pr_comment json parse")?;
    into_effect(into_var, body)
}

/// Verify a Forgejo webhook signature.
///
/// Forgejo (and Gitea) sends `X-Gitea-Signature: <hex_hmac_sha256>`
/// computed over the raw request body using the configured secret.
/// Returns true on match, false otherwise. Constant-time comparison.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    // Header may be raw hex or `sha256=<hex>`; accept both.
    let hex_part = header.strip_prefix("sha256=").unwrap_or(header);
    let provided = match hex::decode(hex_part.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if provided.len() != expected.len() {
        return false;
    }
    constant_time_eq(&provided, &expected)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_verifies() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;
        let secret = b"test-secret";
        let body = br#"{"action":"opened"}"#;
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(verify_signature(secret, body, &sig));
        assert!(verify_signature(secret, body, &format!("sha256={sig}")));
        assert!(!verify_signature(secret, body, "deadbeef"));
        assert!(!verify_signature(b"wrong-secret", body, &sig));
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
