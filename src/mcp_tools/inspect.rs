use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::edge_index::{Edge, EdgeIndex};
use crate::entity_loader;
use crate::entity_ref::EntityRef;
use crate::providers::{self, EntityView, Neighborhood, ProviderContext};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct InspectEntityParams {
    pub entity_ref: String,
    pub edge_types: Option<String>,
    pub direction: Option<String>,
    pub per_type_limit: Option<usize>,
    pub property_mode: Option<String>,
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
    source_label: Option<String>,
    target: String,
    target_label: Option<String>,
    direction: String,
}

pub fn bad_input(entity_ref: &str, message: impl AsRef<str>) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "message": message.as_ref(),
            "field": "entity_ref",
            "suggested_fix": "Use a canonical EntityRef such as knowledge:<entry_id>, project_file:<project_id>:<rel_path_hash>:<chunk_hash>:<occurrence_idx>, or commit:<repo_id>:<sha>."
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

pub fn similar_refs(edge_index: &EdgeIndex, r: &EntityRef) -> Vec<String> {
    let needle = r.to_string();
    let prefix = r.entity_type().as_str();
    edge_index
        .known_refs()
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
        Err(_) => return Ok(not_found(r, similar_refs(edge_index, r))),
    };
    let full_neighborhood = full_neighborhood(edge_index, r);
    entity.neighborhood = filtered_neighborhood(
        &full_neighborhood,
        direction,
        edge_filter.as_ref(),
        per_type_limit,
    );
    let rendered_forward = render_edges(ctx, &entity.neighborhood.forward, "out");
    let rendered_reverse = render_edges(ctx, &entity.neighborhood.reverse, "in");
    let recommended = provider.recommended_next_hops(&entity, &full_neighborhood);
    let coverage = provider
        .expected_edge_families(r)
        .into_iter()
        .map(|expectation| {
            let count = full_neighborhood
                .forward
                .iter()
                .chain(full_neighborhood.reverse.iter())
                .filter(|edge| edge.kind == expectation.family_name)
                .count();
            json!({
                "family": expectation.family_name,
                "count": count,
                "expected": if expectation.required { "required" } else { "optional" },
                "status": if count > 0 { "present" } else { "0 (expected)" },
            })
        })
        .collect::<Vec<_>>();
    let properties = render_properties(&entity, property_mode);
    let text = render_text(&entity, &properties, &rendered_forward, &rendered_reverse);
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
        "recommended_next_hops": recommended
            .into_iter()
            .map(|hop| json!({"edge_family": hop.edge_family_name, "count": hop.count}))
            .collect::<Vec<_>>(),
        "edge_family_coverage": coverage,
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

fn full_neighborhood(edge_index: &EdgeIndex, r: &EntityRef) -> Neighborhood {
    Neighborhood {
        forward: edge_index.forward_edges(r).to_vec(),
        reverse: edge_index.reverse_edges(r).to_vec(),
    }
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
        })
        .collect()
}

pub(crate) fn compact_label(
    ctx: &ProviderContext<'_>,
    r: &EntityRef,
    loaded: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    entity_loader::compact_label(ctx, r, loaded)
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

fn render_text(
    entity: &EntityView,
    properties: &BTreeMap<String, String>,
    forward: &[RenderedEdge],
    reverse: &[RenderedEdge],
) -> String {
    let mut text = format!("## {}\n\n", entity.ref_string);
    text.push_str("### Properties\n");
    for (key, value) in properties {
        text.push_str(&format!("- `{key}`: {value}\n"));
    }
    text.push_str("\n### Edges\n");
    text.push_str(&format!("- out: {}\n", forward.len()));
    text.push_str(&format!("- in: {}\n", reverse.len()));
    text
}

pub fn entity_type_count(refs: &[EntityRef]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for r in refs {
        *counts
            .entry(r.entity_type().as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
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
    fn entity_type_count_groups_refs_by_provider_type() {
        let refs = vec![
            EntityRef::parse("knowledge:a").unwrap(),
            EntityRef::parse("knowledge:b").unwrap(),
            EntityRef::parse("commit:repo:abcdef").unwrap(),
        ];
        let counts = entity_type_count(&refs);
        assert_eq!(counts.get("knowledge"), Some(&2));
        assert_eq!(counts.get("commit"), Some(&1));
    }
}
