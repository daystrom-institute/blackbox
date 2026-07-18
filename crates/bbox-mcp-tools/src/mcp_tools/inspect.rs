use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_providers::entity_loader;
use bbox_providers::providers::{self, EntityView, Neighborhood, NextHop, ProviderContext};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectEntityParams {
    pub entity_ref: String,
    pub edge_types: Option<String>,
    pub direction: Option<String>,
    pub per_type_limit: Option<usize>,
    pub property_mode: Option<String>,
    /// Include explicitly opted-in `.bbox/local/graphs` scratch graphs.
    /// Defaults false.
    #[serde(default)]
    pub include_local_graphs: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectDirection {
    Out,
    In,
    Both,
}

impl InspectDirection {
    fn parse(input: Option<&str>) -> Result<Self> {
        match input.unwrap_or("both").to_ascii_lowercase().as_str() {
            "out" => Ok(Self::Out),
            "in" => Ok(Self::In),
            "both" => Ok(Self::Both),
            other => bail!("invalid direction `{other}`; use out, in, or both"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyMode {
    Summary,
    Smart,
    Full,
}

impl PropertyMode {
    fn parse(input: Option<&str>) -> Self {
        match input.unwrap_or("smart").to_ascii_lowercase().as_str() {
            "summary" => Self::Summary,
            "full" => Self::Full,
            _ => Self::Smart,
        }
    }
}

#[derive(Debug, Serialize)]
struct RenderedEdge {
    kind: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_label: Option<String>,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_label: Option<String>,
    direction: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    properties: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
struct RenderedNextHop {
    edge_family: String,
    count: usize,
}

#[derive(Debug, Serialize)]
struct RenderedCoverage {
    family: String,
    count: usize,
    expected: String,
    status: String,
}

pub fn bad_input(entity_ref: &str, message: impl AsRef<str>) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message.as_ref(),
            "field": "entity_ref",
            "suggested_fix": "Use a canonical EntityRef such as knowledge:<entry_id>, project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>, project_graph_vertex:<scope_id>:<graph_id>:<vertex_id>, or commit:<repo_id>:<sha>."
        },
        "entity_ref": entity_ref,
    })
    .to_string()
}

pub fn not_found(r: &EntityRef, similar_refs: Vec<String>) -> String {
    json!({
        "status": "error.not_found",
        "error": {
            "code": "error.not_found",
            "message": format!("No entity found for {r}"),
            "ref": r.to_string(),
            "similar_refs": similar_refs,
        }
    })
    .to_string()
}

pub fn similar_refs(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    r: &EntityRef,
) -> Vec<String> {
    let needle = r.to_string();
    let prefix = r.entity_type().as_str();
    let mut known = edge_index.known_refs();
    known.extend(ctx.project_graph_known_refs());
    known
        .into_iter()
        .map(|known| known.to_string())
        .filter(|known| known.starts_with(prefix))
        .filter(|known| known != &needle)
        .take(5)
        .collect()
}

pub fn inspect_entity(
    p: &InspectEntityParams,
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    edge_index: &EdgeIndex,
) -> Result<String> {
    let direction = match InspectDirection::parse(p.direction.as_deref()) {
        Ok(direction) => direction,
        Err(err) => return Ok(bad_input(&p.entity_ref, err.to_string())),
    };
    let property_mode = PropertyMode::parse(p.property_mode.as_deref());
    let per_type_limit = p.per_type_limit.unwrap_or(5);
    let edge_filter = parse_edge_filter(p.edge_types.as_deref());
    let provider = providers::provider_for(r.entity_type());
    let mut entity = match entity_loader::load(ctx, r) {
        Ok(entity) => entity,
        Err(_) => return Ok(not_found(r, similar_refs(ctx, edge_index, r))),
    };
    let full_neighborhood = full_neighborhood(ctx, edge_index, r);
    entity.neighborhood = filtered_neighborhood(
        &full_neighborhood,
        direction,
        edge_filter.as_ref(),
        per_type_limit,
    );
    let rendered_forward = render_edges(ctx, &entity.neighborhood.forward, "out");
    let rendered_reverse = render_edges(ctx, &entity.neighborhood.reverse, "in");
    let recommended = render_next_hops(provider.recommended_next_hops(&entity, &full_neighborhood));
    let coverage = provider
        .expected_edge_families(r)
        .into_iter()
        .take(10)
        .map(|expectation| {
            let count = full_neighborhood
                .forward
                .iter()
                .chain(full_neighborhood.reverse.iter())
                .filter(|edge| edge.kind == expectation.family_name)
                .count();
            RenderedCoverage {
                family: expectation.family_name,
                count,
                expected: if expectation.required {
                    "required"
                } else {
                    "optional"
                }
                .into(),
                status: if count > 0 { "present" } else { "0 (expected)" }.into(),
            }
        })
        .collect::<Vec<_>>();
    let properties = render_properties(&entity, property_mode);
    let text = render_text(InspectText {
        ctx,
        r,
        entity: &entity,
        properties: &properties,
        forward: &rendered_forward,
        reverse: &rendered_reverse,
        recommended: &recommended,
        coverage: &coverage,
    });
    // The full `coverage` drives the human `text` render above (which surfaces
    // optional-but-absent families as orientation). The structured array,
    // though, drops optional zero-count rows: a machine reader infers "absent"
    // from a family simply not being listed, so emitting `{count: 0, status:
    // "0 (expected)"}` for every unused optional family is pure padding.
    // Required families are retained even at count 0 so the "expected but
    // missing" signal still reaches a structured consumer.
    let coverage_json: Vec<&RenderedCoverage> = coverage
        .iter()
        .filter(|c| c.count > 0 || c.expected == "required")
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "entity_ref": r.to_string(),
        "entity_type": r.entity_type().as_str(),
        "properties": properties,
        "edges": {
            "out": rendered_forward,
            "in": rendered_reverse,
        },
        "recommended_next_hops": recommended,
        "edge_family_coverage": coverage_json,
    }))?)
}

fn parse_edge_filter(raw: Option<&str>) -> Option<HashSet<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn full_neighborhood(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    r: &EntityRef,
) -> Neighborhood {
    let mut neighborhood = Neighborhood {
        // forward_edges_with_synthesis fills in the transcript -> session
        // IN_SESSION edge at query time when it isn't materialized (see its
        // doc comment). This is forward only; reverse has no counterpart.
        forward: edge_index.forward_edges_with_synthesis(r),
        reverse: edge_index.reverse_edges(r).into_iter().cloned().collect(),
    };
    neighborhood
        .forward
        .extend(ctx.project_graph_forward_edges(r));
    neighborhood
        .reverse
        .extend(ctx.project_graph_reverse_edges(r));
    for edge in neighborhood
        .forward
        .iter_mut()
        .chain(neighborhood.reverse.iter_mut())
    {
        resolve_evidence_edge(ctx, edge);
    }
    neighborhood
}

pub fn resolve_evidence_edge(ctx: &ProviderContext<'_>, edge: &mut Edge) {
    let Some(scope_id) = edge.metadata.get("evidence.scope_id").cloned() else {
        return;
    };
    let source_generation = edge
        .metadata
        .get("evidence.source_generation")
        .and_then(|value| value.parse::<u64>().ok());
    let target_generation = edge
        .metadata
        .get("evidence.target_generation")
        .and_then(|value| value.parse::<u64>().ok());
    let source_status = evidence_endpoint_status(ctx, &scope_id, &edge.source, source_generation);
    let target_status = evidence_endpoint_status(ctx, &scope_id, &edge.target, target_generation);
    edge.metadata
        .insert("evidence.source_status".into(), source_status.into());
    edge.metadata
        .insert("evidence.target_status".into(), target_status.into());
    edge.metadata.insert(
        "evidence.freshness".into(),
        aggregate_evidence_status(source_status, target_status).into(),
    );
}

fn evidence_endpoint_status(
    ctx: &ProviderContext<'_>,
    scope_id: &str,
    entity: &EntityRef,
    expected_generation: Option<u64>,
) -> &'static str {
    if evidence_entity_scope(entity).is_some_and(|entity_scope| entity_scope != scope_id) {
        return "unauthorized";
    }
    match entity_loader::load(ctx, entity) {
        Ok(view) => {
            if expected_generation.is_some_and(|expected| {
                view.properties
                    .get("generation")
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|current| current != expected)
            }) {
                "stale"
            } else {
                "current"
            }
        }
        Err(_)
            if matches!(entity, EntityRef::ProjectGraphVertex { .. })
                && expected_generation.is_some() =>
        {
            "stale"
        }
        Err(_) => "missing",
    }
}

fn evidence_entity_scope(entity: &EntityRef) -> Option<&str> {
    match entity {
        EntityRef::ProjectGraphVertex { scope_id, .. } => Some(scope_id),
        EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => Some(project_id),
        _ => None,
    }
}

fn aggregate_evidence_status(source: &str, target: &str) -> &'static str {
    for status in ["unauthorized", "stale", "missing", "unresolved"] {
        if source == status || target == status {
            return status;
        }
    }
    "current"
}

fn filtered_neighborhood(
    full: &Neighborhood,
    direction: InspectDirection,
    edge_filter: Option<&HashSet<String>>,
    per_type_limit: usize,
) -> Neighborhood {
    if per_type_limit == 0 {
        return Neighborhood::default();
    }
    let forward = if matches!(direction, InspectDirection::Out | InspectDirection::Both) {
        filter_and_limit(&full.forward, edge_filter, per_type_limit)
    } else {
        Vec::new()
    };
    let reverse = if matches!(direction, InspectDirection::In | InspectDirection::Both) {
        filter_and_limit(&full.reverse, edge_filter, per_type_limit)
    } else {
        Vec::new()
    };
    Neighborhood { forward, reverse }
}

fn filter_and_limit(
    edges: &[Edge],
    edge_filter: Option<&HashSet<String>>,
    per_type_limit: usize,
) -> Vec<Edge> {
    let mut counts = HashMap::<&str, usize>::new();
    let mut out = Vec::new();
    for edge in edges {
        if edge_filter.is_some_and(|allowed| !allowed.contains(&edge.kind)) {
            continue;
        }
        let count = counts.entry(edge.kind.as_str()).or_default();
        if *count >= per_type_limit {
            continue;
        }
        *count += 1;
        out.push(edge.clone());
    }
    out
}

fn render_edges(ctx: &ProviderContext<'_>, edges: &[Edge], direction: &str) -> Vec<RenderedEdge> {
    edges
        .iter()
        .map(|edge| RenderedEdge {
            kind: edge.kind.clone(),
            source: edge.source.to_string(),
            source_label: compact_label(ctx, &edge.source, None),
            target: edge.target.to_string(),
            target_label: compact_label(ctx, &edge.target, None),
            direction: direction.to_string(),
            properties: edge.metadata.clone(),
        })
        .collect()
}

pub fn compact_label(
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    loaded: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    entity_loader::compact_label(ctx, r, loaded)
}

fn render_next_hops(hops: Vec<NextHop>) -> Vec<RenderedNextHop> {
    hops.into_iter()
        .take(5)
        .map(|hop| RenderedNextHop {
            edge_family: hop.edge_family_name,
            count: hop.count,
        })
        .collect()
}

fn render_properties(entity: &EntityView, property_mode: PropertyMode) -> BTreeMap<String, String> {
    let summary_keys = ["name", "title", "status", "kind", "severity", "id"];
    entity
        .properties
        .iter()
        .filter(|(key, _)| {
            property_mode != PropertyMode::Summary
                || summary_keys.contains(&key.as_str())
                || key.ends_with("_id")
        })
        .map(|(key, value)| {
            let value = match property_mode {
                PropertyMode::Full => value.clone(),
                PropertyMode::Smart if value.len() > 300 => {
                    format!("{}...", value.chars().take(300).collect::<String>())
                }
                _ => value.clone(),
            };
            (key.clone(), value)
        })
        .collect()
}

struct InspectText<'a> {
    ctx: &'a ProviderContext<'a>,
    r: &'a EntityRef,
    entity: &'a EntityView,
    properties: &'a BTreeMap<String, String>,
    forward: &'a [RenderedEdge],
    reverse: &'a [RenderedEdge],
    recommended: &'a [RenderedNextHop],
    coverage: &'a [RenderedCoverage],
}

fn render_text(input: InspectText<'_>) -> String {
    let InspectText {
        ctx,
        r,
        entity,
        properties,
        forward,
        reverse,
        recommended,
        coverage,
    } = input;
    let header = compact_label(ctx, r, Some(&entity.properties))
        .unwrap_or_else(|| entity.ref_string.clone());
    let mut text = format!("## {header}\n\n");
    text.push_str(&format!(
        "`{}` ({})\n\n",
        entity.ref_string,
        entity.entity_type.as_str()
    ));
    text.push_str("### Properties\n");
    for (key, value) in properties {
        text.push_str(&format!("- `{key}`: {value}\n"));
    }
    text.push_str("\n### Edges\n");
    if forward.is_empty() && reverse.is_empty() {
        text.push_str("  (no edges)\n");
    } else {
        for edge in forward {
            text.push_str(&format!(
                "  -->[{}{}] {}\n",
                edge.kind,
                rendered_edge_freshness(edge),
                labeled_ref(&edge.target, edge.target_label.as_deref())
            ));
        }
        for edge in reverse {
            text.push_str(&format!(
                "  <--[{}{}] {}\n",
                edge.kind,
                rendered_edge_freshness(edge),
                labeled_ref(&edge.source, edge.source_label.as_deref())
            ));
        }
    }
    text.push_str("\n### Recommended next hops\n");
    if recommended.is_empty() {
        text.push_str("  (none)\n");
    } else {
        for hop in recommended {
            text.push_str(&format!("  {} ({})\n", hop.edge_family, hop.count));
        }
    }
    text.push_str("\n### Edge family coverage\n");
    if coverage.is_empty() {
        text.push_str("  (none)\n");
    } else {
        for row in coverage {
            text.push_str(&format!(
                "  {}: count={} expected={} status={}\n",
                row.family, row.count, row.expected, row.status
            ));
        }
    }
    text
}

fn rendered_edge_freshness(edge: &RenderedEdge) -> String {
    edge.properties
        .get("evidence.freshness")
        .map(|status| format!(" {status}"))
        .unwrap_or_default()
}

fn labeled_ref(entity_ref: &str, label: Option<&str>) -> String {
    match label {
        Some(label) if label != entity_ref => format!("{entity_ref} ({label})"),
        _ => entity_ref.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_input_uses_design_error_shape() {
        let rendered = bad_input("not-a-ref", "invalid ref");
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "error.bad_input");
        assert_eq!(value["error"]["code"], "error.bad_input");
        assert_eq!(value["error"]["field"], "entity_ref");
    }

    #[test]
    fn not_found_includes_similar_refs_field() {
        let r = EntityRef::parse("knowledge:missing").unwrap();
        let rendered = not_found(&r, vec!["knowledge:nearby".to_string()]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "error.not_found");
        assert_eq!(value["error"]["code"], "error.not_found");
        assert_eq!(value["error"]["similar_refs"][0], "knowledge:nearby");
    }

    #[test]
    fn inspect_system_memory_ref() {
        bbox_system_memory::init_for_tests_from(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../system-defaults/memories"
        )));
        let params = InspectEntityParams {
            entity_ref: "system_memory:sm-agentic-opening-sequence".into(),
            edge_types: None,
            direction: None,
            per_type_limit: Some(0),
            property_mode: Some("summary".into()),
            include_local_graphs: None,
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "ok");
        assert_eq!(
            value["entity_ref"],
            "system_memory:sm-agentic-opening-sequence"
        );
        assert_eq!(value["properties"]["id"], "sm-agentic-opening-sequence");
        assert!(value["properties"].get("content").is_none());
    }

    #[test]
    fn inspect_entity_synthesizes_transcript_in_session_edge() {
        // gap-edc84378: a transcript ref with zero materialized edges must
        // still surface an IN_SESSION out-edge via
        // EdgeIndex::forward_edges_with_synthesis, and the (required) edge
        // family coverage row must report it present.
        let params = InspectEntityParams {
            entity_ref: "transcript:claude:sess-1:42:0".into(),
            edge_types: None,
            direction: None,
            per_type_limit: None,
            property_mode: Some("summary".into()),
            include_local_graphs: None,
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "ok");
        let out_edges = value["edges"]["out"].as_array().unwrap();
        assert!(
            out_edges
                .iter()
                .any(|edge| edge["kind"] == "IN_SESSION"
                    && edge["target"] == "session:claude:sess-1"),
            "expected synthesized IN_SESSION out-edge, got {out_edges:?}"
        );
        let coverage = value["edge_family_coverage"].as_array().unwrap();
        assert!(
            coverage
                .iter()
                .any(|row| row["family"] == "IN_SESSION" && row["count"] == 1),
            "expected IN_SESSION coverage row with count=1, got {coverage:?}"
        );
    }

    #[test]
    fn edge_family_coverage_omits_optional_zero_count_rows() {
        // An entity with no edges in the index: every optional expected family
        // resolves to count 0. Those rows are padding and must not reach the
        // structured payload; only present (count > 0) or required families do.
        bbox_system_memory::init_for_tests_from(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../system-defaults/memories"
        )));
        let params = InspectEntityParams {
            entity_ref: "system_memory:sm-agentic-opening-sequence".into(),
            edge_types: None,
            direction: None,
            per_type_limit: Some(0),
            property_mode: Some("summary".into()),
            include_local_graphs: None,
        };
        let r = EntityRef::parse(&params.entity_ref).unwrap();
        let rendered = inspect_entity(
            &params,
            &ProviderContext::empty_for_tests(),
            &r,
            &EdgeIndex::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let coverage = value["edge_family_coverage"].as_array().unwrap();
        for row in coverage {
            let count = row["count"].as_u64().unwrap();
            let expected = row["expected"].as_str().unwrap();
            assert!(
                count > 0 || expected == "required",
                "optional zero-count family leaked into structured coverage: {row}"
            );
        }
    }
}
