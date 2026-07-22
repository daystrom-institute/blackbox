use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::mcp_tools::hybrid_search::{
    self, HybridDegraded, HybridResult, HybridSearchParams, HybridVectorStatus,
};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_indexing::index::TranscriptIndex;
use bbox_knowledge::knowledge::Knowledge;
use bbox_providers::entity_loader;
use bbox_providers::providers::{self, Neighborhood, ProviderContext};

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
    /// Restrict project_file results to a specific project (path or
    /// project_id). Identical semantics to `bbox_hybrid_search`'s `project`
    /// parameter — see that tool's docs.
    #[serde(default)]
    pub project: Option<String>,
    /// Knowledge visibility: published, own, or all. Defaults to own only
    /// when the MCP session has authoritative checkout context.
    #[serde(default)]
    pub provisional: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiscoverSeedResponse {
    status: &'static str,
    text: String,
    seeds: Vec<SeedEntity>,
    #[serde(skip_serializing_if = "HybridVectorStatus::is_empty")]
    vector_status: HybridVectorStatus,
    #[serde(skip_serializing_if = "HybridDegraded::is_empty")]
    degraded: HybridDegraded,
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
    active_selectors: &BTreeMap<String, String>,
    p: &DiscoverSeedParams,
) -> Result<String> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let hybrid = hybrid_search::hybrid_search_typed_with_active_selectors(
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
            project: p.project.clone(),
            provisional: p.provisional.clone(),
            rerank_cap: None,
            rerank: None,
        },
        active_selectors,
    )?;
    let seeds = hybrid
        .results
        .iter()
        .filter_map(|result| seed_from_hybrid_result(ctx, edge_index, result))
        .collect::<Vec<_>>();
    let text = render_text(&p.query, &seeds);
    Ok(serde_json::to_string_pretty(&DiscoverSeedResponse {
        status: "ok",
        text,
        seeds,
        vector_status: hybrid.vector_status,
        degraded: hybrid.degraded,
    })?)
}

fn seed_from_hybrid_result(
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    result: &HybridResult,
) -> Option<SeedEntity> {
    let entity_ref = result.entity_id.as_str();
    let parsed = EntityRef::parse(entity_ref).ok()?;
    let label = if result.label.trim().is_empty() {
        entity_loader::compact_label(ctx, &parsed, None).unwrap_or_else(|| entity_ref.to_string())
    } else {
        result.label.clone()
    };
    let match_source = match_source(&result.sources);
    Some(SeedEntity {
        entity_ref: entity_ref.to_string(),
        label,
        score: result.score,
        match_source,
        notable_edges: notable_edges(ctx, edge_index, &parsed),
    })
}

fn match_source(sources: &std::collections::BTreeMap<String, f32>) -> String {
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
    let forward = edge_index
        .forward_edges(entity_ref)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let reverse = edge_index
        .reverse_edges(entity_ref)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if forward.is_empty() && reverse.is_empty() {
        return Vec::new();
    }
    let neighborhood = Neighborhood { forward, reverse };
    let entity = entity_loader::load(ctx, entity_ref)
        .unwrap_or_else(|_| providers::empty_neighborhood_view(entity_ref, Default::default()));
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
    let mut seen = std::collections::HashSet::<(String, String)>::new();
    for family in priorities {
        for edge in edges.iter().filter(|edge| edge.kind == *family) {
            if selected.len() >= PER_DIRECTION_EDGE_LIMIT {
                return selected;
            }
            let key = (edge.kind.clone(), edge.target.to_string());
            if !seen.insert(key) {
                continue;
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
            if selected.iter().any(|selected| {
                selected.kind == edge.kind && selected.target == edge.target.to_string()
            }) {
                continue;
            }
            selected.push(render_notable_edge(ctx, edge, direction));
        }
    }
    selected
}

fn render_notable_edge(ctx: &ProviderContext<'_>, edge: &Edge, direction: &str) -> NotableEdge {
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
    for (idx, seed) in seeds.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {:.5} ({})\n",
            idx + 1,
            seed.label,
            seed.score,
            seed.match_source
        ));
        if seed.label == seed.entity_ref {
            out.push_str(&format!("   {}\n", seed.entity_ref));
        }
        for edge in &seed.notable_edges {
            let arrow = if edge.direction == "out" {
                "-->"
            } else {
                "<--"
            };
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
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};

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
        let result = HybridResult {
            rank: 1,
            entity_id: "knowledge:a".into(),
            score: 0.5,
            base_score: 0.5,
            label: "A".into(),
            doc_type: Some("knowledge".into()),
            chunk_kind: None,
            role: None,
            sources: std::collections::BTreeMap::from([("bm25".into(), 0.1)]),
            excerpt: None,
        };
        let ctx = ProviderContext::empty_for_tests();
        let seed = seed_from_hybrid_result(&ctx, &EdgeIndex::default(), &result).unwrap();
        assert!(seed.notable_edges.is_empty());
        assert_eq!(seed.match_source, "bm25");
    }

    #[test]
    fn render_text_hides_entity_ref_when_label_is_real() {
        let seed = SeedEntity {
            entity_ref: "project_file:abc:def:0".into(),
            label: "src/main.rs".into(),
            score: 0.5,
            match_source: "hybrid".into(),
            notable_edges: Vec::new(),
        };

        let text = render_text("main", &[seed]);

        assert!(text.contains("src/main.rs"));
        assert!(!text.contains("project_file:abc:def:0"));
    }

    #[test]
    fn render_text_keeps_entity_ref_for_fallback_label() {
        let seed = SeedEntity {
            entity_ref: "project_file:abc:def:0".into(),
            label: "project_file:abc:def:0".into(),
            score: 0.5,
            match_source: "hybrid".into(),
            notable_edges: Vec::new(),
        };

        let text = render_text("main", &[seed]);

        assert!(text.contains("project_file:abc:def:0"));
    }
}
