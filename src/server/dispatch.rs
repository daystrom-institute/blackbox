use std::sync::Arc;

use serde_json::{Value, json};

use crate::packets::apply_with as apply_packet_with;
use crate::routing;
use crate::server::SharedState;
use crate::server::routes::dispatch_verdict;

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
