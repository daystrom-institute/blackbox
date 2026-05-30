use std::sync::Arc;

use serde_json::{Value, json};

use crate::packets::apply_with as apply_packet_with;
use crate::routing;
use crate::server::SharedState;
use crate::server::routes::dispatch_verdict;

/// Inbound `proposal-approved` / `proposal-clarify` signal hook for
/// the Slack daily brief. Fires when a reaction (approve) or thread
/// reply (clarify) lands on a posted triage proposal AND no workflow
/// was waiting for the signal. Resolves the message back to its
/// SlackProposalLink and posts a threaded acknowledgement in Slack.
/// The actual apply work and the bro_resume refinement loop drop in
/// here once the foreach-driven Badgey workflow stack is wired —
/// then this hook becomes the call site for
/// `badgey_apply_proposal_internal` (approve) and `bro_resume`
/// (clarify). Errors are logged, never bubbled — best-effort
/// observability path.
pub(crate) async fn try_slack_proposal_signal_hook(
    signal: &str,
    state: &Arc<SharedState>,
    correlate: &serde_json::Map<String, Value>,
    entity: &Value,
) {
    let thread_ts = correlate
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if thread_ts.is_empty() {
        return;
    }
    let team_id = entity.get("team_id").and_then(|v| v.as_str()).unwrap_or("");
    let channel_id = entity.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    if team_id.is_empty() || channel_id.is_empty() {
        return;
    }
    let link = match state
        .slack_proposal_links
        .lookup_by_msg(team_id, channel_id, thread_ts)
    {
        Some(l) => l,
        None => return,
    };
    let user = entity
        .get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("someone");
    let bbox_user = entity
        .get("bbox_user")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let acknowledger = if bbox_user.is_empty() {
        format!("<@{user}>")
    } else {
        format!("<@{user}> ({bbox_user})")
    };
    let text = match signal {
        "proposal-approved" => format!(
            ":white_check_mark: Approved by {acknowledger}. \
             Apply path lands with the foreach-driven Badgey workflow — \
             logged for follow-up. (proposal `{}`)",
            link.proposal_id
        ),
        "proposal-clarify" => {
            let reply_text = entity.get("text").and_then(|v| v.as_str()).unwrap_or("");
            // Char-aware truncation — naive byte slicing can panic on
            // non-ASCII at codepoint boundaries.
            let snippet = if reply_text.chars().count() > 120 {
                let truncated: String = reply_text.chars().take(120).collect();
                format!("{truncated}…")
            } else {
                reply_text.to_string()
            };
            format!(
                ":speech_balloon: Heard your follow-up from {acknowledger}{}. \
                 Refinement loop lands with the foreach-driven Badgey workflow — \
                 the proposal author isn't a live agent yet. \
                 (proposal `{}`)",
                if snippet.is_empty() {
                    String::new()
                } else {
                    format!(": _{snippet}_")
                },
                link.proposal_id,
            )
        }
        _ => return,
    };
    if signal == "proposal-approved" {
        if let Err(e) = state
            .slack_proposal_links
            .bump_version(team_id, channel_id, thread_ts)
        {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                "bump_version on slack proposal link failed: {e}"
            );
        }
    }
    let token = match std::env::var("SLACK_BOT_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t,
        _ => {
            tracing::info!(
                proposal_id = %link.proposal_id,
                signal = %signal,
                "proposal hook fired but SLACK_BOT_TOKEN unset; skipping ack post"
            );
            return;
        }
    };
    let req_body = json!({
        "channel": channel_id,
        "thread_ts": thread_ts,
        "text": text,
        "mrkdwn": true,
    });
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                "building reqwest client for Slack ack failed: {e}"
            );
            return;
        }
    };
    match client
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(&token)
        .json(&req_body)
        .send()
        .await
    {
        Ok(resp) => {
            let parsed: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %link.proposal_id,
                        "parsing Slack ack response failed: {e}"
                    );
                    return;
                }
            };
            if !parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                tracing::warn!(
                    proposal_id = %link.proposal_id,
                    signal = %signal,
                    "Slack ack post returned ok=false: {parsed}"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                proposal_id = %link.proposal_id,
                signal = %signal,
                "Slack ack post failed: {e}"
            );
        }
    }
}

/// Apply a routing packet to an extracted entity and dispatch the
/// resulting verdict. The shared dispatch entry point used by every
/// event inlet (webhooks AND pollers) — both reduce to "I have an
/// entity + a routing-packet id, route it." Inlet-specific concerns
/// (signature verify, schedule, dedup) live in the caller.
pub(crate) async fn dispatch_routed_event(
    state: Arc<SharedState>,
    inlet_name: &str,
    routing_packet_id: &str,
    entity: Value,
    default_project_dir: Option<String>,
) -> anyhow::Result<Value> {
    let prediction = {
        let store = state.packets.read();
        let packet = store
            .load(routing_packet_id)
            .map_err(|e| anyhow::anyhow!("routing packet load: {e}"))?;
        apply_packet_with(&packet, &entity, &*store)
    };
    let consequent_json = match prediction {
        Some(p) => p.consequent.to_json(),
        None => {
            tracing::warn!(
                "{inlet_name}: routing packet '{routing_packet_id}' produced no_match — dead-lettering",
            );
            return Ok(json!({
                "status": "no_match",
                "reason": "routing packet returned no_match (default → dead-letter)",
                "extracted_entity": entity,
            }));
        }
    };
    let resolved_consequent = routing::resolve_entity_template(&entity, &consequent_json);
    let verdict = routing::RoutingVerdict::parse(&resolved_consequent)
        .map_err(|e| anyhow::anyhow!("verdict parse: {e}"))?;
    dispatch_verdict(state, inlet_name, default_project_dir, verdict, entity).await
}

/// Dispatch a pre-built RoutingVerdict directly, skipping the
/// routing-packet evaluation step. Used by the whiteboard transition
/// path: when a phase advances, the engine knows the verdict shape
/// (always `signal_arc { signal: "board-transitioned", correlate: ... }`),
/// no extractor or packet round-trip needed.
pub(crate) async fn dispatch_routing_verdict_direct(
    state: Arc<SharedState>,
    inlet_name: &str,
    verdict: routing::RoutingVerdict,
    entity: Value,
) -> anyhow::Result<Value> {
    dispatch_verdict(state, inlet_name, None, verdict, entity).await
}

#[cfg(test)]
mod tests {
    // Async env-mutating tests hold the std `test_env_lock` across `.await` on
    // purpose (env must stay set while the awaited code reads it); #[tokio::test]
    // is single-threaded so this can't deadlock the runtime.
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use crate::server::state::BlackboxServer;
    use crate::slack_proposal_links;
    use crate::util;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }
    #[tokio::test]
    async fn proposal_approved_hook_bumps_link_version() {
        let _env = crate::util::test_env_lock();
        // Verifies the dispatch_verdict signal hook resolves a Slack
        // message back to its SlackProposalLink and bumps the version
        // on `proposal-approved`. The HTTP ack post is short-circuited
        // (no SLACK_BOT_TOKEN set in the test env) but the bump
        // happens before the token check.
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let link = slack_proposal_links::SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C01".into(),
            msg_ts: "ts1".into(),
            proposal_id: "triage-1".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/x".into(),
            posted_at: util::now_iso(),
        };
        server.state.slack_proposal_links.record(link).unwrap();
        let mut correlate = serde_json::Map::new();
        correlate.insert("thread_ts".into(), Value::String("ts1".into()));
        let entity = json!({
            "team_id": "T01",
            "channel": "C01",
            "user": "Ualice",
            "bbox_user": "alice",
        });
        // Ensure no token leaks in from the surrounding env so the
        // hook short-circuits before HTTP. (Safety belt — the test
        // depends on the bump happening before the token check.)
        unsafe {
            std::env::remove_var("SLACK_BOT_TOKEN");
        }
        try_slack_proposal_signal_hook("proposal-approved", &server.state, &correlate, &entity)
            .await;
        let bumped = server
            .state
            .slack_proposal_links
            .lookup_by_msg("T01", "C01", "ts1")
            .unwrap();
        assert_eq!(bumped.version, 2);
    }

    #[tokio::test]
    async fn proposal_clarify_hook_does_not_bump_version() {
        let _env = crate::util::test_env_lock();
        // Clarify hook resolves the message back to its link and
        // (will eventually) post a stub reply, but does NOT bump the
        // link version — version is reserved for chat.update of the
        // original proposal post when a refined version lands.
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let link = slack_proposal_links::SlackProposalLink {
            team_id: "T01".into(),
            channel_id: "C01".into(),
            msg_ts: "ts2".into(),
            proposal_id: "triage-2".into(),
            instance_id: None,
            authoring_session_id: None,
            version: 1,
            project_dir: "/repo/x".into(),
            posted_at: util::now_iso(),
        };
        server.state.slack_proposal_links.record(link).unwrap();
        let mut correlate = serde_json::Map::new();
        correlate.insert("thread_ts".into(), Value::String("ts2".into()));
        let entity = json!({
            "team_id": "T01",
            "channel": "C01",
            "user": "Ualice",
            "text": "actually never mind, this one is fine as-is",
        });
        unsafe {
            std::env::remove_var("SLACK_BOT_TOKEN");
        }
        try_slack_proposal_signal_hook("proposal-clarify", &server.state, &correlate, &entity)
            .await;
        let unchanged = server
            .state
            .slack_proposal_links
            .lookup_by_msg("T01", "C01", "ts2")
            .unwrap();
        assert_eq!(unchanged.version, 1);
    }

    #[tokio::test]
    async fn proposal_signal_hook_no_op_for_unknown_thread_ts() {
        let _env = crate::util::test_env_lock();
        // No SlackProposalLink for the correlated thread_ts → hook is
        // a silent no-op. Any other in-thread reply or reaction in
        // the workspace should NOT cause stub acks.
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let mut correlate = serde_json::Map::new();
        correlate.insert("thread_ts".into(), Value::String("ts-unknown".into()));
        let entity = json!({
            "team_id": "T01",
            "channel": "C01",
            "user": "Ualice",
        });
        unsafe {
            std::env::remove_var("SLACK_BOT_TOKEN");
        }
        try_slack_proposal_signal_hook("proposal-approved", &server.state, &correlate, &entity)
            .await;
        // Nothing to assert beyond "did not panic" — but make a
        // sanity probe on the link store size to confirm we didn't
        // accidentally insert anything.
        assert!(
            server
                .state
                .slack_proposal_links
                .lookup_by_msg("T01", "C01", "ts-unknown")
                .is_none()
        );
    }
}
