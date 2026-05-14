use super::*;
use crate::*;

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
