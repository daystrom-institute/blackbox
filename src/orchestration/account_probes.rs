//! Account utilization probers.
//!
//! Fetch real per-account quota/headroom from each provider's usage surface and
//! write it into the allocator [`ProbeStore`], so `quota_capacity` differentiates
//! providers by genuine remaining headroom instead of every lane tying — a tie
//! collapses tier selection onto the deterministic tiebreak (the brodex-wins
//! symptom). The allocator already *consumes* these signals; this module is the
//! producer that was designed (`acquire-drone.md` §6) but never implemented.
//!
//! Each provider has a pure parser (unit-tested against the live response shape)
//! plus an async fetch. `refresh_account_probes` runs them all and merges the
//! results into the store, keyed by the same `lane_key` the allocator scores on.
//!
//! NOT yet implemented: Gemini credential-freshness, and transcript-usage
//! enrichment between probes (the `last_runtime_observation_at` fusion).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::orchestration::allocator::{
    CredentialStatus, ProbeRecord, QuotaConfidence, QuotaStatus, lane_key, probe_store_load,
    probe_store_save,
};
use crate::orchestration::providers::Provider;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

const ZAI_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const CLAUDE_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// PAYG balance (USD) at or above which DeepSeek scores full capacity. Below it,
/// capacity scales down linearly so a near-empty account is deprioritized.
const DEEPSEEK_BALANCE_FLOOR_USD: f64 = 5.0;

fn base_record(provider: Provider, account: Option<String>, now: u64) -> ProbeRecord {
    ProbeRecord {
        provider,
        account,
        credential_status: CredentialStatus::Present,
        quota_status: QuotaStatus::Known,
        quota_confidence: QuotaConfidence::QuotaProbe,
        five_hour_utilization: None,
        seven_day_utilization: None,
        balance_capacity: None,
        cooldown_until: None,
        last_probe_at: Some(now),
        last_runtime_observation_at: None,
        raw_summary: None,
    }
}

/// Read `ANTHROPIC_AUTH_TOKEN` from a Claude-compatible config dir's
/// `settings.json` (mirrors `brofile::default_claude_compatible_env`).
fn read_anthropic_token(config_dir: &Path) -> Option<String> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(config_dir.join("settings.json")).ok()?)
            .ok()?;
    v["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .map(str::to_string)
}

// ── GLM / Z.AI: zai-usage-endpoint ──────────────────────────────────────────

struct ZaiAccount {
    account: Option<String>,
    token: String,
}

/// Discover the default `~/.claude-zai` plus any `~/.claude-zai-<name>` variant.
fn discover_zai_accounts(home: &Path) -> Vec<ZaiAccount> {
    let mut out = Vec::new();
    if let Some(token) = read_anthropic_token(&home.join(".claude-zai")) {
        out.push(ZaiAccount {
            account: None,
            token,
        });
    }
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(suffix) = name.strip_prefix(".claude-zai-") {
                if !suffix.is_empty() {
                    if let Some(token) = read_anthropic_token(&entry.path()) {
                        out.push(ZaiAccount {
                            account: Some(suffix.to_string()),
                            token,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Parse z.ai `monitor/usage/quota/limit`. Discriminates the two `TOKENS_LIMIT`
/// rows by `(number, unit)`: `(5, 3)` = five-hour, `(1, 6)` = seven-day.
/// `percentage` is 0..100.
fn parse_zai_quota(account: Option<String>, body: &Value, now: u64) -> Result<ProbeRecord> {
    let limits = body["data"]["limits"]
        .as_array()
        .context("z.ai quota response missing data.limits[]")?;
    let window = |number: i64, unit: i64| -> Option<f64> {
        limits.iter().find_map(|limit| {
            (limit["type"].as_str() == Some("TOKENS_LIMIT")
                && limit["number"].as_i64() == Some(number)
                && limit["unit"].as_i64() == Some(unit))
            .then(|| (limit["percentage"].as_f64().unwrap_or(0.0) / 100.0).clamp(0.0, 1.0))
        })
    };
    let five_hour = window(5, 3);
    let seven_day = window(1, 6);
    if five_hour.is_none() && seven_day.is_none() {
        anyhow::bail!("z.ai quota response had no TOKENS_LIMIT 5h/7d windows");
    }
    let level = body["data"]["level"].as_str().unwrap_or("");
    let exhausted = five_hour.unwrap_or(0.0) >= 1.0 || seven_day.unwrap_or(0.0) >= 1.0;
    let mut rec = base_record(Provider::Glm, account, now);
    rec.five_hour_utilization = five_hour;
    rec.seven_day_utilization = seven_day;
    rec.quota_status = if exhausted {
        QuotaStatus::Exhausted
    } else {
        QuotaStatus::Known
    };
    rec.raw_summary = Some(format!("zai quota probe, plan level={level}"));
    Ok(rec)
}

async fn probe_glm(
    client: &reqwest::Client,
    home: &Path,
    now: u64,
) -> Vec<(String, ProbeRecord)> {
    let mut out = Vec::new();
    for acct in discover_zai_accounts(home) {
        let result = async {
            let resp = client
                .get(ZAI_QUOTA_URL)
                .header("Authorization", &acct.token)
                .header("Accept-Language", "en-US,en")
                .header("Content-Type", "application/json")
                .timeout(PROBE_TIMEOUT)
                .send()
                .await
                .context("z.ai request failed")?;
            let body: Value = resp.json().await.context("z.ai response not JSON")?;
            parse_zai_quota(acct.account.clone(), &body, now)
        }
        .await;
        match result {
            Ok(rec) => out.push((lane_key(Provider::Glm, rec.account.as_deref()), rec)),
            Err(e) => tracing::warn!(account = ?acct.account, error = %e, "glm probe failed"),
        }
    }
    out
}

// ── Claude: rate-limit-headers ──────────────────────────────────────────────

/// Claude Code OAuth access token from `~/.claude/.credentials.json`.
fn read_claude_oauth_token(home: &Path) -> Option<String> {
    let v: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".claude").join(".credentials.json")).ok()?,
    )
    .ok()?;
    v["claudeAiOauth"]["accessToken"]
        .as_str()
        .map(str::to_string)
}

fn header_fraction(headers: &HeaderMap, name: &str) -> Option<f64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .parse::<f64>()
        .ok()
        .map(|v| v.clamp(0.0, 1.0))
}

/// Parse Anthropic unified rate-limit headers. These utilizations are already
/// fractions (e.g. `0.02`), unlike GLM/Codex percentages.
fn parse_claude_ratelimit(account: Option<String>, headers: &HeaderMap, now: u64) -> Option<ProbeRecord> {
    let five_hour = header_fraction(headers, "anthropic-ratelimit-unified-5h-utilization");
    let seven_day = header_fraction(headers, "anthropic-ratelimit-unified-7d-utilization");
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    let status = headers
        .get("anthropic-ratelimit-unified-status")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let exhausted =
        status == "rejected" || five_hour.unwrap_or(0.0) >= 1.0 || seven_day.unwrap_or(0.0) >= 1.0;
    let mut rec = base_record(Provider::Claude, account, now);
    rec.five_hour_utilization = five_hour;
    rec.seven_day_utilization = seven_day;
    rec.quota_status = if exhausted {
        QuotaStatus::Exhausted
    } else {
        QuotaStatus::Known
    };
    rec.raw_summary = Some(format!("claude unified rate-limit, status={status}"));
    Some(rec)
}

async fn probe_claude(
    client: &reqwest::Client,
    home: &Path,
    now: u64,
) -> Option<(String, ProbeRecord)> {
    let token = read_claude_oauth_token(home)?;
    let result = async {
        let resp = client
            .post(CLAUDE_MESSAGES_URL)
            .header("authorization", format!("Bearer {token}"))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("content-type", "application/json")
            .timeout(PROBE_TIMEOUT)
            .json(&serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "ping"}]
            }))
            .send()
            .await
            .context("claude request failed")?;
        parse_claude_ratelimit(None, resp.headers(), now)
            .context("claude response had no unified rate-limit headers")
    }
    .await;
    match result {
        Ok(rec) => Some((lane_key(Provider::Claude, None), rec)),
        Err(e) => {
            tracing::warn!(error = %e, "claude probe failed");
            None
        }
    }
}

// ── Codex: usage-endpoint (shared with Brodex) ──────────────────────────────

/// `(access_token, account_id)` from the codex CLI's `~/.codex/auth.json`.
fn read_codex_auth(home: &Path) -> Option<(String, Option<String>)> {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".codex").join("auth.json")).ok()?)
            .ok()?;
    let token = v["tokens"]["access_token"].as_str()?.to_string();
    let account_id = v["tokens"]["account_id"].as_str().map(str::to_string);
    Some((token, account_id))
}

/// Parse ChatGPT `wham/usage`. `used_percent` is 0..100. `primary_window` is the
/// 5h window, `secondary_window` the weekly/7d.
fn parse_codex_usage(provider: Provider, body: &Value, now: u64) -> Option<ProbeRecord> {
    let rl = &body["rate_limit"];
    let pct = |w: &str| -> Option<f64> {
        rl[w]["used_percent"]
            .as_f64()
            .map(|p| (p / 100.0).clamp(0.0, 1.0))
    };
    let five_hour = pct("primary_window");
    let seven_day = pct("secondary_window");
    if five_hour.is_none() && seven_day.is_none() {
        return None;
    }
    let limit_reached = rl["limit_reached"].as_bool().unwrap_or(false);
    let plan = body["plan_type"].as_str().unwrap_or("");
    let exhausted =
        limit_reached || five_hour.unwrap_or(0.0) >= 1.0 || seven_day.unwrap_or(0.0) >= 1.0;
    let mut rec = base_record(provider, None, now);
    rec.five_hour_utilization = five_hour;
    rec.seven_day_utilization = seven_day;
    rec.quota_status = if exhausted {
        QuotaStatus::Exhausted
    } else {
        QuotaStatus::Known
    };
    rec.raw_summary = Some(format!("codex wham/usage, plan={plan}"));
    Some(rec)
}

async fn probe_codex(
    client: &reqwest::Client,
    home: &Path,
    now: u64,
) -> Vec<(String, ProbeRecord)> {
    let Some((token, account_id)) = read_codex_auth(home) else {
        return Vec::new();
    };
    let result = async {
        let mut req = client
            .get(CODEX_USAGE_URL)
            .header("authorization", format!("Bearer {token}"))
            .timeout(PROBE_TIMEOUT);
        if let Some(acct) = &account_id {
            req = req.header("ChatGPT-Account-Id", acct);
        }
        let resp = req.send().await.context("codex request failed")?;
        let body: Value = resp.json().await.context("codex response not JSON")?;
        Ok::<Value, anyhow::Error>(body)
    }
    .await;
    let body = match result {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "codex probe failed");
            return Vec::new();
        }
    };
    // Codex and Brodex both draw from the same ChatGPT plan quota, so one usage
    // probe describes both lanes.
    let mut out = Vec::new();
    if let Some(rec) = parse_codex_usage(Provider::Codex, &body, now) {
        out.push((lane_key(Provider::Codex, None), rec));
    }
    if let Some(rec) = parse_codex_usage(Provider::Brodex, &body, now) {
        out.push((lane_key(Provider::Brodex, None), rec));
    }
    out
}

// ── DeepSeek: balance ───────────────────────────────────────────────────────

/// Parse `api.deepseek.com/user/balance`. PAYG: maps available balance to a
/// capacity (no 5h/7d utilization).
fn parse_deepseek_balance(body: &Value, now: u64) -> ProbeRecord {
    let available = body["is_available"].as_bool().unwrap_or(false);
    let balance = body["balance_infos"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|b| b["total_balance"].as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let capacity = if !available || balance <= 0.0 {
        0.0
    } else {
        (balance / DEEPSEEK_BALANCE_FLOOR_USD).clamp(0.05, 1.0)
    };
    let mut rec = base_record(Provider::Deepseek, None, now);
    rec.quota_confidence = QuotaConfidence::PaygBalance;
    rec.balance_capacity = Some(capacity);
    rec.quota_status = if available && balance > 0.0 {
        QuotaStatus::Known
    } else {
        QuotaStatus::Exhausted
    };
    rec.raw_summary = Some(format!("deepseek balance ${balance:.2} available={available}"));
    rec
}

async fn probe_deepseek(
    client: &reqwest::Client,
    home: &Path,
    now: u64,
) -> Option<(String, ProbeRecord)> {
    let token = read_anthropic_token(&home.join(".claude-ds"))?;
    let result = async {
        let resp = client
            .get(DEEPSEEK_BALANCE_URL)
            .header("authorization", format!("Bearer {token}"))
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
            .context("deepseek request failed")?;
        let body: Value = resp.json().await.context("deepseek response not JSON")?;
        Ok::<Value, anyhow::Error>(body)
    }
    .await;
    match result {
        Ok(body) => Some((
            lane_key(Provider::Deepseek, None),
            parse_deepseek_balance(&body, now),
        )),
        Err(e) => {
            tracing::warn!(error = %e, "deepseek probe failed");
            None
        }
    }
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// Refresh every provider account probe and merge into the store, keyed by the
/// same `lane_key` the allocator scores against. Per-provider failures are
/// logged, not fatal. Returns the number of probes written.
pub async fn refresh_account_probes(store_dir: &Path, home: &Path, now: u64) -> usize {
    let client = reqwest::Client::new();
    let mut probed: Vec<(String, ProbeRecord)> = Vec::new();
    probed.extend(probe_glm(&client, home, now).await);
    if let Some(p) = probe_claude(&client, home, now).await {
        probed.push(p);
    }
    probed.extend(probe_codex(&client, home, now).await);
    if let Some(p) = probe_deepseek(&client, home, now).await {
        probed.push(p);
    }
    if probed.is_empty() {
        return 0;
    }
    let mut store = probe_store_load(store_dir);
    for (key, rec) in &probed {
        tracing::info!(
            %key,
            five_hour = ?rec.five_hour_utilization,
            seven_day = ?rec.seven_day_utilization,
            balance = ?rec.balance_capacity,
            "account probe refreshed"
        );
        store.records.insert(key.clone(), rec.clone());
    }
    probe_store_save(store_dir, &store);
    probed.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zai_quota_extracts_token_windows_not_the_time_limit() {
        let body = serde_json::json!({"data": {"limits": [
            {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 0},
            {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 0, "nextResetTime": 1i64},
            {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 1}
        ], "level": "max"}});
        let rec = parse_zai_quota(None, &body, 1000).unwrap();
        assert_eq!(rec.provider, Provider::Glm);
        assert_eq!(rec.five_hour_utilization, Some(0.0));
        assert_eq!(rec.seven_day_utilization, Some(0.0));
        assert!(matches!(rec.quota_confidence, QuotaConfidence::QuotaProbe));
        assert_eq!(rec.last_probe_at, Some(1000));
    }

    #[test]
    fn parse_zai_quota_percentage_to_fraction_and_exhaustion() {
        let body = serde_json::json!({"data": {"limits": [
            {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 82},
            {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 100}
        ], "level": "max"}});
        let rec = parse_zai_quota(Some("z2".into()), &body, 1).unwrap();
        assert_eq!(rec.five_hour_utilization, Some(0.82));
        assert_eq!(rec.seven_day_utilization, Some(1.0));
        assert!(matches!(rec.quota_status, QuotaStatus::Exhausted));
    }

    #[test]
    fn parse_claude_headers_reads_fractions_directly() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.02".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-7d-utilization",
            "0.17".parse().unwrap(),
        );
        h.insert("anthropic-ratelimit-unified-status", "allowed".parse().unwrap());
        let rec = parse_claude_ratelimit(None, &h, 5).unwrap();
        assert_eq!(rec.provider, Provider::Claude);
        // Already fractions — NOT divided by 100.
        assert_eq!(rec.five_hour_utilization, Some(0.02));
        assert_eq!(rec.seven_day_utilization, Some(0.17));
        assert!(matches!(rec.quota_status, QuotaStatus::Known));
    }

    #[test]
    fn parse_claude_headers_rejected_status_is_exhausted() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            "0.5".parse().unwrap(),
        );
        h.insert("anthropic-ratelimit-unified-status", "rejected".parse().unwrap());
        let rec = parse_claude_ratelimit(None, &h, 0).unwrap();
        assert!(matches!(rec.quota_status, QuotaStatus::Exhausted));
        // No utilization headers at all → no probe.
        assert!(parse_claude_ratelimit(None, &HeaderMap::new(), 0).is_none());
    }

    #[test]
    fn parse_codex_usage_maps_windows_and_keys_both_lanes() {
        let body = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "limit_reached": false,
                "primary_window": {"used_percent": 0},
                "secondary_window": {"used_percent": 17}
            }
        });
        let codex = parse_codex_usage(Provider::Codex, &body, 9).unwrap();
        assert_eq!(codex.five_hour_utilization, Some(0.0));
        assert_eq!(codex.seven_day_utilization, Some(0.17));
        assert!(matches!(codex.quota_status, QuotaStatus::Known));
        // Brodex shares the same plan quota.
        let brodex = parse_codex_usage(Provider::Brodex, &body, 9).unwrap();
        assert_eq!(brodex.provider, Provider::Brodex);
        assert_eq!(brodex.seven_day_utilization, Some(0.17));
    }

    #[test]
    fn parse_codex_usage_limit_reached_is_exhausted() {
        let body = serde_json::json!({"plan_type": "pro", "rate_limit": {
            "limit_reached": true,
            "primary_window": {"used_percent": 100},
            "secondary_window": {"used_percent": 40}
        }});
        let rec = parse_codex_usage(Provider::Codex, &body, 0).unwrap();
        assert!(matches!(rec.quota_status, QuotaStatus::Exhausted));
    }

    #[test]
    fn parse_deepseek_balance_maps_funded_account_to_capacity() {
        let body = serde_json::json!({
            "is_available": true,
            "balance_infos": [{"currency": "USD", "total_balance": "7.52"}]
        });
        let rec = parse_deepseek_balance(&body, 3);
        assert_eq!(rec.provider, Provider::Deepseek);
        assert!(matches!(rec.quota_confidence, QuotaConfidence::PaygBalance));
        // $7.52 ≥ $5 floor → full capacity.
        assert_eq!(rec.balance_capacity, Some(1.0));
        assert!(matches!(rec.quota_status, QuotaStatus::Known));
    }

    #[test]
    fn parse_deepseek_balance_low_and_empty_accounts() {
        // Below floor scales down.
        let low = parse_deepseek_balance(
            &serde_json::json!({"is_available": true, "balance_infos": [{"total_balance": "2.50"}]}),
            0,
        );
        assert_eq!(low.balance_capacity, Some(0.5));
        // Unavailable / empty → exhausted, zero capacity.
        let empty = parse_deepseek_balance(
            &serde_json::json!({"is_available": false, "balance_infos": [{"total_balance": "0.00"}]}),
            0,
        );
        assert_eq!(empty.balance_capacity, Some(0.0));
        assert!(matches!(empty.quota_status, QuotaStatus::Exhausted));
    }
}
