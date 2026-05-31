//! Account utilization probers.
//!
//! Fetch real per-account quota/headroom from each provider's usage surface and
//! write it into the allocator [`ProbeStore`], so `quota_capacity` differentiates
//! providers by genuine remaining headroom instead of every lane tying — a tie
//! collapses tier selection onto the deterministic tiebreak (the brodex-wins
//! symptom). The allocator already *consumes* these signals; this module is the
//! missing producer.
//!
//! Spec: `design/orchestration/supervision/acquire-drone.md` §6. v1 implements
//! the GLM/Z.AI `zai-usage-endpoint` mechanism end to end; Claude
//! (unified-rate-limit headers), Codex (`wham/usage`), and DeepSeek (balance)
//! follow the same shape and land next.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::orchestration::allocator::{
    CredentialStatus, ProbeRecord, QuotaConfidence, QuotaStatus, lane_key, probe_store_load,
    probe_store_save,
};
use crate::orchestration::providers::Provider;

const ZAI_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One discoverable provider account: provider, the lane account name the
/// allocator keys on (`None` = the provider default lane), and the bearer token.
struct AccountToken {
    provider: Provider,
    account: Option<String>,
    token: String,
}

/// Read `ANTHROPIC_AUTH_TOKEN` from a Claude-compatible config dir's
/// `settings.json` (mirrors `brofile::default_claude_compatible_env`).
fn read_anthropic_token(config_dir: &Path) -> Option<String> {
    let body = std::fs::read_to_string(config_dir.join("settings.json")).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    v["env"]["ANTHROPIC_AUTH_TOKEN"]
        .as_str()
        .map(str::to_string)
}

/// Discover GLM/Z.AI accounts: the default `~/.claude-zai` plus any
/// `~/.claude-zai-<name>` variant. Account name `None` for the default keeps the
/// probe key aligned with the allocator's default lane (`glm:default`).
fn discover_zai_accounts(home: &Path) -> Vec<AccountToken> {
    let mut out = Vec::new();
    if let Some(token) = read_anthropic_token(&home.join(".claude-zai")) {
        out.push(AccountToken {
            provider: Provider::Glm,
            account: None,
            token,
        });
    }
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(suffix) = name.strip_prefix(".claude-zai-") {
                if suffix.is_empty() {
                    continue;
                }
                if let Some(token) = read_anthropic_token(&entry.path()) {
                    out.push(AccountToken {
                        provider: Provider::Glm,
                        account: Some(suffix.to_string()),
                        token,
                    });
                }
            }
        }
    }
    out
}

/// Parse a z.ai `monitor/usage/quota/limit` response into a [`ProbeRecord`].
/// Pure — unit-tested against the live response shape. Discriminates the two
/// `TOKENS_LIMIT` rows by `(number, unit)`: `(5, 3)` is the five-hour window,
/// `(1, 6)` the weekly/seven-day window. `percentage` is 0..100.
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
    Ok(ProbeRecord {
        provider: Provider::Glm,
        account,
        credential_status: CredentialStatus::Present,
        quota_status: if exhausted {
            QuotaStatus::Exhausted
        } else {
            QuotaStatus::Known
        },
        quota_confidence: QuotaConfidence::QuotaProbe,
        five_hour_utilization: five_hour,
        seven_day_utilization: seven_day,
        balance_capacity: None,
        cooldown_until: None,
        last_probe_at: Some(now),
        last_runtime_observation_at: None,
        raw_summary: Some(format!("zai quota probe, plan level={level}")),
    })
}

/// Probe one z.ai account over HTTP and parse the result.
async fn probe_zai_account(
    client: &reqwest::Client,
    acct: &AccountToken,
    now: u64,
) -> Result<ProbeRecord> {
    let resp = client
        .get(ZAI_QUOTA_URL)
        .header("Authorization", &acct.token)
        .header("Accept-Language", "en-US,en")
        .header("Content-Type", "application/json")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .context("z.ai quota request failed")?;
    let body: Value = resp.json().await.context("z.ai quota response not JSON")?;
    parse_zai_quota(acct.account.clone(), &body, now)
}

/// Refresh every discoverable account probe and merge into the store, keyed by
/// the same `lane_key` the allocator scores against. Per-account failures are
/// logged, not fatal. Returns the number of probes written.
pub async fn refresh_account_probes(store_dir: &Path, home: &Path, now: u64) -> usize {
    let accounts = discover_zai_accounts(home);
    if accounts.is_empty() {
        return 0;
    }
    let client = reqwest::Client::new();
    let mut store = probe_store_load(store_dir);
    let mut written = 0usize;
    for acct in &accounts {
        match probe_zai_account(&client, acct, now).await {
            Ok(record) => {
                let key = lane_key(record.provider, record.account.as_deref());
                tracing::info!(
                    %key,
                    five_hour = ?record.five_hour_utilization,
                    seven_day = ?record.seven_day_utilization,
                    "account probe refreshed"
                );
                store.records.insert(key, record);
                written += 1;
            }
            Err(e) => tracing::warn!(
                provider = ?acct.provider,
                account = ?acct.account,
                error = %e,
                "account probe failed"
            ),
        }
    }
    if written > 0 {
        probe_store_save(store_dir, &store);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact live response captured from the Z.AI quota endpoint.
    fn live_response() -> Value {
        serde_json::json!({
            "code": 200,
            "msg": "Operation successful",
            "data": {
                "limits": [
                    {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 0},
                    {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 0,
                     "nextResetTime": 1780397143975i64},
                    {"type": "TIME_LIMIT", "unit": 5, "number": 1, "usage": 4000,
                     "currentValue": 1, "remaining": 3999, "percentage": 1}
                ],
                "level": "max"
            },
            "success": true
        })
    }

    #[test]
    fn parse_zai_quota_extracts_both_token_windows_not_the_time_limit() {
        let rec = parse_zai_quota(None, &live_response(), 1000).unwrap();
        assert_eq!(rec.provider, Provider::Glm);
        // 5h window: TOKENS_LIMIT number=5 unit=3, percentage 0 → 0.0
        assert_eq!(rec.five_hour_utilization, Some(0.0));
        // 7d window: TOKENS_LIMIT number=1 unit=6, percentage 0 → 0.0
        assert_eq!(rec.seven_day_utilization, Some(0.0));
        // The TIME_LIMIT row (percentage 1 = web-tool count) must NOT be read as
        // a token window — that's the mislabel the spec warns about.
        assert_ne!(rec.five_hour_utilization, Some(0.01));
        assert!(matches!(rec.quota_confidence, QuotaConfidence::QuotaProbe));
        assert!(matches!(rec.quota_status, QuotaStatus::Known));
        assert_eq!(rec.last_probe_at, Some(1000));
    }

    #[test]
    fn parse_zai_quota_maps_percentage_to_fraction_and_flags_exhaustion() {
        let mut body = live_response();
        body["data"]["limits"][0]["percentage"] = serde_json::json!(82);
        body["data"]["limits"][1]["percentage"] = serde_json::json!(100);
        let rec = parse_zai_quota(Some("acct2".into()), &body, 1).unwrap();
        assert_eq!(rec.five_hour_utilization, Some(0.82));
        assert_eq!(rec.seven_day_utilization, Some(1.0));
        // 7d fully used → exhausted.
        assert!(matches!(rec.quota_status, QuotaStatus::Exhausted));
        assert_eq!(rec.account.as_deref(), Some("acct2"));
    }

    #[test]
    fn parse_zai_quota_rejects_a_response_with_no_token_windows() {
        let body = serde_json::json!({"data": {"limits": [
            {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 1}
        ], "level": "max"}});
        assert!(parse_zai_quota(None, &body, 0).is_err());
    }
}
