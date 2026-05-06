use std::collections::BTreeSet;

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::edge_index::{Edge, EdgeIndex};
use crate::entity_loader;
use crate::entity_ref::EntityRef;
use crate::index::TranscriptIndex;
use crate::knowledge::Knowledge;
use crate::mcp_tools::hybrid_search::{self, HybridSearchParams};
use crate::providers::{self, Neighborhood, ProviderContext};

const DEFAULT_LIMIT: u64 = 8;
const MAX_LIMIT: u64 = 30;
const PER_DIRECTION_EDGE_LIMIT: usize = 2;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DiscoverSeedParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub doc_type: Option<String>,
    #[serde(default)]
    pub include_vectors: Option<bool>,
    #[serde(default)]
    pub vector_weight: Option<f32>,
    #[serde(default)]
    pub query_vector: Option<Vec<f32>>,
}

#[derive(Debug, Serialize)]
struct DiscoverSeedResponse {
    status: &'static str,
    text: String,
    seeds: Vec<SeedEntity>,
    vector_status: Value,
    degraded: Value,
}

#[derive(Debug, Serialize)]
struct SeedEntity {
    entity_ref: String,
    label: String,
    score: f32,
    match_source: String,
    notable_edges: Vec<NotableEdge>,
}

#[derive(Debug, Clone, Serialize)]
struct NotableEdge {
    kind: String,
    direction: String,
    target: String,
    target_label: String,
}

pub fn discover_seed_entities(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    p: &DiscoverSeedParams,
) -> Result<String> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let hybrid = hybrid_search::hybrid_search(
        index,
        knowledge,
        ctx,
        &HybridSearchParams {
            query: p.query.clone(),
            limit: Some(limit),
            doc_type: p.doc_type.clone(),
            include_vectors: p.include_vectors,
            vector_weight: p.vector_weight,
            query_vector: p.query_vector.clone(),
        },
    )?;
    let hybrid: Value = serde_json::from_str(&hybrid)?;
    let seeds = hybrid
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| seed_from_hybrid_result(ctx, edge_index, result))
        .collect::<Vec<_>>();
    let vector_status = hybrid.get("vector_status").cloned().unwrap_or(Value::Null);
    let degraded = hybrid.get("degraded").cloned().unwrap_or(Value::Null);
    let text = render_text(&p.query, &seeds);
    Ok(serde_json::to_string_pretty(&DiscoverSeedResponse {
        status: "ok",
        text,
        seeds,
        vector_status,
        degraded,
    })?)
}

fn seed_from_hybrid_result(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    result: &Value,
) -> Option<SeedEntity> {
    let entity_ref = result.get("entity_id")?.as_str()?;
    let parsed = EntityRef::parse(entity_ref).ok()?;
    let label = result
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| entity_loader::compact_label(ctx, &parsed, None))
        .unwrap_or_else(|| entity_ref.to_string());
    let score = result.get("score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let match_source = result
        .get("sources")
        .and_then(Value::as_object)
        .map(match_source)
        .unwrap_or_else(|| "hybrid".into());
    Some(SeedEntity {
        entity_ref: entity_ref.to_string(),
        label,
        score,
        match_source,
        notable_edges: notable_edges(ctx, edge_index, &parsed),
    })
}

fn match_source(sources: &serde_json::Map<String, Value>) -> String {
    let has_bm25 = sources.contains_key("bm25");
    let has_vector = sources.keys().any(|key| key.starts_with("vector:"));
    match (has_bm25, has_vector) {
        (true, true) => "hybrid",
        (true, false) => "bm25",
        (false, true) => "vector",
        (false, false) => "hybrid",
    }
    .into()
}

fn notable_edges(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    entity_ref: &EntityRef,
) -> Vec<NotableEdge> {
    let forward = edge_index.forward_edges(entity_ref).to_vec();
    let reverse = edge_index.reverse_edges(entity_ref).to_vec();
    if forward.is_empty() && reverse.is_empty() {
        return Vec::new();
    }
    let neighborhood = Neighborhood { forward, reverse };
    let entity = entity_loader::load(ctx, entity_ref).unwrap_or_else(|_| {
        providers::empty_neighborhood_view(entity_ref, Default::default())
    });
    let priorities = providers::provider_for(entity_ref.entity_type())
        .recommended_next_hops(&entity, &neighborhood)
        .into_iter()
        .filter(|hop| hop.count > 0)
        .map(|hop| hop.edge_family_name)
        .collect::<Vec<_>>();
    if priorities.is_empty() {
        return Vec::new();
    }

    let priority_set = priorities.iter().collect::<BTreeSet<_>>();
    let mut out = Vec::new();
    out.extend(select_notable_edges(
        ctx,
        &neighborhood.forward,
        "out",
        &priorities,
        &priority_set,
    ));
    out.extend(select_notable_edges(
        ctx,
        &neighborhood.reverse,
        "in",
        &priorities,
        &priority_set,
    ));
    out
}

fn select_notable_edges(
    ctx: &ProviderContext<'_>,
    edges: &[Edge],
    direction: &str,
    priorities: &[String],
    priority_set: &BTreeSet<&String>,
) -> Vec<NotableEdge> {
    let mut selected = Vec::new();
    for family in priorities {
        for edge in edges.iter().filter(|edge| edge.kind == *family) {
            if selected.len() >= PER_DIRECTION_EDGE_LIMIT {
                return selected;
            }
            selected.push(render_notable_edge(ctx, edge, direction));
        }
    }
    if selected.len() < PER_DIRECTION_EDGE_LIMIT {
        for edge in edges
            .iter()
            .filter(|edge| priority_set.contains(&edge.kind))
        {
            if selected.len() >= PER_DIRECTION_EDGE_LIMIT {
                break;
            }
            if selected
                .iter()
                .any(|selected| selected.kind == edge.kind && selected.target == edge.target.to_string())
            {
                continue;
            }
            selected.push(render_notable_edge(ctx, edge, direction));
        }
    }
    selected
}

fn render_notable_edge(
    ctx: &ProviderContext<'_>,
    edge: &Edge,
    direction: &str,
) -> NotableEdge {
    let other = if direction == "out" {
        &edge.target
    } else {
        &edge.source
    };
    NotableEdge {
        kind: edge.kind.clone(),
        direction: direction.to_string(),
        target: other.to_string(),
        target_label: entity_loader::compact_label(ctx, other, None)
            .unwrap_or_else(|| other.to_string()),
    }
}

fn render_text(query: &str, seeds: &[SeedEntity]) -> String {
    let mut out = format!("Seed entities for: {query}\n\n");
    if seeds.is_empty() {
        out.push_str("No seeds found.\n");
        return out;
    }
    out.push_str("Search results are seeds, not proof. Inspect the best 1-3 hits before answering or traversing further.\n\n");
    for seed in seeds {
        out.push_str(&format!(
            "{}. {} — {:.5} ({})\n   {}\n",
            seeds
                .iter()
                .position(|candidate| candidate.entity_ref == seed.entity_ref)
                .map(|idx| idx + 1)
                .unwrap_or(1),
            seed.label,
            seed.score,
            seed.match_source,
            seed.entity_ref
        ));
        for edge in &seed.notable_edges {
            let arrow = if edge.direction == "out" { "-->" } else { "<--" };
            out.push_str(&format!(
                "   {arrow}[{}] {} ({})\n",
                edge.kind, edge.target, edge.target_label
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{EdgeConfidence, EdgeProvenance};

    fn edge(source: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: EntityRef::parse(source).unwrap(),
            kind: kind.into(),
            target: EntityRef::parse(target).unwrap(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        }
    }

    #[test]
    fn notable_edges_are_bounded_by_direction() {
        let source = "knowledge:a";
        let index = EdgeIndex::from_edges_for_tests(vec![
            edge(source, "SUPERSEDES", "knowledge:b"),
            edge(source, "DERIVED_FROM", "knowledge:c"),
            edge(source, "KNOWLEDGE_FROM_SESSION", "session:claude:s1"),
            edge("knowledge:d", "SUPERSEDES", source),
            edge("knowledge:e", "DERIVED_FROM", source),
            edge("session:claude:s2", "KNOWLEDGE_FROM_SESSION", source),
        ]);
        let ctx = ProviderContext::empty_for_tests();
        let edges = notable_edges(&ctx, &index, &EntityRef::parse(source).unwrap());
        assert!(edges.iter().filter(|edge| edge.direction == "out").count() <= 2);
        assert!(edges.iter().filter(|edge| edge.direction == "in").count() <= 2);
        assert!(!edges.is_empty());
    }

    #[test]
    fn empty_edge_index_keeps_seed_without_edges() {
        let result = serde_json::json!({
            "entity_id": "knowledge:a",
            "label": "A",
            "score": 0.5,
            "sources": {"bm25": 0.1},
        });
        let ctx = ProviderContext::empty_for_tests();
        let seed = seed_from_hybrid_result(&ctx, &EdgeIndex::default(), &result).unwrap();
        assert!(seed.notable_edges.is_empty());
        assert_eq!(seed.match_source, "bm25");
    }
}
