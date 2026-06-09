use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use futures::StreamExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::dispatch::try_slack_proposal_signal_hook;
use super::state::{ArcSnapshot, BlackboxServer, SharedState, SignalEvent, WebhookDelivery};
use super::workflow_capabilities::validate_workflow_capabilities;
use crate::artifacts::{
    self, ArtifactInstallParams, ArtifactListParams, ArtifactRemoveParams, ArtifactSupersedeParams,
};
use crate::chunker;
use crate::crons;
use crate::edge_index;
use crate::embed_queue;
use crate::entity_ref;
use crate::index;
use crate::orchestration;
use crate::orchestration::providers::Provider;
use crate::packets::{self, apply_with as apply_packet_with};
use crate::pollers;
use crate::projects::ProjectRecord;
use crate::routing;
use crate::tools::bro_helpers::{
    build_member_entry, infer_provider_from_path, roster_entry_key, split_csv,
};
use crate::tools::bro_params::{
    BroadcastParams, CancelParams, DashboardParams, ExecParams, InterruptParams, ResumeParams,
    StatusParams, SteerParams,
};
use crate::tools::bro_runtime_params::{
    BroRosterEntry, OrchestrateListEntry, OrchestrateRequest, OrchestrateStatusQuery,
    OrchestrateStatusResponse, RosterQuery,
};
use crate::util;
use crate::webhooks;
use crate::workflow;

pub(crate) async fn orchestrate_list_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    // Snapshot threads + notes via the raw stores so we don't hold
    // any parking_lot guard across an await (there aren't any awaits
    // in this handler, but the pattern is still cleaner).
    let entries: Vec<OrchestrateListEntry> = {
        let threads = state.threads.read();
        let notes = state.notes.read();
        let mut out: Vec<OrchestrateListEntry> = threads
            .all()
            .iter()
            .filter(|t| {
                matches!(t.kind, Some(crate::threads::ThreadKind::WorkItem))
                    && t.name.as_deref().is_some_and(|n| n.starts_with("wf-"))
            })
            .map(|t| {
                let tid = &t.id;
                let mut latest_anchor: Option<(String, String)> = None;
                let mut final_status: Option<String> = None;
                let mut note_count = 0usize;
                for n in notes.all() {
                    if n.thread_id.as_deref() != Some(tid.as_str()) {
                        continue;
                    }
                    note_count += 1;
                    let body = n.body.as_str();
                    if body.starts_with("ANCHOR ") {
                        let is_newer = latest_anchor
                            .as_ref()
                            .map(|(ts, _)| n.created_at.as_str() > ts.as_str())
                            .unwrap_or(true);
                        if is_newer {
                            latest_anchor = Some((n.created_at.clone(), body.to_string()));
                        }
                    }
                    if body.starts_with("workflow ") && body.contains("completed in") {
                        final_status = Some("completed".into());
                    } else if body.starts_with("workflow errored") {
                        final_status = Some("errored".into());
                    } else if body.starts_with("paused at user node") {
                        final_status = Some("paused".into());
                    } else if body.starts_with("policy halt") {
                        final_status = Some("policy_halt".into());
                    }
                }
                OrchestrateListEntry {
                    thread_id: t.id.clone(),
                    name: t.name.clone(),
                    topic: t.topic.clone(),
                    status: t.status.as_ref().to_string(),
                    created_at: t.created_at.clone(),
                    last_activity: t.last_activity.clone(),
                    project: if t.project.is_empty() {
                        None
                    } else {
                        Some(t.project.clone())
                    },
                    latest_anchor: latest_anchor.map(|(_, b)| b),
                    final_status,
                    note_count,
                }
            })
            .collect();
        out.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        out
    };
    axum::Json(entries).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrchestratePeekQuery {
    /// Optional thread_id filter — when set, return only that arc's
    /// snapshot. When absent, return all running_arcs entries.
    #[serde(default)]
    thread_id: Option<String>,
}

pub(crate) async fn orchestrate_peek_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(q): Query<OrchestratePeekQuery>,
) -> impl axum::response::IntoResponse {
    let map = state.running_arcs.read();
    match q.thread_id {
        Some(tid) => match map.get(&tid) {
            Some(s) => axum::Json(serde_json::to_value(s).unwrap_or_default()),
            None => axum::Json(serde_json::json!({
                "error": format!("no arc snapshot for thread_id={tid}")
            })),
        },
        None => {
            let mut all: Vec<&ArcSnapshot> = map.values().collect();
            all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            axum::Json(serde_json::to_value(&all).unwrap_or_default())
        }
    }
}

pub(crate) async fn orchestrate_status_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(q): Query<OrchestrateStatusQuery>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let requested_id = q.thread_id;
    let resolved_thread_id = resolve_orchestrate_thread_id(&state, &requested_id);
    // Snapshot notes linked to this thread via the raw store.
    let entries: Vec<Value> = {
        let store = state.notes.read();
        store
            .all()
            .iter()
            .filter(|n| n.thread_id.as_deref() == Some(resolved_thread_id.as_str()))
            .map(|n| serde_json::to_value(n).unwrap_or_default())
            .collect()
    };
    let latest_anchor = entries
        .iter()
        .filter(|e| {
            e.get("body")
                .and_then(Value::as_str)
                .map(|b| b.starts_with("ANCHOR "))
                .unwrap_or(false)
        })
        .max_by_key(|e| {
            e.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        })
        .and_then(|e| e.get("body").and_then(Value::as_str).map(String::from));
    axum::Json(OrchestrateStatusResponse {
        thread_id: resolved_thread_id,
        notes: entries,
        latest_anchor,
    })
    .into_response()
}

pub(crate) fn resolve_orchestrate_thread_id(state: &SharedState, requested_id: &str) -> String {
    if requested_id.starts_with("thread-") {
        return requested_id.to_string();
    }

    state
        .running_arcs
        .read()
        .values()
        .find(|snapshot| snapshot.arc_id == requested_id || snapshot.arc_thread_id == requested_id)
        .map(|snapshot| snapshot.arc_thread_id.clone())
        .unwrap_or_else(|| requested_id.to_string())
}

pub(crate) async fn orchestrate_stream_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateRequest>,
) -> axum::response::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::Sse;
    use axum::response::sse::Event;
    let compiled = workflow::compile(req.workflow);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

    // Kick off the run on a background task; events stream via tx.
    tokio::spawn(async move {
        let state_clone = state.clone();
        let server = BlackboxServer::new(state_clone);
        match compiled {
            Err(e) => {
                let _ = tx.send(json!({
                    "kind": "compile_error",
                    "data": {"message": e.to_string()},
                    "timestamp": crate::util::now_iso(),
                }));
            }
            Ok(compiled) => {
                let result = workflow::run_workflow_streaming(
                    &server,
                    &compiled,
                    req.project_dir,
                    req.max_steps,
                    tx.clone(),
                )
                .await;
                // Terminal frame: the full result. Clients should
                // detect `kind: "result"` as end-of-run.
                let _ = tx.send(json!({
                    "kind": "result",
                    "data": result,
                    "timestamp": crate::util::now_iso(),
                }));
            }
        }
        // tx dropped here closes the stream.
    });

    let stream = async_stream::stream! {
        while let Some(ev) = rx.recv().await {
            let s = ev.to_string();
            yield Ok::<_, std::convert::Infallible>(Event::default().data(s));
        }
    };
    Sse::new(stream)
}

pub(crate) async fn orchestrate_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateRequest>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let compiled = match workflow::compile(req.workflow) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile failed: {e}"),
            )
                .into_response();
        }
    };
    // Stateful capability validation must gate this path too. The MCP
    // `bro_orchestrate_run` path already validates before dry-run; keep the
    // plain HTTP dry-run on the same validation path.
    if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("capability validation: {e}"),
        )
            .into_response();
    }
    if req.dry_run {
        return axum::Json(workflow::engine::dry_run(&compiled)).into_response();
    }
    let server = BlackboxServer::new(state);
    let result = workflow::run_workflow(&server, &compiled, req.project_dir, req.max_steps).await;
    axum::Json(result).into_response()
}

/// HTTP webhook ingestion endpoint. URL: `POST /webhook/:name`.
///
/// Pipeline (in order):
///   1. Look up WebhookSpec by name (404 if unknown)
///   2. Verify signature scheme against headers + raw body
///   3. Optional delivery-id dedup (Forgejo: X-Gitea-Delivery)
///   4. Run extractor over payload → flat entity
///   5. Apply routing packet → RoutingVerdict
///   6. Dispatch verdict (start_arc | signal_arc | cancel_arc | ignore | dead_letter)
pub(crate) async fn webhook_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let header_map = headers_to_lowercase_map(&headers);
    let header_subset = header_subset_for_log(&header_map);
    let body_bytes: &[u8] = &body;
    let outcome = process_webhook(&state, &name, &header_map, body_bytes).await;
    let (status, response_body) = match &outcome {
        Ok(v) => (200u16, v.clone()),
        Err(e) => (400u16, json!({"error": e.to_string()})),
    };
    let entity = response_body
        .get("extracted_entity")
        .cloned()
        .unwrap_or(Value::Null);
    let verdict_classification = response_body
        .get("status")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if status == 200 {
                "unknown".into()
            } else {
                "error".into()
            }
        });
    state.record_webhook(WebhookDelivery {
        received_at: util::now_iso(),
        webhook_name: name.clone(),
        source: "webhook".into(),
        headers: header_subset,
        extracted_entity: entity,
        verdict_classification,
        response_status: status,
        response_body: response_body.clone(),
    });
    match outcome {
        Ok(verdict_json) => (axum::http::StatusCode::OK, axum::Json(verdict_json)).into_response(),
        Err(e) => {
            tracing::warn!("webhook /{name}: {e}");
            (
                axum::http::StatusCode::BAD_REQUEST,
                format!("webhook error: {e}"),
            )
                .into_response()
        }
    }
}

/// Replay an arbitrary payload through a webhook's extractor + routing
/// packet WITHOUT dispatching the verdict. Returns the extracted entity
/// + routing verdict so authors can debug without firing arcs.
/// URL: `POST /webhook/:name/replay`. Skips signature verification.
pub(crate) async fn webhook_replay_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    let header_map = headers_to_lowercase_map(&headers);
    use axum::response::IntoResponse;
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("payload not JSON: {e}"),
            )
                .into_response();
        }
    };
    match webhook_replay_inner(&state, &name, &payload, &header_map) {
        Ok(response_body) => axum::Json(response_body).into_response(),
        Err((status, msg)) => (status, msg).into_response(),
    }
}

/// Shared replay path used by both the HTTP `/webhook/:name/replay`
/// endpoint and the `bro_webhook_replay` MCP tool. Records the result
/// into the delivery ring buffer with `source: replay`.
pub(crate) fn webhook_replay_inner(
    state: &Arc<SharedState>,
    name: &str,
    payload: &Value,
    headers: &HashMap<String, String>,
) -> Result<Value, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let spec = state
        .webhooks
        .get(name)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown webhook '{name}'")))?;
    let combined = combine_payload_and_headers(payload, headers);
    let entity = spec
        .extractor
        .extract(&combined)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("extractor failed: {e}")))?;
    let prediction = {
        let store = state.packets.read();
        let packet = store.load(&spec.routing_packet).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("routing packet load: {e}"),
            )
        })?;
        apply_packet_with(&packet, &entity, &*store)
    };
    let verdict_kind = prediction
        .as_ref()
        .map(|p| p.classification.clone())
        .unwrap_or_else(|| "no_match".into());
    let verdict = prediction.map(|p| p.consequent.to_json());
    let response_body = json!({
        "entity": entity.clone(),
        "verdict_classification": verdict_kind.clone(),
        "verdict_consequent": verdict,
    });
    state.record_webhook(WebhookDelivery {
        received_at: util::now_iso(),
        webhook_name: name.to_string(),
        source: "replay".into(),
        headers: header_subset_for_log(headers),
        extracted_entity: entity,
        verdict_classification: verdict_kind,
        response_status: 200,
        response_body: response_body.clone(),
    });
    Ok(response_body)
}

/// Subset of inbound headers preserved in the webhook delivery log.
/// Lowercased `x-*` headers carry the routing-relevant signal (event
/// type, delivery id, signature header). Bulk Forgejo/GitHub
/// boilerplate (`accept`, `user-agent`, `content-length`) and the
/// signature value itself are dropped — keeps the buffer small and
/// avoids leaking signature bytes into the read surface.
pub(crate) fn header_subset_for_log(
    headers: &HashMap<String, String>,
) -> serde_json::Map<String, Value> {
    headers
        .iter()
        .filter(|(k, _)| k.starts_with("x-"))
        .filter(|(k, _)| !k.contains("signature"))
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect()
}

pub(crate) fn headers_to_lowercase_map(headers: &axum::http::HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_lowercase(), s.to_string()))
        })
        .collect()
}

/// Wrap a webhook body into a single Value that the Extractor can
/// project from. Body fields stay at the top level (so canonical
/// `$.action` / `$.pull_request.number` paths work) and headers are
/// available under `$._headers.<name>` for header-driven routing
/// (Forgejo's event type is in `X-Gitea-Event`, not the body).
pub(crate) fn combine_payload_and_headers(
    payload: &Value,
    headers: &HashMap<String, String>,
) -> Value {
    let mut map = match payload {
        Value::Object(m) => m.clone(),
        other => {
            let mut m = serde_json::Map::new();
            m.insert("_payload".into(), other.clone());
            m
        }
    };
    let header_obj: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    map.insert("_headers".into(), Value::Object(header_obj));
    Value::Object(map)
}

/// True iff the bind host string resolves to a loopback address.
/// Recognized: `127.0.0.0/8` literals, `localhost` (string match —
/// resolution is host-config dependent and we keep it conservative),
/// `::1`. `0.0.0.0` and any other IPv4 are treated as non-loopback.
pub(crate) fn is_loopback_bind(bind_host: &str) -> bool {
    let h = bind_host.trim();
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

pub(crate) async fn process_webhook(
    state: &Arc<SharedState>,
    name: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> anyhow::Result<Value> {
    let spec = state
        .webhooks
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown webhook '{name}'"))?;

    // Signature verification (loopback flag controls the `none`
    // scheme escape hatch — defense in depth alongside install_check).
    webhooks::verify_signature(&spec.signature, headers, body, state.bind_is_loopback)
        .map_err(|e| anyhow::anyhow!("signature: {e}"))?;

    // Delivery-id dedup (idempotency).
    let delivery_id = spec
        .delivery_id_header
        .as_deref()
        .and_then(|h| headers.get(&h.to_lowercase()))
        .map(|s| s.as_str());
    if !state.webhooks.check_delivery_id(name, delivery_id) {
        tracing::info!(
            "webhook '{name}': dropped duplicate delivery {:?}",
            delivery_id
        );
        return Ok(json!({"status": "duplicate_dropped"}));
    }

    let payload: Value =
        serde_json::from_slice(body).map_err(|e| anyhow::anyhow!("payload not JSON: {e}"))?;

    // Combined extractor input: payload fields at top level (so
    // ordinary Forgejo paths like `$.action`, `$.pull_request.number`
    // work) PLUS `_headers` for header-driven event-type routing.
    let combined = combine_payload_and_headers(&payload, headers);

    // Project payload via extractor.
    let entity = spec
        .extractor
        .extract(&combined)
        .map_err(|e| anyhow::anyhow!("extractor: {e}"))?;

    // Apply routing packet.
    let prediction = {
        let store = state.packets.read();
        let packet = store
            .load(&spec.routing_packet)
            .map_err(|e| anyhow::anyhow!("routing packet load: {e}"))?;
        apply_packet_with(&packet, &entity, &*store)
    };

    let consequent_json = match prediction {
        Some(p) => p.consequent.to_json(),
        None => {
            tracing::warn!(
                "webhook '{name}': routing packet '{}' produced no_match — dead-lettering. entity={}",
                spec.routing_packet,
                entity
            );
            return Ok(json!({
                "status": "no_match",
                "reason": "routing packet returned no_match (default → dead-letter)",
                "extracted_entity": entity,
            }));
        }
    };

    // Resolve `${entity.X}` references inside the routing verdict
    // (typed: `${entity.pr_number}` becomes `Number(117)`, not the
    // string `"117"`) so routing rules can carry typed correlation
    // tuples + payload selections without the rule author hand-
    // encoding entity scalars.
    let resolved_consequent = routing::resolve_entity_template(&entity, &consequent_json);
    let verdict = routing::RoutingVerdict::parse(&resolved_consequent)
        .map_err(|e| anyhow::anyhow!("verdict parse: {e}"))?;

    dispatch_verdict(
        state.clone(),
        &spec.name,
        spec.default_project_dir.clone(),
        verdict,
        entity,
    )
    .await
}

pub(crate) async fn dispatch_verdict(
    state: Arc<SharedState>,
    inlet_name: &str,
    default_project_dir: Option<String>,
    verdict: routing::RoutingVerdict,
    entity: Value,
) -> anyhow::Result<Value> {
    use routing::RoutingVerdict;
    match verdict {
        RoutingVerdict::Ignore => Ok(json!({"status": "ignored"})),
        RoutingVerdict::DeadLetter { reason } => {
            tracing::warn!("{inlet_name}: dead-lettered (reason={:?})", reason);
            Ok(json!({
                "status": "dead_letter",
                "reason": reason,
                "extracted_entity": entity,
            }))
        }
        RoutingVerdict::SignalArc {
            signal,
            correlate,
            payload,
        } => {
            // Carry the routing verdict's payload (or, when absent,
            // the full extracted entity) through to the resumed wait
            // as `${last_signal.payload}`. Without this hooks like
            // `set_var feedback_text = ${last_signal.payload.review.body}`
            // would only see the correlation tuple.
            let signal_payload = payload.unwrap_or_else(|| entity.clone());
            let resolved =
                signal_arc_dispatch(&state, &signal, correlate.clone(), signal_payload).await;
            // Slack proposal-approved hook: when a `proposal-approved`
            // signal falls idle (no workflow waiting) AND the reacted
            // message maps to a posted triage proposal, acknowledge in
            // Slack and bump the link version. Real apply (CAS to
            // BadgeyProposalStore + dispatched task) drops in trivially
            // once §6.3 sub-bro authoring stores proposals under a
            // registered BadgeyInstance.
            if matches!(signal.as_str(), "proposal-approved" | "proposal-clarify")
                && resolved.get("status").and_then(|v| v.as_str()) == Some("no_matching_wait")
            {
                try_slack_proposal_signal_hook(&signal, &state, &correlate, &entity).await;
            }
            Ok(resolved)
        }
        RoutingVerdict::CancelArc { correlate } => {
            // Match running arcs whose pending-wait correlation is a
            // superset of `correlate`: every key in the verdict's
            // tuple must be present with the same value somewhere on
            // the arc's wait registrations. Empty correlate matches
            // every running arc (the broadcast-cancel form). Each
            // matching arc gets its CancellationToken tripped.
            let mut cancelled: Vec<String> = Vec::new();
            // Snapshot the wait store and find arc ids whose
            // registrations contain a tuple matching `correlate`.
            let snapshot = state.wait_store.snapshot();
            let mut matching_arc_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for w in snapshot {
                let matches = correlate.is_empty()
                    || correlate
                        .iter()
                        .all(|(k, v)| w.correlation.get(k) == Some(v));
                if matches {
                    matching_arc_ids.insert(w.arc_id);
                }
            }
            for arc_id in matching_arc_ids {
                if state.cancel_arc(&arc_id) {
                    cancelled.push(arc_id);
                }
            }
            Ok(json!({
                "status": "cancel_arc_dispatched",
                "cancelled_arcs": cancelled,
                "correlate": correlate,
            }))
        }
        RoutingVerdict::StartArc {
            workflow: workflow_id,
            initial_vars,
        } => {
            let registry = state.workflow_registry.clone();
            let spec_clone = {
                let map = registry.read();
                map.get(&workflow_id).cloned()
            };
            let workflow_spec = spec_clone.ok_or_else(|| {
                anyhow::anyhow!("start_arc verdict references unknown workflow id '{workflow_id}'")
            })?;
            let compiled = workflow::compile(workflow_spec)
                .map_err(|e| anyhow::anyhow!("workflow compile: {e}"))?;
            // Validate brofile/team capability composition against the
            // workflow's actor `requires` lists. Webhook ingress used
            // to skip this and let dispatch silently downgrade — fix
            // is to gate the spawn on the same check the MCP / HTTP
            // dispatch paths already use.
            if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
                return Err(anyhow::anyhow!(
                    "workflow '{workflow_id}' capability validation: {e}"
                ));
            }
            // Merge: extracted entity → initial_vars → caller's
            // explicit verdict initial_vars. Last writer wins, so
            // a routing rule's verdict can override entity fields if
            // it really needs to. Workflow vars_schema validates;
            // unknown keys are accepted (open schema by design).
            let mut merged_vars = serde_json::Map::new();
            if let Value::Object(m) = &entity {
                for (k, v) in m {
                    // Skip the synthetic `_headers` collection — it's
                    // there for routing predicates, not for the arc.
                    if k == "_headers" {
                        continue;
                    }
                    if !matches!(v, Value::Null) {
                        merged_vars.insert(k.clone(), v.clone());
                    }
                }
            }
            for (k, v) in initial_vars {
                merged_vars.insert(k, v);
            }
            // Slack thread continuity: if this arc came in through the
            // slack inlet and the entity carries `(team_id, channel,
            // thread_ts)`, look up any prior Claude session_id for
            // that thread and seed the badgey actor with it. The
            // `_actor_session.<actor>` magic key is stripped from
            // initial_vars in the engine before seed_vars runs (see
            // `extract_actor_session_seeds`).
            let slack_thread_key = (inlet_name == "slack")
                .then(|| {
                    let team = merged_vars.get("team_id").and_then(Value::as_str)?;
                    let channel = merged_vars.get("channel").and_then(Value::as_str)?;
                    let thread_ts = merged_vars.get("thread_ts").and_then(Value::as_str)?;
                    Some((team.to_string(), channel.to_string(), thread_ts.to_string()))
                })
                .flatten();
            if let Some((team, channel, thread_ts)) = slack_thread_key.as_ref() {
                if let Some(session_id) = state.slack_thread_store.get(team, channel, thread_ts) {
                    merged_vars.insert("_actor_session.badgey".into(), Value::String(session_id));
                }
            }
            // project_dir resolution priority:
            //   1. ${INLET_NAME_UPPERCASE}_PROJECT_DIR env override
            //      (works for webhooks AND pollers — both pass their
            //      `name` as inlet_name)
            //   2. inlet's `default_project_dir`
            //   3. None (worktree hooks will fail explicitly — better
            //      than silent fallback to cwd)
            let env_var = format!(
                "{}_PROJECT_DIR",
                inlet_name.to_uppercase().replace('-', "_")
            );
            let project_dir = std::env::var(&env_var).ok().or(default_project_dir);
            let workflow_id_clone = workflow_id.clone();
            // If the inlet that triggered this arc was a cron, the
            // cron registry has already incremented its in-flight
            // counter (in crons::run_one_tick → try_claim). Decrement
            // when the arc terminates so the next tick is admissible.
            // Inlets are labeled `cron:<name>` upstream; parse out the
            // name here.
            let cron_name = inlet_name.strip_prefix("cron:").map(|s| s.to_string());
            let crons_for_done = state.crons.clone();
            let server = BlackboxServer::new(state.clone());
            let slack_state = state.clone();
            tokio::spawn(async move {
                let result = workflow::run_workflow_with_initial_vars(
                    &server,
                    &compiled,
                    project_dir,
                    Some(50),
                    merged_vars,
                )
                .await;
                // Capture the badgey session_id back to the slack
                // thread store so the next @mention in this thread
                // resumes the same conversation.
                if let Some((team, channel, thread_ts)) = slack_thread_key.as_ref() {
                    if let Some(session_id) = result.actor_sessions.get("badgey") {
                        if !session_id.is_empty() && session_id != "pending" {
                            slack_state
                                .slack_thread_store
                                .set(team, channel, thread_ts, session_id);
                        }
                    }
                }
                if let Some(name) = cron_name {
                    crons_for_done.mark_done(&name);
                }
            });
            Ok(json!({
                "status": "arc_started",
                "workflow": workflow_id_clone,
            }))
        }
    }
}

/// Dispatch an installed workflow by registry id, with optional initial
/// vars. Mirrors the `start_arc` routing verdict in webhook handling
/// but exposes it for direct CLI / scripted invocation.
#[derive(Debug, Deserialize)]
pub(crate) struct OrchestrateByIdRequest {
    workflow_id: String,
    #[serde(default)]
    initial_vars: serde_json::Map<String, Value>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    await_completion: Option<bool>,
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

pub(crate) async fn orchestrate_by_id_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<OrchestrateByIdRequest>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec = match state
        .workflow_registry
        .read()
        .get(&req.workflow_id)
        .cloned()
    {
        Some(s) => s,
        None => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                format!("workflow id '{}' not in registry", req.workflow_id),
            )
                .into_response();
        }
    };
    let compiled = match workflow::compile(spec) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile failed: {e}"),
            )
                .into_response();
        }
    };
    let server = BlackboxServer::new(state.clone());
    if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("capability validation failed: {e}"),
        )
            .into_response();
    }
    let (task, arc_id) =
        server.spawn_workflow_task(compiled, req.project_dir, req.max_steps, req.initial_vars);
    if req.await_completion.unwrap_or(false) {
        let completed = orchestration::wait_for_task_with_timeout(&task, req.timeout_seconds).await;
        let mut out = if completed {
            orchestration::task_result_json(&task)
        } else {
            orchestration::timeout_snapshot_json(&task)
        };
        out["arcId"] = Value::String(arc_id);
        return axum::Json(out).into_response();
    }
    let inner = task.inner.lock();
    axum::Json(serde_json::json!({
        "taskId": inner.id,
        "sessionId": inner.session_id,
        "arcId": arc_id,
        "status": "running",
        "poll": {
            "status_tool": "bro_status",
            "wait_tool": "bro_wait",
            "arc_status_tool": "bro_arc_status"
        }
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct IrcStatusQuery {
    #[serde(default)]
    tail: Option<usize>,
}

pub(crate) async fn control_exec_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(mut req): axum::Json<ExecParams>,
) -> axum::Json<CallToolResult> {
    // The HTTP control handlers (`/control/exec`, `/irc/exec`) bypass
    // the MCP bro_exec tool surface but route through the same spawn
    // funnel. Force the origin to Cockpit so the roster tab groups
    // cockpit/IRC-launched tasks separately from peer-bros-launched
    // ones (which carry AgentDispatch). The MCP bro_exec path itself
    // defaults to AgentDispatch and ignores this override slot.
    req.origin_override = Some(bro_core::Origin::Cockpit);
    axum::Json(BlackboxServer::new(state).bro_exec(Parameters(req)).await)
}

pub(crate) async fn control_resume_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ResumeParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_resume(Parameters(req)).await)
}

// ── /control/closeout — phased closeout driver endpoint ────────────────────
//
// design/fleet-tui/closeout-command.md §4.1, Phase 3a (daemon-side). The
// endpoint applies the SAME pre-driver safety guards `exit_worktree` applies
// (managed-worktree, branch-prefix eligibility, detached-HEAD refusal,
// confirm gate) by reusing `bro_tools::fleet_worktree::prepare_closeout_request`
// (the shared entry extracted in Phase 1) — no silent duplication-with-drift.
// It then resolves `target` to the base repo's CURRENT BRANCH when the caller
// omits it (operator-decided default; the tool's default stays "main").
// Finally it calls `run_closeout_phases` and returns the STRUCTURED
// `CloseoutOutcome` directly (§4.3 — NOT a collapsed/rendered legacy tool
// JSON). Guard/validation failures return a 4xx with a clear error body.
pub(crate) async fn control_closeout_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<bro_protocol::CloseoutRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use bro_tools::fleet_worktree::{
        CloseoutOutcome as ToolOutcome, prepare_closeout_request, run_closeout_phases,
    };
    use serde_json::json;

    let _ = state; // SharedState is reserved for future hooks/state; not needed today.

    // Disposition whitelist (matches the existing tool's match arm).
    let disposition = req.disposition.trim().to_string();
    match disposition.as_str() {
        "keep" | "preflight" | "discard" | "publish" | "merge" | "adopt" => {}
        other => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": format!(
                        "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
                    ),
                })),
            )
                .into_response();
        }
    }
    // Mutating dispositions must carry confirm=true (same gate as the tool).
    if matches!(
        disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    ) && !req.confirm
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": format!("{disposition} requires confirm=true"),
            })),
        )
            .into_response();
    }
    // publish additionally needs a non-empty commit_message (preflight gate).
    if disposition == "publish"
        && req
            .commit_message
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "publish requires commit_message",
            })),
        )
            .into_response();
    }

    // Anchor base-repo resolution on the WORKTREE path, NOT the daemon CWD.
    //
    // In prod the daemon is launched from `WorkingDirectory=/Users/invidious`
    // (the launchd plist — NOT a git repo), and `BRO_FLEET_BASE_REPO` is not
    // in the plist env. If we passed the daemon CWD as `cx_root`,
    // `fleet_base_repo` would fall through to `primary_worktree`, which runs
    // `git -C <cx_root> worktree list` — that errors when `cx_root` is not
    // a git checkout, and every git-touching closeout (publish/merge/adopt/
    // discard/preflight) would fail in prod.
    //
    // The request's `worktree` is always a valid git worktree, so
    // `primary_worktree(<worktree>)` correctly returns the base/main
    // worktree regardless of where the daemon was launched. The tool path
    // behaves the same way: its `cx_root` is the worktree.
    let cx_root = PathBuf::from(&req.worktree);
    let worktree_arg: Option<&str> = if req.worktree.trim().is_empty() {
        None
    } else {
        Some(req.worktree.as_str())
    };

    // The cockpit creates managed worktrees under its fleet/agent store
    // (`bro_home/{fleet,agent}/worktrees`), NOT the legacy
    // `<repo_parent>/.bro-fleet-worktrees` convention the tool path assumes.
    // Pass those store roots so the managed-worktree guard accepts the
    // worktrees the cockpit actually produces (without them, `/closeout`
    // refuses every real fleet worktree — dogfooding finding).
    let extra_managed_roots = crate::managed_worktrees::cockpit_managed_worktree_roots();

    // Shared pre-driver guard: managed-worktree, branch-prefix, detached-HEAD,
    // target resolution. The endpoint's target default is the base repo's
    // CURRENT branch (operator-decided) — the tool's default stays "main".
    let mut driver_req = match prepare_closeout_request(
        &cx_root,
        worktree_arg,
        |base_repo| match req.target.as_deref() {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => bro_tools::fleet_worktree::current_branch(base_repo)
                .unwrap_or_else(|_| "main".to_string()),
        },
        req.allow_branch_prefixes.clone(),
        &extra_managed_roots,
    ) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };

    // Stamp the caller-supplied intent (guard doesn't see disposition/confirm/etc.)
    driver_req.disposition = disposition.clone();
    driver_req.confirm = req.confirm;
    driver_req.commit_message = req.commit_message.clone();
    driver_req.paths = req.paths.clone();
    driver_req.dry_run = req.dry_run;
    // Resolved project closeout hooks (the cockpit strict-loaded fleet.json and
    // sent them fully resolved). Translate the wire shape into the bro_tools
    // local type; the driver fires them at phase boundaries. Skipped on dry_run.
    driver_req.closeout_hooks = req.closeout_hooks.as_ref().map(to_driver_hooks);

    let outcome = run_closeout_phases(&driver_req);

    // Translate bro_tools::CloseoutOutcome into the bro_protocol wire shape.
    // The two type families are intentionally distinct (bro_protocol is
    // contract-bottom; bro_tools stays free of it). Each field maps 1:1;
    // the daemon does the conversion. PathBuf serializes to a string under
    // serde, and serde_json::Value passes through as embedded JSON.
    let wire_outcome: bro_protocol::CloseoutOutcome = match &outcome {
        ToolOutcome::Success { phases } => bro_protocol::CloseoutOutcome::Success {
            phases: phases.iter().map(to_wire_phase).collect(),
        },
        ToolOutcome::Failed(r) => bro_protocol::CloseoutOutcome::Failed(to_wire_phase(r)),
    };

    // The status code reflects whether the endpoint reached the driver, not
    // the success of the driver (which already failed above with 4xx). A
    // failed phase is still a valid HTTP outcome — the structured error
    // class is the signal the cockpit routes on, not the HTTP status.
    let status = StatusCode::OK;
    (status, axum::Json(json!(wire_outcome))).into_response()
}

/// Map `bro_tools::fleet_worktree::CloseoutPhase` →
/// `bro_protocol::CloseoutPhase` by name (both are `Copy` + `Serialize` +
/// `Deserialize` and use the same `snake_case` rename, so the numeric
/// discriminants line up — but the type families are independent and the
/// daemon bridges them, per the design).
fn to_wire_phase(r: &bro_tools::fleet_worktree::PhaseResult) -> bro_protocol::PhaseResult {
    use bro_protocol::CloseoutErrorClass as WireErr;
    use bro_protocol::CloseoutPhase as WirePhase;
    use bro_tools::fleet_worktree::{CloseoutErrorClass as ToolErr, CloseoutPhase as ToolPhase};
    let phase = match r.phase {
        ToolPhase::Preflight => WirePhase::Preflight,
        ToolPhase::StageCommit => WirePhase::StageCommit,
        ToolPhase::FfBase => WirePhase::FfBase,
        ToolPhase::Rebase => WirePhase::Rebase,
        ToolPhase::FfMerge => WirePhase::FfMerge,
        ToolPhase::Push => WirePhase::Push,
        ToolPhase::Remove => WirePhase::Remove,
        ToolPhase::Hook => WirePhase::Hook,
    };
    let error_class = match r.error_class {
        ToolErr::None => WireErr::None,
        ToolErr::BaseNotReady => WireErr::BaseNotReady,
        ToolErr::FfBaseFailed => WireErr::FfBaseFailed,
        ToolErr::StageFailed => WireErr::StageFailed,
        ToolErr::CommitFailed => WireErr::CommitFailed,
        ToolErr::RebaseConflict => WireErr::RebaseConflict,
        ToolErr::FfMergeFailed => WireErr::FfMergeFailed,
        ToolErr::PushRejected => WireErr::PushRejected,
        ToolErr::RemoveFailed => WireErr::RemoveFailed,
        ToolErr::HookBlocked => WireErr::HookBlocked,
        ToolErr::Other => WireErr::Other,
    };
    bro_protocol::PhaseResult {
        phase,
        repo_cwd: r.repo_cwd.clone(),
        ok: r.ok,
        error_class,
        content: r.content.clone(),
    }
}

/// Translate the resolved wire `CloseoutHooksWire` into the `bro_tools` local
/// `CloseoutHooks` the phased driver consumes. The daemon bridges the two type
/// families (bro_protocol is contract-bottom; bro_tools stays free of it).
fn to_driver_hooks(
    w: &bro_protocol::CloseoutHooksWire,
) -> bro_tools::fleet_worktree::CloseoutHooks {
    use bro_tools::fleet_worktree::{CloseoutHooks, HookOnFail};
    let on_fail = match w.on_fail.as_deref() {
        Some("block") => HookOnFail::Block,
        _ => HookOnFail::Warn,
    };
    CloseoutHooks {
        hooks: w.hooks.clone(),
        cwd: w.cwd.as_ref().map(std::path::PathBuf::from),
        on_fail,
        timeout_secs: w.timeout_secs.unwrap_or(600),
    }
}

pub(crate) async fn control_steer_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<SteerParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_steer(Parameters(req)))
}

pub(crate) async fn control_interrupt_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<InterruptParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_interrupt(Parameters(req)))
}

pub(crate) async fn control_broadcast_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<BroadcastParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(
        BlackboxServer::new(state)
            .bro_broadcast(Parameters(req))
            .await,
    )
}

pub(crate) async fn control_status_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Query(query): Query<IrcStatusQuery>,
) -> axum::Json<CallToolResult> {
    axum::Json(
        BlackboxServer::new(state).bro_status(Parameters(StatusParams {
            task_id,
            tail: query.tail,
        })),
    )
}

// ── /control/roster — daemon-authoritative fleet roster snapshot ─────────
//
// Slice 1a of design/fleet-tui/daemon-roster-and-tail-unification.md §3
// item 2. A pure-addition read-only endpoint that projects every task
// currently in `state.task_store` into a `RosterSummaryV1` and wraps
// them in a versioned `RosterSnapshotV1` envelope. No new task fields,
// no client-side change yet, no SSE — that is Slice 2.
//
// Two design notes for the derivation:
//
// 1. `model` is best-effort, scanned from the task's recorded event
//    buffer. The fleet client uses the same logic at
//    `crates/bro-fleet-client/src/fleet.rs:1355-1363`; we inline the
//    scan here because the touched-file list for this slice is
//    daemon-side only (the design explicitly defers client changes).
//
// 2. `last_event_at` is NOT a stored field — the daemon has no
//    per-event arrival stamp on V1 (the client stamps it from
//    `eventCount` growth today, fleet.rs:1301). We derive it from
//    `max(started_at, completed_at)`: for a live task this is the
//    spawn time, for a terminal task this is the completion time.
//    Coarse, but it stays within "no new task fields" and the
//    derivation is documented in the DTO doc-comment in
//    `crates/bro-protocol/src/lib.rs`.
pub(crate) async fn control_roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    use bro_protocol::RosterSnapshotV1;

    // Read the generation before the task snapshot. A concurrent delta may
    // make the task data newer than `version`, which can produce a duplicate
    // delta after the client connects, but it will not make the client miss one.
    let version = state.roster_events().current_version();
    let tasks = {
        let store = state.task_store.read();
        store
            .all_tasks()
            .into_iter()
            .map(|task| orchestration::roster_summary_from_task(&task))
            .collect()
    };

    let snapshot = RosterSnapshotV1 { version, tasks };
    axum::Json(snapshot).into_response()
}

pub(crate) async fn control_roster_forget_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::json;

    let dropped = {
        let mut store = state.task_store.write();
        let Some(task) = store.get(&task_id) else {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({
                    "error": format!("task {task_id} is not in the roster"),
                })),
            )
                .into_response();
        };
        let status = task.inner.lock().status;
        if !status.is_terminal() {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": format!(
                        "task {task_id} is {status:?}; only terminal tasks can be forgotten"
                    ),
                })),
            )
                .into_response();
        }
        store.retain_drop(|task| task.id() != task_id)
    };

    if dropped.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": format!("task {task_id} is not in the roster"),
            })),
        )
            .into_response();
    }

    orchestration::request_persist(&state.task_store, &state.store_dir);
    state.roster_events().emit_removed(task_id.clone());

    axum::Json(json!({
        "taskId": task_id,
        "forgotten": true,
    }))
    .into_response()
}

pub(crate) async fn control_dashboard_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<DashboardParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_dashboard(Parameters(query)))
}

pub(crate) async fn control_cancel_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<CancelParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_cancel(Parameters(req)))
}

pub(crate) async fn control_team_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(team_name): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    match orchestration::team::load_team(&team_name, &state.store_dir) {
        Some(team) => axum::Json(json!({
            "team": team.name,
            "members": team.members.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            format!("unknown team: {team_name}"),
        )
            .into_response(),
    }
}

// ── Admin HTTP endpoints (plain JSON; no MCP framing) ──────────────
//
// These wrap the same operations the MCP tools expose so install
// scripts can use plain `curl`. They're loopback-only via the listener
// binding.

pub(crate) async fn admin_runtime_metrics() -> impl axum::response::IntoResponse {
    axum::Json(json!({
        "status": "ok",
        "snapshot": super::runtime_metrics::latest_runtime_metrics_snapshot(),
    }))
    .into_response()
}

pub(crate) async fn admin_packet_compile(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<Value>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let p: packets::CompileParams = match serde_json::from_value(req) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("compile params parse: {e}"),
            )
                .into_response();
        }
    };
    let result = state.packets.read().compile(&p);
    match result {
        Ok(msg) => axum::Json(json!({"status": "ok", "message": msg})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("compile: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn read_artifact_source(source: &str) -> anyhow::Result<Value> {
    const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
    let raw = if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        let response = client.get(source).send().await?.error_for_status()?;
        let scheme = response.url().scheme();
        if scheme != "http" && scheme != "https" {
            anyhow::bail!("artifact source redirected to unsupported scheme `{scheme}`");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !(content_type.contains("application/json")
            || content_type.contains("text/json")
            || content_type.contains("text/plain"))
        {
            anyhow::bail!("artifact source content-type must be JSON or text/plain");
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_ARTIFACT_BYTES as u64)
        {
            anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > MAX_ARTIFACT_BYTES {
                anyhow::bail!("artifact source too large; limit is {MAX_ARTIFACT_BYTES} bytes");
            }
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes)?
    } else {
        std::fs::read_to_string(source)?
    };
    Ok(serde_json::from_str(&raw)?)
}

pub(crate) async fn install_artifact_from_params(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    let value = read_artifact_source(&p.source).await?;
    install_artifact_value(state, p, value).await
}

pub(crate) async fn install_artifact_value(
    state: &Arc<SharedState>,
    p: ArtifactInstallParams,
    mut value: Value,
) -> anyhow::Result<artifacts::ArtifactMetadata> {
    let mut installed_agent: Option<(
        orchestration::agents::types::AgentRef,
        orchestration::agents::types::AgentManifest,
        Vec<String>,
    )> = None;
    match p.kind {
        artifacts::ArtifactKind::Workflow => {
            let spec: workflow::Workflow = serde_json::from_value(value.clone())?;
            let compiled = workflow::compile(spec.clone())?;
            if let Err(e) = validate_workflow_capabilities(&compiled, state) {
                anyhow::bail!("workflow capability validation failed: {e}");
            }
            let id = p.name.clone().unwrap_or_else(|| spec.name.clone());
            let dir = state.store_dir.join("workflows");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_string_pretty(&spec).unwrap_or_default(),
            )?;
            state.workflow_registry.write().insert(id, spec);
        }
        artifacts::ArtifactKind::Packet => {
            let params: packets::CompileParams = serde_json::from_value(value.clone())?;
            state.packets.read().compile(&params)?;
        }
        artifacts::ArtifactKind::Brofile => {
            let brofile: orchestration::brofile::Brofile = serde_json::from_value(value.clone())?;
            let written =
                orchestration::brofile::save_brofile(&brofile, "global", &state.store_dir, None)
                    .map_err(|e| anyhow::anyhow!("brofile registry write failed: {e}"))?;
            // Post-install verification — the artifact catalog reports
            // "active" only when the runtime registry can actually see
            // the brofile. Prevents silent G11-style desync where the
            // catalog says installed but bro_brofile list returns
            // empty.
            if orchestration::brofile::resolve_brofile(&brofile.name, &state.store_dir, None)
                .is_none()
            {
                anyhow::bail!(
                    "brofile written to {} but resolve_brofile returned None — runtime registry desync",
                    written.display()
                );
            }
        }
        artifacts::ArtifactKind::Team => {
            // Teams are stored as artifacts but have no additional validation at install time.
        }
        artifacts::ArtifactKind::Cron => {
            let spec: crons::CronSpec = serde_json::from_value(value.clone())?;
            crons::validate_schedule(&spec.schedule)?;
            let dir = state.store_dir.join("crons");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join(format!("{}.json", spec.name)),
                serde_json::to_string_pretty(&spec).unwrap_or_default(),
            )?;
            state.crons.install(spec.clone());
            let handle = crons::spawn_loop(state.clone(), spec.clone());
            state.crons.track_handle(&spec.name, handle);
        }
        artifacts::ArtifactKind::Agent => {
            if !value.is_object() {
                anyhow::bail!("agent artifact must be a JSON object");
            }
            let adapter_registry = state.agent_adapter_registry.read();
            let catalog = state.artifacts.read();
            let ctx = orchestration::agents::validate::InstallCtx {
                adapter_registry: &adapter_registry,
                brofile_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(artifacts::ArtifactKind::Brofile, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
                agent_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(artifacts::ArtifactKind::Agent, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
            };
            orchestration::agents::validate::validate_agent_install(&value, &ctx)?;
            drop(catalog);
            let manifest_value = value
                .get("manifest")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("agent artifact missing manifest"))?;
            let mut manifest: orchestration::agents::types::AgentManifest =
                serde_json::from_value(manifest_value)?;
            let name = p
                .name
                .clone()
                .or_else(|| {
                    value
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .ok_or_else(|| anyhow::anyhow!("agent artifact missing name"))?;
            let version = p
                .version
                .clone()
                .or_else(|| value.get("version").and_then(artifact_version_string))
                .ok_or_else(|| anyhow::anyhow!("agent artifact missing version"))?
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("agent artifact version must parse as u32"))?;
            let agent_ref = orchestration::agents::types::AgentRef { name, version };
            manifest.embedding = Some(embed_queue::agent_manifest_embedding(&agent_ref, &manifest));
            value["manifest"]["embedding"] = serde_json::to_value(&manifest.embedding)?;
            let install_warnings = agent_install_warnings(state, &manifest);
            installed_agent = Some((agent_ref, manifest, install_warnings));
        }
        artifacts::ArtifactKind::Atom => {
            if !value.is_object() {
                anyhow::bail!("atom artifact must be a JSON object");
            }
            let catalog = state.artifacts.read();
            let ctx = orchestration::atoms::validate::InstallCtx {
                brofile_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(artifacts::ArtifactKind::Brofile, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
                atom_exists: |name: &str| -> bool {
                    catalog
                        .metadata_for(artifacts::ArtifactKind::Atom, name)
                        .ok()
                        .flatten()
                        .is_some_and(|m| m.active)
                },
            };
            orchestration::atoms::validate::validate_atom_install(&value, &ctx)?;
        }
    }
    let mut meta = state
        .artifacts
        .write()
        .install_value(p.kind, p.source, &value, p.name, p.version, p.supersedes)
        .and_then(|meta| {
            if let Some(prev) = meta.supersedes.as_deref() {
                if prev != meta.name {
                    deactivate_artifact(state, meta.kind, prev)?;
                }
            }
            Ok(meta)
        })?;
    if let Some((agent_ref, manifest, install_warnings)) = installed_agent {
        if !install_warnings.is_empty() {
            meta = state.artifacts.write().update_install_warnings(
                artifacts::ArtifactKind::Agent,
                &agent_ref.name,
                install_warnings,
            )?;
        }
        embed_queue::enqueue_agent_manifest(&agent_ref, &manifest);
        persist_agent_provenance_edges(state, &agent_ref, &manifest)?;
    }
    Ok(meta)
}

pub(crate) fn restore_runtime_artifacts_from_catalog(
    state: &Arc<SharedState>,
) -> anyhow::Result<usize> {
    let entries = state.artifacts.read().list(&ArtifactListParams {
        kind: None,
        name: None,
        include_superseded: false,
    })?;
    let mut restored = 0usize;

    for entry in entries
        .into_iter()
        .filter(|entry| entry.active)
        .filter(|entry| {
            matches!(
                entry.kind,
                artifacts::ArtifactKind::Workflow
                    | artifacts::ArtifactKind::Packet
                    | artifacts::ArtifactKind::Brofile
            )
        })
    {
        let Some(value) = state
            .artifacts
            .read()
            .load_artifact_value(entry.kind, &entry.name)?
        else {
            tracing::warn!(
                "active {} artifact '{}' has no catalog payload; runtime registry not restored",
                entry.kind.as_str(),
                entry.name
            );
            continue;
        };

        match entry.kind {
            artifacts::ArtifactKind::Workflow => {
                let spec: workflow::Workflow = serde_json::from_value(value.clone())
                    .with_context(|| format!("parsing workflow artifact '{}'", entry.name))?;
                let compiled = workflow::compile(spec.clone())
                    .with_context(|| format!("compiling workflow artifact '{}'", entry.name))?;
                validate_workflow_capabilities(&compiled, state)
                    .map_err(|e| anyhow::anyhow!("{e}"))
                    .with_context(|| format!("validating workflow artifact '{}'", entry.name))?;

                let dir = state.store_dir.join("workflows");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join(format!("{}.json", entry.name)),
                    serde_json::to_string_pretty(&spec).unwrap_or_default(),
                )?;
                state
                    .workflow_registry
                    .write()
                    .insert(entry.name.clone(), spec);
                restored += 1;
            }
            artifacts::ArtifactKind::Packet => {
                let params: packets::CompileParams = serde_json::from_value(value.clone())
                    .with_context(|| format!("parsing packet artifact '{}'", entry.name))?;
                state
                    .packets
                    .read()
                    .compile(&params)
                    .with_context(|| format!("compiling packet artifact '{}'", entry.name))?;
                restored += 1;
            }
            artifacts::ArtifactKind::Brofile => {
                let brofile: orchestration::brofile::Brofile =
                    serde_json::from_value(value.clone())
                        .with_context(|| format!("parsing brofile artifact '{}'", entry.name))?;
                let written = orchestration::brofile::save_brofile(
                    &brofile,
                    "global",
                    &state.store_dir,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("brofile registry write failed: {e}"))?;
                if orchestration::brofile::resolve_brofile(&brofile.name, &state.store_dir, None)
                    .is_none()
                {
                    anyhow::bail!(
                        "brofile written to {} but resolve_brofile returned None — runtime registry desync",
                        written.display()
                    );
                }
                restored += 1;
            }
            _ => {}
        }
    }

    Ok(restored)
}

pub(crate) fn agent_install_warnings(
    state: &Arc<SharedState>,
    manifest: &orchestration::agents::types::AgentManifest,
) -> Vec<String> {
    let Some(overlay) = manifest.filter_overlay.as_ref() else {
        return Vec::new();
    };
    let (base_allow, base_disallow) = if let Some(brofile_ref) = manifest.brofile_ref.as_ref() {
        let Some(brofile) =
            orchestration::brofile::resolve_brofile(brofile_ref, &state.store_dir, None)
        else {
            return Vec::new();
        };
        match brofile.filters {
            Some(filters) => (filters.allow, filters.disallow),
            None => (Vec::new(), Vec::new()),
        }
    } else if let Some(inline) = manifest.brofile_inline.as_ref() {
        BlackboxServer::extract_inline_filters(inline)
    } else {
        (Vec::new(), Vec::new())
    };

    let mut warnings = Vec::new();
    for allowed in &overlay.allow {
        if base_disallow.contains(allowed) {
            warnings.push(format!(
                "filter_overlay.allow `{allowed}` is also disallowed by the base brofile; deny-wins merge keeps it disallowed"
            ));
        }
    }
    for disallowed in &overlay.disallow {
        if base_allow.contains(disallowed) {
            warnings.push(format!(
                "filter_overlay.disallow `{disallowed}` overrides a base brofile allow entry"
            ));
        }
    }
    warnings
}

pub(crate) fn artifact_version_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(crate) fn persist_agent_provenance_edges(
    state: &Arc<SharedState>,
    agent_ref: &orchestration::agents::types::AgentRef,
    manifest: &orchestration::agents::types::AgentManifest,
) -> anyhow::Result<()> {
    use orchestration::agents::types::AgentProvenance;
    let Some(AgentProvenance::Distilled {
        evidence_session_ids,
        created_from_threads,
        ..
    }) = manifest.provenance.as_ref()
    else {
        return Ok(());
    };
    let source = entity_ref::EntityRef::Agent {
        name: agent_ref.name.clone(),
        version: agent_ref.version,
    };
    let mut edges = Vec::new();
    for session in evidence_session_ids {
        let target = entity_ref::EntityRef::parse(session)?;
        if !matches!(target, entity_ref::EntityRef::Session { .. }) {
            anyhow::bail!("distilled agent evidence ref is not a session: {session}");
        }
        edges.push(agent_derived_from_edge(source.clone(), target));
    }
    for thread in created_from_threads {
        let target = entity_ref::EntityRef::parse(thread)?;
        if !matches!(target, entity_ref::EntityRef::Thread { .. }) {
            anyhow::bail!("distilled agent thread ref is not a thread: {thread}");
        }
        edges.push(agent_derived_from_edge(source.clone(), target));
    }
    let edges_dir = edge_index::edges_dir_from_bro_store(&state.store_dir);
    let written = edge_index::append_explicit_edges(&edges_dir, "agents", &edges)?;
    if written > 0 {
        rebuild_edge_index_from_shared(state, false);
    }
    Ok(())
}

pub(crate) fn agent_derived_from_edge(
    source: entity_ref::EntityRef,
    target: entity_ref::EntityRef,
) -> edge_index::Edge {
    edge_index::Edge {
        source,
        kind: "DERIVED_FROM".into(),
        target,
        provenance: chunker::EdgeProvenance::Explicit,
        confidence: chunker::EdgeConfidence::Exact,
        metadata: Default::default(),
    }
}

pub(crate) fn deactivate_artifact(
    state: &Arc<SharedState>,
    kind: artifacts::ArtifactKind,
    name: &str,
) -> anyhow::Result<()> {
    match kind {
        artifacts::ArtifactKind::Workflow => {
            state.workflow_registry.write().remove(name);
            let path = state
                .store_dir
                .join("workflows")
                .join(format!("{name}.json"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        artifacts::ArtifactKind::Packet => {
            state.packets.read().remove_domain(name)?;
        }
        artifacts::ArtifactKind::Brofile => {
            orchestration::brofile::delete_brofile(name, "global", &state.store_dir, None);
        }
        artifacts::ArtifactKind::Agent => {
            // No separate registry to deactivate for agents (yet).
        }
        artifacts::ArtifactKind::Atom => {
            // No separate registry to deactivate for atoms (yet).
        }
        artifacts::ArtifactKind::Team => {
            // Teams are stored purely as artifacts; no separate registry to deactivate.
        }
        artifacts::ArtifactKind::Cron => {
            state.crons.remove(name);
            let path = state.store_dir.join("crons").join(format!("{name}.json"));
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    Ok(())
}

pub(crate) fn rebuild_edge_index_from_shared(
    state: &SharedState,
    include_tantivy_projection: bool,
) {
    let edges_dir = edge_index::edges_dir_from_bro_store(&state.store_dir);
    // Compute the rebuilt index while holding the store read-locks, then drop
    // ALL of them before acquiring `edge_index.write()`. Holding idx.read()/
    // kb.read()/etc. across the edge_index.write() acquisition is a deadlock
    // hazard:
    //   A (this rebuild)        holds idx.read, wants edge_index.write
    //   R (auto-reindex commit) wants idx.write -> queues behind A; a queued
    //                           writer then blocks new idx *readers* (parking_lot
    //                           is fair, so readers don't starve the writer)
    //   D (a graph tool, e.g.   holds edge_index.read (live arg), wants idx.read
    //      bbox_blame)          -> blocked behind R
    // => A waits on D's edge_index.read, D waits on R's queued idx.write, R waits
    //    on A's idx.read. Cycle. Acquiring edge_index.write() with no store locks
    //    held removes A from the cycle entirely.
    let rebuilt = {
        let idx = state.idx.read();
        let kb = state.kb.read();
        let threads = state.threads.read();
        let notes = state.notes.read();
        let task_store = state.task_store.read();
        let roadmap = state.roadmap.read();
        let registered_project_ids = state
            .projects
            .read()
            .list()
            .into_iter()
            .map(|project| project.project_id)
            .collect();
        edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
            index: &idx,
            knowledge: &kb,
            threads: &threads,
            notes: &notes,
            task_store: &task_store,
            roadmap: &roadmap,
            edges_dir,
            registered_project_ids: Some(registered_project_ids),
            include_tantivy_projection,
            include_observed: true,
        })
        // all store read-guards drop here
    };
    *state.edge_index.write() = rebuilt;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeSidecarSignature {
    files: u64,
    bytes: u64,
    modified_nanos: u128,
}

fn edge_sidecar_signature(edges_dir: &std::path::Path) -> EdgeSidecarSignature {
    let mut sig = EdgeSidecarSignature {
        files: 0,
        bytes: 0,
        modified_nanos: 0,
    };
    let mut stack = vec![edges_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                // Skip in-progress materialization temp dirs (`*.write-tmp`):
                // the overlay/snapshot writer builds the new files there before
                // an atomic rename, so counting them makes the watcher observe
                // mid-write churn and rebuild against a half-written overlay.
                // (Original fix by @benstpierre in PR #3, incorporated here.)
                if path.extension().is_some_and(|ext| ext == "write-tmp") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            sig.files += 1;
            sig.bytes = sig.bytes.saturating_add(meta.len());
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            sig.modified_nanos = sig.modified_nanos.wrapping_add(modified);
        }
    }
    // Fold in the manifest-index, which records *which* snapshot/overlay is
    // active per workspace. A branch switch flips the active pointer between two
    // already-materialized snapshots without changing any `.jsonl` mtime, so the
    // recursive scan above is blind to it and the in-memory graph would go
    // stale. The materialization writer rewrites this file exactly when the
    // active workspace graph changes (writer-side idempotency guard), making its
    // mtime/len a precise active-pointer change signal. It is `.json`, so the
    // scan above skipped it — no double counting.
    if let Ok(meta) = std::fs::metadata(crate::manifest::manifest_index_path(edges_dir)) {
        sig.bytes = sig.bytes.saturating_add(meta.len());
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        sig.modified_nanos = sig.modified_nanos.wrapping_add(modified);
    }
    sig
}

/// Watcher thread that rebuilds the EdgeIndex when edge sidecars change.
/// The auto-reindex thread writes new docs + edge sidecars every interval,
/// but it can't trigger a rebuild itself (it spawns before SharedState exists).
/// The watcher uses sidecar-only rebuilds so background maintenance does not
/// materialize every stored Tantivy document.
pub(crate) fn spawn_edge_index_rebuild_watcher(
    state: Arc<SharedState>,
    interval: std::time::Duration,
) {
    std::thread::Builder::new()
        .name("blackbox-edge-rebuild".into())
        .spawn(move || {
            // Initial settle so the boot-time rebuild already ran.
            std::thread::sleep(std::time::Duration::from_secs(20));
            let mut last_seen: u64 = state.idx.read().num_docs();
            let edges_dir = edge_index::edges_dir_from_bro_store(&state.store_dir);
            let mut last_signature = edge_sidecar_signature(&edges_dir);
            loop {
                std::thread::sleep(interval);
                let current = state.idx.read().num_docs();
                let signature = edge_sidecar_signature(&edges_dir);
                if signature != last_signature {
                    let started = std::time::Instant::now();
                    rebuild_edge_index_from_shared(&state, false);
                    tracing::info!(
                        prev_docs = last_seen,
                        new_docs = current,
                        sidecar_files = signature.files,
                        sidecar_bytes = signature.bytes,
                        elapsed_ms = started.elapsed().as_millis(),
                        "edge-index watcher: sidecars changed, EdgeIndex rebuilt"
                    );
                    last_signature = signature;
                } else if current > last_seen {
                    tracing::debug!(
                        prev_docs = last_seen,
                        new_docs = current,
                        sidecar_files = signature.files,
                        sidecar_bytes = signature.bytes,
                        "edge-index watcher: corpus grew without sidecar changes; rebuild skipped"
                    );
                }
                if current > last_seen {
                    last_seen = current;
                }
            }
        })
        .expect("failed to spawn edge index rebuild watcher");
}

pub(crate) fn trigger_project_bootstrap_arc(state: Arc<SharedState>, record: ProjectRecord) {
    let Some(spec) = state
        .workflow_registry
        .read()
        .get("project-bootstrap-arc")
        .cloned()
    else {
        tracing::debug!(
            project_id = %record.project_id,
            "project-bootstrap-arc is not installed; registration recorded without arc trigger"
        );
        return;
    };
    let compiled = match workflow::compile(spec) {
        Ok(compiled) => compiled,
        Err(err) => {
            tracing::warn!(error = %err, "project-bootstrap-arc compile failed");
            return;
        }
    };
    if let Err(err) = validate_workflow_capabilities(&compiled, &state) {
        tracing::warn!(error = %err, "project-bootstrap-arc capability validation failed");
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime available; skipped project-bootstrap-arc trigger");
        return;
    };
    let project_dir = Some(record.canonical_path.clone());
    let mut vars = serde_json::Map::new();
    vars.insert("project_id".to_string(), Value::String(record.project_id));
    vars.insert(
        "project_path".to_string(),
        Value::String(record.canonical_path),
    );
    if let Some(repo_id) = record.repo_id {
        vars.insert("repo_id".to_string(), Value::String(repo_id));
    }
    handle.spawn(async move {
        let server = BlackboxServer::new(state);
        let _ = workflow::run_workflow_with_initial_vars(
            &server,
            &compiled,
            project_dir,
            Some(50),
            vars,
        )
        .await;
    });
}

pub(crate) fn project_ref_counts(state: &Arc<SharedState>, project: &str) -> anyhow::Result<Value> {
    let knowledge = state
        .kb
        .read()
        .all_entries()
        .iter()
        .filter(|entry| entry.project.as_deref() == Some(project))
        .count();
    let threads = state
        .threads
        .read()
        .all()
        .iter()
        .filter(|thread| thread.project == project)
        .count();
    let notes = state
        .notes
        .read()
        .all()
        .iter()
        .filter(|note| note.project.as_deref() == Some(project))
        .count();
    let pins = state.pins.read().project_ref_count(project);
    let packets = state
        .packets
        .read()
        .list_all()?
        .iter()
        .filter(|packet| packet.project.as_deref() == Some(project))
        .count();
    let slack_channel_bindings = state.slack_channel_bindings.list(None, Some(project)).len();
    let slack_proposal_links = state.slack_proposal_links.project_ref_count(project);
    let teams = orchestration::team::load_all_teams(&state.store_dir)
        .iter()
        .filter(|team| team.project_dir.as_deref() == Some(project))
        .count();
    let councils = state.councils.list_summaries(Some(project)).len();
    let whiteboards = state
        .whiteboards
        .list_ids()
        .iter()
        .filter(|id| {
            state
                .whiteboards
                .get(id)
                .is_some_and(|board| board.read().project == project)
        })
        .count();
    let pollers = state
        .pollers
        .list()
        .iter()
        .filter(|spec| spec.default_project_dir.as_deref() == Some(project))
        .count();
    let crons = state
        .crons
        .list()
        .iter()
        .filter(|spec| spec.default_project_dir.as_deref() == Some(project))
        .count();

    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "notes": notes,
        "pins": pins,
        "packets": packets,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "teams": teams,
        "councils": councils,
        "whiteboards": whiteboards,
        "pollers": pollers,
        "crons": crons,
    }))
}

/// Re-derive the knowledge store's project roots from the live registry so its
/// committed `.bbox/knowledge/` entries are loaded into the query surface.
/// Called whenever the set of registered projects changes (register, rename,
/// unregister) and at daemon startup.
pub(crate) fn sync_kb_project_roots(state: &SharedState) {
    let roots: Vec<std::path::PathBuf> = state
        .projects
        .read()
        .list()
        .into_iter()
        .map(|r| std::path::PathBuf::from(r.canonical_path))
        .collect();
    if let Err(e) = state.kb.write().set_project_roots(roots) {
        tracing::warn!("kb project-root sync: {e:#}");
    }
}

/// Enqueue embeddings for a project's loaded knowledge entries. The BM25
/// reindex picks up committed `.bbox/knowledge/` automatically, but vector
/// coverage is driven by enqueue, so a project registered from a clone would
/// otherwise be invisible to vector search until a manual reembed. The embed
/// worker dedupes by (entity_id, chunk_hash), so re-enqueuing already-embedded
/// entries is a cheap no-op. Returns the number of entries enqueued.
pub(crate) fn enqueue_project_knowledge_embeds(state: &SharedState, project_dir: &str) -> usize {
    let kb = state.kb.read();
    let mut enqueued = 0usize;
    for entry in kb.all_entries().iter().filter(|e| {
        e.project.as_deref() == Some(project_dir)
            && matches!(
                e.status,
                crate::knowledge::Status::Active | crate::knowledge::Status::Superseded
            )
    }) {
        let entity_id = crate::index::knowledge_entity_id(&entry.id);
        let chunk_hash = crate::index::knowledge_chunk_hash(entry);
        crate::embed_queue::enqueue_knowledge(entry, &entity_id, &chunk_hash);
        enqueued += 1;
    }
    enqueued
}

pub(crate) fn migrate_project_refs(
    state: &Arc<SharedState>,
    old_project: &str,
    new_project: &str,
    record: &ProjectRecord,
) -> anyhow::Result<Value> {
    let knowledge = state
        .kb
        .write()
        .rename_project_refs(old_project, new_project)?;
    if knowledge > 0 {
        // This sync migration helper cannot await; knowledge persistence is write-behind here.
        state.kb_persister.request();
    }
    let threads = state
        .threads
        .write()
        .rename_project_refs(old_project, new_project)?;
    if threads > 0 {
        // This sync migration helper cannot await; threads persistence is write-behind here.
        state.threads_persister.request();
    }
    let notes = state
        .notes
        .write()
        .rename_project_refs(old_project, new_project)?;
    if notes > 0 {
        // This sync migration helper cannot await; notes persistence is write-behind here.
        state.notes_persister.request();
    }
    let pins = state
        .pins
        .write()
        .rename_project_refs(old_project, new_project)?;
    if pins > 0 {
        // This sync migration helper cannot await; pins persistence is write-behind here.
        state.pins_persister.request();
    }
    let packets = state
        .packets
        .read()
        .rename_project_refs(old_project, new_project)?;
    let slack_channel_bindings = state.slack_channel_bindings.rename_project_refs(
        old_project,
        new_project,
        Some(record.project_id.as_str()),
    )?;
    let slack_proposal_links = state
        .slack_proposal_links
        .rename_project_refs(old_project, new_project)?;
    let teams =
        orchestration::team::rename_project_refs(&state.store_dir, old_project, new_project);
    let councils = state
        .councils
        .rename_project_refs(old_project, new_project)?;
    let whiteboards = state
        .whiteboards
        .rename_project_refs(old_project, new_project)?;

    let pollers = state.pollers.rename_project_refs(old_project, new_project);
    let poller_count = pollers.len();
    for spec in pollers {
        persist_named_json(&state.store_dir.join("pollers"), &spec.name, &spec)?;
        let handle = pollers::spawn_loop(state.clone(), spec.clone());
        state.pollers.track_handle(&spec.name, handle);
    }

    let crons = state.crons.rename_project_refs(old_project, new_project);
    let cron_count = crons.len();
    for spec in crons {
        persist_named_json(&state.store_dir.join("crons"), &spec.name, &spec)?;
        let handle = crons::spawn_loop(state.clone(), spec.clone());
        state.crons.track_handle(&spec.name, handle);
    }

    Ok(json!({
        "knowledge": knowledge,
        "threads": threads,
        "notes": notes,
        "pins": pins,
        "packets": packets,
        "slack_channel_bindings": slack_channel_bindings,
        "slack_proposal_links": slack_proposal_links,
        "teams": teams,
        "councils": councils,
        "whiteboards": whiteboards,
        "pollers": poller_count,
        "crons": cron_count,
    }))
}

pub(crate) fn persist_named_json<T: Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminWorkflowInstallReq {
    #[serde(default)]
    id: Option<String>,
    spec: Value,
}

pub(crate) async fn admin_workflow_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminWorkflowInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: workflow::Workflow = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("workflow parse: {e}"),
            )
                .into_response();
        }
    };
    let compiled = match workflow::compile(spec.clone()) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("workflow compile: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = validate_workflow_capabilities(&compiled, &state) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("capability validation: {e}"),
        )
            .into_response();
    }
    let id = req.id.unwrap_or_else(|| spec.name.clone());
    let dir = state.store_dir.join("workflows");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{id}.json"));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("workflow persist: {e}"),
        )
            .into_response();
    }
    state.workflow_registry.write().insert(id.clone(), spec);
    axum::Json(json!({"status": "installed", "id": id})).into_response()
}

pub(crate) async fn admin_artifact_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactInstallParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match install_artifact_from_params(&state, req).await {
        Ok(meta) => axum::Json(json!({"status": "installed", "artifact": meta})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact install: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_list(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<ArtifactListParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state.artifacts.read().list(&query) {
        Ok(rows) => axum::Json(json!({"artifacts": rows})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("artifact list: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_supersede(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactSupersedeParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    match state
        .artifacts
        .write()
        .supersede(req.kind, &req.name, &req.superseded_by)
    {
        Ok(meta) => match deactivate_artifact(&state, req.kind, &req.name) {
            Ok(()) => axum::Json(json!({"status": "superseded", "artifact": meta})).into_response(),
            Err(e) => (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact deactivate: {e:#}"),
            )
                .into_response(),
        },
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact supersede: {e:#}"),
        )
            .into_response(),
    }
}

pub(crate) async fn admin_artifact_remove(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<ArtifactRemoveParams>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    if !req.dry_run && !req.confirm {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "artifact remove: hard artifact removal requires confirm=true".to_string(),
        )
            .into_response();
    }
    if !req.dry_run {
        if let Err(e) = state
            .artifacts
            .read()
            .remove_hard(req.kind, &req.name, true, true)
        {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact remove: {e:#}"),
            )
                .into_response();
        }
        if let Err(e) = deactivate_artifact(&state, req.kind, &req.name) {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("artifact deactivate: {e:#}"),
            )
                .into_response();
        }
    }
    match state
        .artifacts
        .write()
        .remove_hard(req.kind, &req.name, req.dry_run, req.confirm)
    {
        Ok(result) => axum::Json(json!({"status": "removed", "artifact": result})).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            format!("artifact remove: {e:#}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminWebhookInstallReq {
    spec: Value,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminPollerInstallReq {
    spec: Value,
}

pub(crate) async fn admin_poller_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminPollerInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: pollers::PollerSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("poller parse: {e}"),
            )
                .into_response();
        }
    };
    let dir = state.store_dir.join("pollers");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("poller persist: {e}"),
        )
            .into_response();
    }
    state.pollers.install(spec.clone());
    let handle = pollers::spawn_loop(state.clone(), spec.clone());
    state.pollers.track_handle(&spec.name, handle);
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "every_seconds": spec.every_seconds,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminCronInstallReq {
    spec: Value,
}

pub(crate) async fn admin_cron_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminCronInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: crons::CronSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("cron parse: {e}"),
            )
                .into_response();
        }
    };
    if let Err(e) = crons::validate_schedule(&spec.schedule) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("cron schedule invalid: {e}"),
        )
            .into_response();
    }
    let dir = state.store_dir.join("crons");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("cron persist: {e}"),
        )
            .into_response();
    }
    state.crons.install(spec.clone());
    let handle = crons::spawn_loop(state.clone(), spec.clone());
    state.crons.track_handle(&spec.name, handle);
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "schedule": spec.schedule,
        "concurrency": spec.concurrency,
    }))
    .into_response()
}

pub(crate) async fn admin_webhook_install(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminWebhookInstallReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let spec: webhooks::WebhookSpec = match serde_json::from_value(req.spec) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("webhook parse: {e}"),
            )
                .into_response();
        }
    };
    // Reject schemes incompatible with current bind (parallel to
    // bro_webhook_install + restore-on-startup).
    if let Err(e) = webhooks::install_check(&spec.signature, state.bind_is_loopback) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            format!("webhook install rejected: {e}"),
        )
            .into_response();
    }
    let dir = state.store_dir.join("webhooks");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", spec.name));
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    ) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("webhook persist: {e}"),
        )
            .into_response();
    }
    state.webhooks.install(spec.clone());
    axum::Json(json!({
        "status": "installed",
        "name": spec.name,
        "endpoint": format!("/webhook/{}", spec.name),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminBrofileUpsertReq {
    name: String,
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
}

pub(crate) async fn admin_brofile_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminBrofileUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let provider: orchestration::providers::Provider = match req.provider.parse() {
        Ok(p) => p,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!("unknown provider '{}'", req.provider),
            )
                .into_response();
        }
    };
    let bf = orchestration::brofile::Brofile {
        name: req.name.clone(),
        provider,
        account: req.account,
        lens: req.lens,
        model: req.model,
        effort: req.effort,
        filters: None,
        surface: None,
        coerce_workspace: None,
        runtime: None,
        context: None,
        code_mode: None,
        service_tier: req.service_tier,
    };
    if let Err(e) = orchestration::brofile::save_brofile(&bf, "global", &state.store_dir, None) {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(json!({"status": "error", "name": req.name, "error": e.to_string()})),
        )
            .into_response();
    }
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdminTeamUpsertReq {
    name: String,
    members: Vec<String>,
}

pub(crate) async fn admin_team_upsert(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<AdminTeamUpsertReq>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    let teamplate = orchestration::team::Teamplate {
        name: req.name.clone(),
        members: req
            .members
            .iter()
            .enumerate()
            .map(|(i, brofile)| orchestration::team::TeamplateMember {
                brofile: brofile.clone(),
                alias: Some(format!("m{}", i + 1)),
                count: 1,
            })
            .collect(),
        advisor: None,
        diversity_floor: None,
    };
    orchestration::team::save_teamplate(&teamplate, "global", &state.store_dir, None);
    let team = orchestration::team::Team {
        name: req.name.clone(),
        teamplate: req.name.clone(),
        members: req
            .members
            .iter()
            .enumerate()
            .map(|(i, brofile)| orchestration::team::TeamMember {
                name: format!("m{}", i + 1),
                brofile: brofile.clone(),
                session_id: None,
                task_history: Vec::new(),
            })
            .collect(),
        advisor: None,
        project_dir: None,
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        diversity_floor: None,
    };
    let _lock = orchestration::team::lock_teams();
    orchestration::team::save_team(&team, &state.store_dir);
    axum::Json(json!({"status": "upserted", "name": req.name})).into_response()
}

pub(crate) async fn signal_arc_dispatch(
    state: &Arc<SharedState>,
    signal: &str,
    correlation: serde_json::Map<String, Value>,
    payload: Value,
) -> Value {
    let store = &state.wait_store;
    let pending_before: Vec<_> = store
        .snapshot()
        .into_iter()
        .filter(|w| w.signal == signal)
        .collect();
    let m = store.match_and_take(signal, &correlation);
    let Some((resolved_slot, notify, arc_id, wait_id)) = m else {
        tracing::info!(
            "signal '{signal}' arrived with correlation {correlation:?} — no matching wait (idle). pending_with_same_signal={:?}",
            pending_before
                .iter()
                .map(|w| (w.arc_id.clone(), w.wait_id.clone(), w.correlation.clone()))
                .collect::<Vec<_>>(),
        );
        state.record_signal(SignalEvent {
            timestamp: util::now_iso(),
            signal: signal.to_string(),
            correlation: correlation.clone(),
            outcome: "no_matching_wait".into(),
            matched_arc_id: None,
            matched_wait_id: None,
            idle_pending: pending_before.clone(),
        });
        return json!({
            "status": "no_matching_wait",
            "signal": signal,
            "correlation": correlation,
            "pending_with_same_signal": pending_before,
        });
    };
    tracing::info!(
        "signal '{signal}' arrived with correlation {correlation:?} — resolved wait arc={arc_id} wait_id={wait_id}",
    );
    state.record_signal(SignalEvent {
        timestamp: util::now_iso(),
        signal: signal.to_string(),
        correlation: correlation.clone(),
        outcome: "matched".into(),
        matched_arc_id: Some(arc_id.clone()),
        matched_wait_id: Some(wait_id.clone()),
        idle_pending: Vec::new(),
    });
    let sig = crate::workflow::context::SignalRef {
        name: signal.to_string(),
        payload,
        correlation,
        received_at: util::now_iso(),
    };
    *resolved_slot.lock() = Some(sig);
    notify.notify_one();
    json!({
        "status": "wait_resolved",
        "arc_id": arc_id,
        "wait_id": wait_id,
        "signal": signal,
    })
}

pub(crate) async fn roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<RosterQuery>,
) -> Result<axum::Json<Vec<BroRosterEntry>>, axum::http::StatusCode> {
    let store_dir = state.store_dir.clone();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();

    let no_selectors =
        wanted_teams.is_empty() && wanted_bros.is_empty() && wanted_sessions.is_empty();

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();

    // Team selectors — each contributes all members. Unknown teams are
    // skipped silently; the empty roster speaks for itself at the CLI layer.
    for tn in &wanted_teams {
        if let Some(team) = orchestration::team::load_team(tn, &store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors — include every match across all teams (deduped by team::bro).
    if !wanted_bros.is_empty() {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                if !wanted_bros.iter().any(|b| b == &member.name) {
                    continue;
                }
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Session selectors — synthetic adhoc lanes.
    for sid in &wanted_sessions {
        let key = format!("session::{sid}");
        if !seen.insert(key) {
            continue;
        }
        let path = index::find_session_file(sid, &config.roots, config.codex_root.as_deref());
        let provider = path.as_deref().and_then(infer_provider_from_path);
        entries.push(BroRosterEntry {
            bro: sid.chars().take(8).collect(),
            bro_selector: sid.clone(),
            team: "adhoc".into(),
            provider: provider
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into()),
            account: None,
            session_id: Some(sid.clone()),
            jsonl_path: path.map(|p| p.to_string_lossy().into_owned()),
            brofile: String::new(),
            model: None,
        });
    }

    // No selectors → full roster across every team (legacy default).
    if no_selectors {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors that the team-walk above didn't resolve fall
    // through here: we synthesize ad-hoc entries from currently-known
    // tasks whose `bro_label` matches. This is the only path that
    // surfaces brofile-only dispatched bros (workflow implementer /
    // advisor nodes) — they have no team membership, so the team
    // walk skips them. Without this, `bro tail keystone-impl` returns
    // an empty roster and the CLI bails with "bro does not exist".
    if !wanted_bros.is_empty() {
        let task_store = state.task_store.read();
        for task in task_store.all_tasks() {
            let inner = task.inner.lock();
            let label = match &inner.bro_label {
                Some(l) => l.clone(),
                None => continue,
            };
            // Match either bare-label (`keystone-impl`) or the
            // `team::member` form so callers can use either.
            let (team, member) = match label.split_once("::") {
                Some((t, m)) => (t.to_string(), m.to_string()),
                None => ("adhoc".to_string(), label.clone()),
            };
            let matches = wanted_bros.iter().any(|w| w == &member || w == &label);
            if !matches {
                continue;
            }
            let key = format!("{team}::{member}");
            if !seen.insert(key) {
                continue;
            }
            let session_id = if inner.session_id == "pending" {
                None
            } else {
                Some(inner.session_id.clone())
            };
            let jsonl_path = session_id.as_deref().and_then(|sid| {
                index::find_session_file(sid, &config.roots, config.codex_root.as_deref())
                    .map(|p| p.to_string_lossy().into_owned())
            });
            entries.push(BroRosterEntry {
                bro: member,
                bro_selector: label,
                team,
                provider: inner.provider.to_string(),
                account: None,
                session_id,
                jsonl_path,
                brofile: String::new(),
                model: None,
            });
        }
    }

    if !wanted_providers.is_empty() {
        entries.retain(|e| {
            e.provider
                .parse::<Provider>()
                .ok()
                .map(|p| wanted_providers.contains(&p))
                .unwrap_or(false)
        });
    }

    Ok(axum::Json(entries))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(tmp.path())))
    }

    #[test]
    fn rebuild_releases_store_locks_before_taking_edge_index_write() {
        // Regression for the rebuild/reindex/blame deadlock: rebuild must not
        // hold idx.read()/kb.read() while acquiring edge_index.write(). We hold
        // edge_index.read() to force the rebuild to park on edge_index.write(),
        // then prove idx and kb are still acquirable during that wait. Pre-fix
        // the rebuild held idx.read across the write acquisition, so idx.write()
        // would never succeed here (the deadlock window).
        use std::sync::Arc;
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Hold a reader on edge_index so the rebuild's final write blocks.
        let held = state.edge_index.read();

        let st = state.clone();
        let handle = std::thread::spawn(move || {
            rebuild_edge_index_from_shared(&st, false);
        });

        // Let the rebuild acquire its store read-locks, finish computing
        // (trivial on an empty test corpus), and PARK on edge_index.write()
        // (blocked because `held` is alive). It cannot return until we drop
        // `held`, so after this settle it is definitively waiting on the write.
        // No early break — we must observe the steady state, not the
        // pre-acquisition race (an early break is what made the first cut of
        // this test pass against the buggy code).
        std::thread::sleep(Duration::from_millis(400));

        // The rebuild must still be parked (it can't complete until we release).
        assert!(
            !handle.is_finished(),
            "precondition: rebuild should be blocked on edge_index.write()"
        );
        // Fixed code dropped the store read-guards before acquiring the write,
        // so idx/kb are free now. Buggy code holds idx.read()/kb.read() while
        // parked here, so these would be None.
        assert!(
            state.idx.try_write().is_some(),
            "idx.write() must be free while rebuild waits on edge_index.write()"
        );
        assert!(
            state.kb.try_write().is_some(),
            "kb.write() must be free while rebuild waits on edge_index.write()"
        );

        // Let the rebuild finish.
        drop(held);
        handle.join().unwrap();
    }

    #[test]
    fn edge_sidecar_signature_ignores_write_tmp_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let mat = edges_dir.join("materialized/workspace/p");
        std::fs::create_dir_all(&mat).unwrap();
        std::fs::write(mat.join("dirty-current/project.jsonl"), "x").unwrap_or(());
        std::fs::create_dir_all(mat.join("dirty-current")).unwrap();
        std::fs::write(mat.join("dirty-current/project.jsonl"), "committed").unwrap();
        let base = edge_sidecar_signature(edges_dir);

        // An in-progress temp dir's jsonl must not move the signature.
        std::fs::create_dir_all(mat.join("dirty-current.write-tmp")).unwrap();
        std::fs::write(
            mat.join("dirty-current.write-tmp/project.jsonl"),
            "half-written-overlay",
        )
        .unwrap();
        assert_eq!(
            base,
            edge_sidecar_signature(edges_dir),
            "*.write-tmp jsonl must not affect the signature"
        );
    }

    #[test]
    fn edge_sidecar_signature_tracks_manifest_index_active_pointers() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let mi = crate::manifest::manifest_index_path(edges_dir);
        std::fs::create_dir_all(mi.parent().unwrap()).unwrap();

        // Baseline: no manifest-index present.
        let sig0 = edge_sidecar_signature(edges_dir);

        std::fs::write(&mi, br#"{"version":1,"workspaces":{}}"#).unwrap();
        let sig1 = edge_sidecar_signature(edges_dir);
        assert_ne!(
            sig0, sig1,
            "manifest-index presence must register in the signature"
        );

        // A different active-pointer set — e.g. a branch switch flipping
        // active_snapshot between two already-materialized snapshots — changes
        // no `.jsonl` mtime, so only the manifest-index fold catches it.
        std::fs::write(
            &mi,
            br#"{"version":1,"workspaces":{"p":{"manifest":"m","active_snapshot":"workspace/p/snapshots/head-x"}}}"#,
        )
        .unwrap();
        let sig2 = edge_sidecar_signature(edges_dir);
        assert_ne!(
            sig1, sig2,
            "active-pointer change must change the signature even with no .jsonl change"
        );
    }

    #[tokio::test]
    async fn read_artifact_source_rejects_oversized_http_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Type: application/json\r\n",
                "Content-Length: 1048577\r\n",
                "\r\n",
                "{}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let err = read_artifact_source(&format!("http://{addr}/artifact.json"))
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("too large"), "got: {err}");
    }
    #[test]
    fn orchestrate_status_resolves_arc_id_to_arc_thread_id() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        server.state.running_arcs.write().insert(
            "thread-test1234".into(),
            ArcSnapshot {
                arc_id: "arc-test1234".into(),
                arc_thread_id: "thread-test1234".into(),
                workflow_name: "test-workflow".into(),
                workflow_version: 1,
                status: "completed".into(),
                current_node: None,
                completed_nodes: vec!["Done".into()],
                in_flight_nodes: vec![],
                last_verdict: Some("satisfied".into()),
                visit_counts: std::collections::HashMap::new(),
                started_at: "2026-05-16T00:00:00Z".into(),
                updated_at: "2026-05-16T00:00:01Z".into(),
            },
        );

        assert_eq!(
            crate::server::routes::resolve_orchestrate_thread_id(&server.state, "arc-test1234"),
            "thread-test1234"
        );
        assert_eq!(
            crate::server::routes::resolve_orchestrate_thread_id(&server.state, "thread-test1234"),
            "thread-test1234"
        );
    }

    /// /control/closeout (Phase 3a, design/fleet-tui/closeout-command.md §4.1)
    /// validates the request before reaching the driver: a disposition not in
    /// the keep/preflight/discard/publish/merge/adopt set returns 400 with a
    /// clear error body. This is the cheapest guard-level assertion — the
    /// other guards (unmanaged worktree, detached HEAD, branch prefix)
    /// require git setup and are covered by the bro-tools unit tests.
    #[tokio::test]
    async fn control_closeout_rejects_unknown_disposition() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "definitely-not-a-real-disposition".to_string(),
            confirm: true,
            target: None,
            commit_message: None,
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "unknown disposition must yield 400"
        );
    }

    /// /control/closeout enforces the confirm gate on mutating dispositions
    /// (discard/publish/merge/adopt), matching `exit_worktree` exactly.
    /// publish without confirm returns 400.
    #[tokio::test]
    async fn control_closeout_publish_without_confirm_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "publish".to_string(),
            confirm: false,
            target: None,
            commit_message: Some("test commit".to_string()),
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "publish without confirm must yield 400"
        );
    }

    /// /control/closeout also enforces publish's commit_message gate, matching
    /// the tool's preflight bail ("publish requires commit_message").
    #[tokio::test]
    async fn control_closeout_publish_without_commit_message_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "publish".to_string(),
            confirm: true,
            target: None,
            commit_message: None,
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "publish without commit_message must yield 400"
        );
    }

    // ── /control/roster — Slice 1a focused test ──────────────────────────
    //
    // Spec: the DTO serializes with the expected fields and has NO
    // events field (regression guard for the cf87a52 truncation
    // class); the handler returns one summary per task in
    // state.task_store. The DTO field-shape assertion lives in
    // `crates/bro-protocol/src/lib.rs::tests::roster_summary_v1_serializes_without_events_field`;
    // this test focuses on the handler: insert N tasks, get back N
    // summaries, none of them carrying an events array.
    #[tokio::test]
    async fn control_roster_returns_one_summary_per_task() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Seed two tasks — one running, one completed — so we can also
        // see that the status mapping is consistent (not asserted
        // here; the DTO test owns the field shape, this test owns
        // the count + absence of events).
        {
            let mut store = state.task_store.write();
            store
                .insert(
                    "task-a".to_string(),
                    orchestration::test_task(
                        "task-a",
                        orchestration::TaskStatus::Running,
                        Provider::Glm,
                    ),
                )
                .expect("insert task-a");
            store
                .insert(
                    "task-b".to_string(),
                    orchestration::test_task(
                        "task-b",
                        orchestration::TaskStatus::Completed,
                        Provider::Deepseek,
                    ),
                )
                .expect("insert task-b");
        }

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("roster body must be valid JSON");
        let tasks = value
            .get("tasks")
            .and_then(|t| t.as_array())
            .expect("envelope must carry a `tasks` array");
        assert_eq!(
            tasks.len(),
            2,
            "expected one RosterSummaryV1 per task in the store, got {tasks:?}"
        );

        // Per-summary regression guard: no events field, no
        // recentEvents field, no array-typed field at all (a Vec
        // empty-or-otherwise would reopen the 80KB truncation class).
        for (i, summary) in tasks.iter().enumerate() {
            let obj = summary
                .as_object()
                .unwrap_or_else(|| panic!("summary[{i}] must be an object"));
            assert!(
                !obj.contains_key("events"),
                "summary[{i}] must NOT carry an `events` field (cf87a52 regression guard)"
            );
            assert!(
                !obj.contains_key("recentEvents"),
                "summary[{i}] must NOT carry a `recentEvents` field"
            );
            for (k, v) in obj {
                assert!(
                    !v.is_array(),
                    "summary[{i}].{k} unexpectedly serialized as array: {v}"
                );
            }
        }

        // Envelope shape: { version, tasks }. Version is the roster
        // generation; this test asserts only presence, while the Slice 2
        // delta test owns generation semantics.
        let obj = value.as_object().expect("envelope must be an object");
        assert!(obj.contains_key("version"), "envelope must carry `version`");
        assert_eq!(
            obj.len(),
            2,
            "envelope must carry exactly `version` and `tasks`"
        );
    }

    #[tokio::test]
    async fn control_roster_forget_drops_only_terminal_tasks_from_snapshot() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        {
            let mut store = state.task_store.write();
            store
                .insert(
                    "running".to_string(),
                    orchestration::test_task(
                        "running",
                        orchestration::TaskStatus::Running,
                        Provider::Glm,
                    ),
                )
                .expect("insert running");
            store
                .insert(
                    "terminal".to_string(),
                    orchestration::test_task(
                        "terminal",
                        orchestration::TaskStatus::Completed,
                        Provider::Brodex,
                    ),
                )
                .expect("insert terminal");
        }

        let running_resp = control_roster_forget_handler(
            AxumState(state.clone()),
            axum::extract::Path("running".to_string()),
        )
        .await;
        assert_eq!(running_resp.status(), axum::http::StatusCode::CONFLICT);

        let terminal_resp = control_roster_forget_handler(
            AxumState(state.clone()),
            axum::extract::Path("terminal".to_string()),
        )
        .await;
        assert_eq!(terminal_resp.status(), axum::http::StatusCode::OK);

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: bro_protocol::RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        let ids: Vec<String> = snapshot
            .tasks
            .into_iter()
            .map(|task| task.task_id.as_str().to_string())
            .collect();

        assert_eq!(ids, vec!["running".to_string()]);
    }

    #[tokio::test]
    async fn control_roster_generation_is_shared_by_snapshot_and_deltas() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::{RosterDelta, RosterSnapshotV1};
        use tokio::time::{Duration, timeout};

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let mut rx = state.roster_tx.subscribe();

        let task = orchestration::spawn_in_process_task(
            "task-roster-generation".to_string(),
            Provider::Workflow,
            "session-roster-generation".to_string(),
            None,
            state.store_dir.clone(),
            state.task_store.clone(),
            state.tail_tx.clone(),
            Some(state.roster_events()),
            Some("roster-test".to_string()),
            None,
            Some(state.system_events.clone()),
            bro_core::Origin::Workflow,
        );

        let added = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("added delta should arrive")
            .expect("roster channel open");
        let added_seq = added.seq();
        match added {
            RosterDelta::Added { seq, task } => {
                assert_eq!(seq, 1, "first roster mutation should use generation 1");
                assert_eq!(task.task_id.as_str(), "task-roster-generation");
            }
            other => panic!("expected Added delta, got {other:?}"),
        }

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        assert_eq!(
            snapshot.version, added_seq,
            "snapshot version must read the same generation counter as deltas"
        );

        orchestration::push_in_process_event(
            &task,
            serde_json::json!({"type":"assistant","message":{"model":"test-model"}}),
        );
        let updated = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("updated delta should arrive")
            .expect("roster channel open");
        match updated {
            RosterDelta::Updated { seq, task } => {
                assert_eq!(seq, snapshot.version + 1);
                assert_eq!(task.task_id.as_str(), "task-roster-generation");
                assert_eq!(task.model.as_deref(), Some("test-model"));
            }
            other => panic!("expected Updated delta, got {other:?}"),
        }

        orchestration::finish_in_process_task(
            &task,
            orchestration::TaskStatus::Completed,
            Some("done".to_string()),
            None,
            &state.task_store,
            &state.store_dir,
            &state.tail_tx,
            Some(state.system_events.clone()),
        );
        let terminal_update = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal update delta should arrive")
            .expect("roster channel open");
        assert!(
            matches!(terminal_update, RosterDelta::Updated { .. }),
            "terminal status transition should emit Updated, got {terminal_update:?}"
        );

        let server = BlackboxServer::new(state.clone());
        server.bro_prune(Parameters(crate::tools::bro_params::PruneParams {
            status: None,
            provider: None,
            older_than_hours: None,
            dry_run: None,
            task_ids: Some(vec!["task-roster-generation".to_string()]),
            retro: None,
            retro_min_turns: None,
            retro_max: None,
        }));
        let removed = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("removed delta should arrive")
            .expect("roster channel open");
        match removed {
            RosterDelta::Removed { seq, task_id } => {
                assert!(seq > terminal_update.seq());
                assert_eq!(task_id.as_str(), "task-roster-generation");
            }
            other => panic!("expected Removed delta, got {other:?}"),
        }
    }

    // Slice 1b — `/control/roster` must surface the spawn-time
    // `origin` for every task so the roster UI can tab Fleet vs
    // Dispatched vs Workflow vs Atom without re-deriving. The
    // handler reads `inner.origin` (Slice 1b plumbing) and projects
    // it onto `RosterSummaryV1.origin` (added in Slice 1b); this
    // test seeds two tasks with explicit different origins and
    // asserts the per-task summary carries the right one.
    #[tokio::test]
    async fn control_roster_projects_origin_per_task() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Seed two tasks with deliberately different origins so a
        // mis-projection (e.g. always defaulting to `unknown` or
        // always reading the same task) would be visible.
        {
            let mut store = state.task_store.write();
            let task_a = orchestration::test_task(
                "task-cockpit",
                orchestration::TaskStatus::Running,
                Provider::Glm,
            );
            task_a.inner.lock().origin = bro_core::Origin::Cockpit;
            store
                .insert("task-cockpit".to_string(), task_a)
                .expect("insert task-cockpit");

            let task_b = orchestration::test_task(
                "task-workflow",
                orchestration::TaskStatus::Running,
                Provider::Glm,
            );
            {
                let mut inner = task_b.inner.lock();
                inner.origin = bro_core::Origin::Workflow;
                inner.workflow_owned = orchestration::workflow_owned_for_origin(inner.origin);
            }
            store
                .insert("task-workflow".to_string(), task_b)
                .expect("insert task-workflow");
        }

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let tasks = value
            .get("tasks")
            .and_then(|t| t.as_array())
            .expect("envelope must carry a `tasks` array");
        assert_eq!(tasks.len(), 2);

        // Per-summary `origin` projection check. We index by
        // task_id (stable) rather than array order (HashMap-iteration
        // order).
        let mut by_id = std::collections::HashMap::new();
        for s in tasks {
            let obj = s.as_object().expect("summary must be object");
            let task_id = obj
                .get("task_id")
                .and_then(|v| v.as_str())
                .expect("summary must carry task_id")
                .to_string();
            let origin = obj
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("summary[{task_id}] must carry lowercase origin"))
                .to_string();
            by_id.insert(task_id, origin);
        }

        assert_eq!(
            by_id.get("task-cockpit").map(String::as_str),
            Some("cockpit"),
            "cockpit-origin task must project as \"cockpit\" on the roster"
        );
        assert_eq!(
            by_id.get("task-workflow").map(String::as_str),
            Some("workflow"),
            "workflow-origin task must project as \"workflow\" on the roster"
        );
    }

    #[tokio::test]
    async fn control_roster_surfaces_worktree_and_workflow_owned_metadata() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::RosterSnapshotV1;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let task = orchestration::test_task(
            "task-meta",
            orchestration::TaskStatus::Running,
            Provider::Workflow,
        );
        {
            let mut inner = task.inner.lock();
            inner.managed_worktree = Some("/tmp/managed/task-meta".to_string());
            inner.origin = bro_core::Origin::Workflow;
            inner.workflow_owned = orchestration::workflow_owned_for_origin(inner.origin);
        }
        state
            .task_store
            .write()
            .insert("task-meta".to_string(), task)
            .expect("insert task-meta");

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        let summary = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id.as_str() == "task-meta")
            .expect("task-meta should be present");
        assert_eq!(
            summary.managed_worktree.as_deref(),
            Some("/tmp/managed/task-meta")
        );
        assert!(summary.workflow_owned);
    }
}
