use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use chrono::Utc;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::embed::{Bucket, EmbeddingProvider, EmbeddingRouter};
use crate::embed_queue;
use crate::index::{HybridBm25Hit, TranscriptIndex};
use crate::knowledge::Knowledge;
use crate::search::rerank::{self, RerankFeatures};
use crate::search::rrf::{self, RankedHit, RankedList};
use crate::vectors::{self, PartitionMetrics};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
const DEFAULT_FETCH: usize = 50;
const RRF_K: f32 = 60.0;
const BM25_WEIGHT: f32 = 0.4;
const VECTOR_WEIGHT: f32 = 0.6;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HybridSearchParams {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub doc_type: Option<String>,
    #[serde(default)]
    pub include_vectors: Option<bool>,
    /// Optional deterministic query vector for fixtures and operator probes.
    /// When absent, bbox_hybrid_search embeds the query through each configured
    /// route and degrades per route if a provider is unavailable.
    #[serde(default)]
    pub query_vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize)]
struct HybridSearchResponse {
    text: String,
    results: Vec<HybridResult>,
    vector_status: HybridVectorStatus,
    degraded: HybridDegraded,
}

#[derive(Debug, Clone, Serialize)]
struct HybridResult {
    rank: usize,
    entity_id: String,
    score: f32,
    base_score: f32,
    label: String,
    doc_type: Option<String>,
    chunk_kind: Option<String>,
    role: Option<String>,
    sources: BTreeMap<String, f32>,
    excerpt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct HybridVectorStatus {
    queues: BTreeMap<String, crate::embed::queue::RouteStatus>,
    partitions: BTreeMap<String, PartitionMetrics>,
    searched_partitions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct HybridDegraded {
    vector_errors: BTreeMap<String, String>,
    skipped_partitions: BTreeMap<String, String>,
}

pub fn hybrid_search(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    p: &HybridSearchParams,
) -> Result<String> {
    let query = p.query.trim();
    if query.is_empty() {
        anyhow::bail!("query is required");
    }
    let limit = p
        .limit
        .unwrap_or(DEFAULT_LIMIT as u64)
        .min(MAX_LIMIT as u64) as usize;
    let fetch = DEFAULT_FETCH.max(limit * 4);
    let bm25_hits = index.hybrid_bm25_hits(query, fetch, p.doc_type.as_deref())?;
    let mut features = features_from_bm25(&bm25_hits);

    let mut lists = vec![RankedList {
        source: "bm25".into(),
        weight: BM25_WEIGHT,
        hits: bm25_hits
            .iter()
            .map(|hit| RankedHit {
                entity_id: hit.entity_id.clone(),
                rank: hit.rank,
                score: hit.score,
                source: "bm25".into(),
            })
            .collect(),
    }];

    let mut vector_status = HybridVectorStatus {
        queues: embed_queue::status_response().routes,
        partitions: vectors::metrics(),
        searched_partitions: Vec::new(),
    };
    let mut degraded = HybridDegraded::default();
    if p.include_vectors.unwrap_or(true) {
        let vector_lists = vector_ranked_lists(
            query,
            p.query_vector.as_deref(),
            fetch,
            &vector_status.partitions,
            &mut degraded,
        )?;
        vector_status.searched_partitions = vector_lists
            .iter()
            .map(|list| list.source.trim_start_matches("vector:").to_string())
            .collect();
        lists.extend(vector_lists);
    }

    let fused = rrf::fuse_rrf(&lists, RRF_K, fetch);
    enrich_fused_features(
        index,
        knowledge,
        fused.iter().map(|hit| hit.entity_id.as_str()),
        &mut features,
    )?;
    let now = Utc::now();
    let mut results = fused
        .into_iter()
        .map(|hit| {
            let feature = features.get(&hit.entity_id).cloned().unwrap_or_default();
            let score = rerank::apply_rerank(hit.score, &feature, now);
            let bm25 = bm25_hits
                .iter()
                .find(|bm25| bm25.entity_id == hit.entity_id);
            HybridResult {
                rank: 0,
                entity_id: hit.entity_id.clone(),
                score,
                base_score: hit.score,
                label: bm25
                    .and_then(|hit| hit.title.clone())
                    .unwrap_or_else(|| compact_entity_label(&hit.entity_id)),
                doc_type: feature.doc_type,
                chunk_kind: feature.chunk_kind,
                role: feature.role,
                sources: hit.sources,
                excerpt: bm25.map(|hit| hit.excerpt.clone()),
            }
        })
        .collect::<Vec<_>>();
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    results.truncate(limit);
    for (idx, result) in results.iter_mut().enumerate() {
        result.rank = idx + 1;
    }

    let text = render_text(query, &results, &vector_status, &degraded);
    Ok(serde_json::to_string_pretty(&HybridSearchResponse {
        text,
        results,
        vector_status,
        degraded,
    })?)
}

fn vector_ranked_lists(
    query: &str,
    supplied_query_vector: Option<&[f32]>,
    fetch: usize,
    partitions: &BTreeMap<String, PartitionMetrics>,
    degraded: &mut HybridDegraded,
) -> Result<Vec<RankedList>> {
    let router = EmbeddingRouter::load_default().unwrap_or_default();
    let mut route_buckets = BTreeMap::<String, BTreeSet<Bucket>>::new();
    for bucket in Bucket::ALL {
        match router.route(bucket, None) {
            Ok(route) => {
                route_buckets
                    .entry(route.vector_route_id())
                    .or_default()
                    .insert(bucket);
            }
            Err(err) => {
                degraded
                    .vector_errors
                    .insert(bucket.as_str().into(), sanitize_error(&err));
            }
        }
    }

    let mut lists = Vec::new();
    let mut embedded_queries = HashMap::<String, Vec<f32>>::new();
    for (route, metrics) in partitions {
        if metrics.active_count == 0 {
            continue;
        }
        let query_vector = if let Some(vector) = supplied_query_vector {
            if vector.len() != metrics.dims {
                degraded.skipped_partitions.insert(
                    route.clone(),
                    format!(
                        "query vector dims {} do not match partition dims {}",
                        vector.len(),
                        metrics.dims
                    ),
                );
                continue;
            }
            vector.to_vec()
        } else {
            let Some(buckets) = route_buckets.get(route) else {
                degraded.skipped_partitions.insert(
                    route.clone(),
                    "no configured bucket maps to this partition".into(),
                );
                continue;
            };
            let bucket = *buckets.iter().next().unwrap_or(&Bucket::Knowledge);
            let cache_key = format!("{route}:{}", bucket.as_str());
            if !embedded_queries.contains_key(&cache_key) {
                match embed_query(&router, bucket, query) {
                    Ok(vector) => {
                        embedded_queries.insert(cache_key.clone(), vector);
                    }
                    Err(err) => {
                        degraded
                            .vector_errors
                            .insert(route.clone(), sanitize_error(&err));
                        continue;
                    }
                }
            }
            embedded_queries
                .get(&cache_key)
                .cloned()
                .unwrap_or_default()
        };
        let hits = vectors::search(route, &query_vector, fetch)
            .with_context(|| format!("searching vector partition {route}"))?;
        lists.push(RankedList {
            source: format!("vector:{route}"),
            weight: VECTOR_WEIGHT,
            hits: hits
                .into_iter()
                .enumerate()
                .map(|(idx, hit)| RankedHit {
                    entity_id: hit.id,
                    rank: idx + 1,
                    score: 1.0 - hit.distance,
                    source: format!("vector:{route}"),
                })
                .collect(),
        });
    }
    Ok(lists)
}

fn embed_query(router: &EmbeddingRouter, bucket: Bucket, query: &str) -> Result<Vec<f32>> {
    let provider = router.route_for(bucket, None)?;
    embed_with_provider(provider, query)
}

fn embed_with_provider(provider: Box<dyn EmbeddingProvider>, query: &str) -> Result<Vec<f32>> {
    let texts = vec![query.to_string()];
    let vectors = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(provider.embed_batch(&texts))),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().context("creating embedding runtime")?;
            runtime.block_on(provider.embed_batch(&texts))
        }
    }?;
    vectors
        .into_iter()
        .next()
        .context("embedding provider returned no query vector")
}

fn features_from_bm25(hits: &[HybridBm25Hit]) -> BTreeMap<String, RerankFeatures> {
    hits.iter()
        .map(|hit| {
            (
                hit.entity_id.clone(),
                RerankFeatures {
                    doc_type: non_empty(&hit.doc_type),
                    chunk_kind: non_empty(&hit.chunk_kind),
                    role: non_empty(&hit.role),
                    ..RerankFeatures::default()
                },
            )
        })
        .collect()
}

fn enrich_knowledge_features(
    knowledge: &Knowledge,
    features: &mut BTreeMap<String, RerankFeatures>,
) {
    for (entity_id, feature) in features {
        let Some(id) = entity_id.strip_prefix("knowledge:") else {
            continue;
        };
        let Some(entry) = knowledge.entry(id) else {
            continue;
        };
        if feature.doc_type.is_none() {
            feature.doc_type = Some("knowledge".into());
        }
        feature.approval = Some(format!("{:?}", entry.approval));
        feature.created_at = Some(entry.created_at.clone());
        feature.last_recalled = entry.last_recalled.clone();
        feature.recall_count = entry.recall_count.min(u64::from(u32::MAX)) as u32;
    }
}

fn enrich_fused_features<'a>(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    entity_ids: impl Iterator<Item = &'a str>,
    features: &mut BTreeMap<String, RerankFeatures>,
) -> Result<()> {
    for entity_id in entity_ids {
        if !features.contains_key(entity_id) {
            let mut feature = index
                .entity_properties(entity_id)?
                .map(|properties| features_from_properties(&properties))
                .unwrap_or_default();
            if entity_id.starts_with("knowledge:") && feature.doc_type.is_none() {
                feature.doc_type = Some("knowledge".into());
            }
            features.insert(entity_id.to_string(), feature);
        }
    }
    enrich_knowledge_features(knowledge, features);
    Ok(())
}

fn features_from_properties(properties: &BTreeMap<String, String>) -> RerankFeatures {
    RerankFeatures {
        doc_type: properties.get("doc_type").cloned(),
        chunk_kind: properties.get("chunk_kind").cloned(),
        role: properties.get("role").cloned(),
        ..RerankFeatures::default()
    }
}

fn render_text(
    query: &str,
    results: &[HybridResult],
    vector_status: &HybridVectorStatus,
    degraded: &HybridDegraded,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Hybrid search: {query}\n\n"));
    if results.is_empty() {
        out.push_str("No results found.\n");
    } else {
        for result in results {
            out.push_str(&format!(
                "{}. {} — {:.5}\n   {}\n",
                result.rank, result.label, result.score, result.entity_id
            ));
            if let Some(excerpt) = &result.excerpt {
                out.push_str(&format!("   > {}\n", excerpt.replace('\n', " ")));
            }
        }
    }
    out.push_str(&format!(
        "\nVector status: {} queue route(s), {} partition(s), {} searched\n",
        vector_status.queues.len(),
        vector_status.partitions.len(),
        vector_status.searched_partitions.len()
    ));
    if !degraded.vector_errors.is_empty() || !degraded.skipped_partitions.is_empty() {
        out.push_str("Degraded vector routes:\n");
        for (route, err) in &degraded.vector_errors {
            out.push_str(&format!("  - {route}: {err}\n"));
        }
        for (route, err) in &degraded.skipped_partitions {
            out.push_str(&format!("  - {route}: {err}\n"));
        }
    }
    out
}

fn compact_entity_label(entity_id: &str) -> String {
    entity_id.chars().take(80).collect()
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn sanitize_error(err: &anyhow::Error) -> String {
    let mut value = err.to_string();
    if let Some((first, _)) = value.split_once('\n') {
        value = first.to_string();
    }
    if value.len() > 200 {
        value.truncate(197);
        value.push_str("...");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{
        Approval, Category, KnowledgeEntry, KnowledgeStore, Priority, Scope, Status,
    };
    use crate::search::rrf::{FusedHit, RankedList};

    #[test]
    fn response_shape_includes_vector_status() {
        let text = render_text(
            "fixture",
            &[HybridResult {
                rank: 1,
                entity_id: "knowledge:a".into(),
                score: 0.1,
                base_score: 0.1,
                label: "A".into(),
                doc_type: Some("knowledge".into()),
                chunk_kind: None,
                role: None,
                sources: BTreeMap::new(),
                excerpt: None,
            }],
            &HybridVectorStatus::default(),
            &HybridDegraded::default(),
        );
        assert!(text.contains("Hybrid search: fixture"));
        assert!(text.contains("Vector status"));
    }

    #[test]
    fn reranked_results_collapse_same_entity() {
        let fused = vec![FusedHit {
            entity_id: "knowledge:a".into(),
            score: 0.2,
            sources: BTreeMap::from([("bm25".into(), 0.1), ("vector:x".into(), 0.1)]),
        }];
        let lists = vec![RankedList {
            source: "bm25".into(),
            weight: 1.0,
            hits: vec![RankedHit {
                entity_id: "knowledge:a".into(),
                rank: 1,
                score: 1.0,
                source: "bm25".into(),
            }],
        }];
        assert_eq!(
            crate::search::rrf::fuse_rrf(&lists, 60.0, 10)[0].entity_id,
            fused[0].entity_id
        );
    }

    #[test]
    fn query_vector_dim_mismatch_degrades_partition() {
        let mut degraded = HybridDegraded::default();
        let partitions = BTreeMap::from([(
            "route-a".into(),
            PartitionMetrics {
                route: "route-a".into(),
                state: crate::vectors::PartitionState::Active { dims: 3 },
                dims: 3,
                wal_records: 1,
                active_count: 1,
                hnsw_rebuilds: 1,
                hnsw: None,
            },
        )]);
        let lists =
            vector_ranked_lists("q", Some(&[1.0, 0.0]), 5, &partitions, &mut degraded).unwrap();
        assert!(lists.is_empty());
        assert!(degraded.skipped_partitions["route-a"].contains("do not match"));
    }

    #[test]
    fn vector_only_knowledge_features_receive_approval_multiplier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.json");
        let store = KnowledgeStore {
            version: 1,
            entries: vec![KnowledgeEntry {
                id: "vector-only".into(),
                title: "Vector only".into(),
                content: "semantic-only content".into(),
                cluster: None,
                variants: HashMap::new(),
                category: Category::Memory,
                scope: Scope::Project,
                project: Some("/tmp/project".into()),
                providers: Vec::new(),
                priority: Priority::Standard,
                weight: 100,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                render: true,
                decay: true,
                review_at: None,
                supersedes: None,
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: "2026-05-05T00:00:00Z".into(),
                updated_at: "2026-05-05T00:00:00Z".into(),
                recall_count: 0,
                last_recalled: None,
            }],
        };
        std::fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();
        let knowledge = Knowledge::open(&path).unwrap();
        let mut features = BTreeMap::from([(
            "knowledge:vector-only".to_string(),
            RerankFeatures::default(),
        )]);

        enrich_knowledge_features(&knowledge, &mut features);

        let feature = &features["knowledge:vector-only"];
        assert_eq!(feature.doc_type.as_deref(), Some("knowledge"));
        assert_eq!(feature.approval.as_deref(), Some("UserConfirmed"));
        assert_eq!(rerank::type_multiplier(feature), 1.35);
    }
}
