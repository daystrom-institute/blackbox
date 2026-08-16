use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::Utc;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::search::rerank::{self, RerankFeatures};
use bbox_corpus_core::search::rrf::{self, RankedHit, RankedList};
use bbox_embed::embed::rerank::{RerankConfig, RerankHit, rerank_blocking};
use bbox_embed::embed::{Bucket, EmbeddingRouter, VisualRouteMeta, query_cache};
use bbox_embed::embed_queue;
use bbox_indexing::index::{GRAPH_VERTEX_DOC_TYPE, HybridBm25Hit, TranscriptIndex};
use bbox_knowledge::knowledge::Knowledge;
use bbox_providers::entity_loader;
use bbox_providers::providers::{ProviderContext, ProviderProjectAuthority};
use bbox_vectors::{self as vectors, PartitionMetrics};

const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
const DEFAULT_FETCH: usize = 50;
const RRF_K: f32 = 60.0;
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
    /// Weight assigned to vector rank lists during RRF fusion.
    /// Defaults to 0.6; 0.0 is BM25-only, 1.0 is vector-only.
    #[serde(default)]
    pub vector_weight: Option<f32>,
    /// Optional deterministic query vector for fixtures and operator probes.
    /// When absent, bbox_hybrid_search embeds the query through each configured
    /// route and degrades per route if a provider is unavailable.
    #[serde(default)]
    pub query_vector: Option<Vec<f32>>,
    /// Restrict results to entities scoped to a specific project. Accepts
    /// an absolute project path (e.g. `/home/user/repos/my-app`), a
    /// project_id (8-hex), or a registered project alias (declared in the
    /// repo's `.bbox/config.toml` `[project] aliases`). When set, only project_file entries from that
    /// project and thread entries whose stored project resolves to that id
    /// are kept; commits, knowledge, transcripts, and other project-agnostic
    /// entity types pass through unfiltered. Use this to scope queries to
    /// your current repo when cross-project keyword pollution would otherwise
    /// dominate the top-N (a common case when multiple registered repos share
    /// vocabulary like "voyage" or "embed").
    #[serde(default)]
    pub project: Option<String>,
    /// Pre-resolved project filter id installed by the daemon boundary
    /// (phase-2 §9.2 B2): the shared engine resolves `project`, with the
    /// eight-hex pass-through and deterministic path-hash fallback surviving
    /// as version-1 compatibility lanes daemon-side. `serde(skip)` so wire
    /// callers cannot forge identity; `None` means no scoping.
    #[serde(skip)]
    pub resolved_project_id: Option<String>,
    /// Knowledge visibility policy: published, own, or all.
    #[serde(default)]
    pub provisional: Option<String>,
    /// Read-surface plane filter for graph vertex documents: `published`,
    /// `provisional`, or `connector`. Repeatable; unset means every plane.
    /// Composed into the word lane BEFORE ranking, so off-plane graph
    /// documents never consume rank positions. M9a indexes the published
    /// plane only; the other plane names parse but match no documents until
    /// their milestones land.
    #[serde(default)]
    pub graph_source: Option<Vec<String>>,
    /// Restrict graph vertex results to the named graphs within the resolved
    /// project. Unset means all graphs. Composed into the word lane BEFORE
    /// ranking together with `graph_source`.
    #[serde(default)]
    pub graph_ids: Option<Vec<String>>,
    /// Operator-probe override for the combined rerank multiplier cap
    /// (default 1.5, clamped to [1.0, 4.0]). Exists so eval sweeps can
    /// measure ranking quality per candidate cap (gap-39b3ce16 protocol in
    /// bbox_corpus_core::search::metrics); not intended for normal callers.
    #[serde(default)]
    pub rerank_cap: Option<f32>,
    /// Rerank stage selection. "model" (default) sends the fused top-k
    /// candidates to the configured cross-encoder (`[embed.rerank]`,
    /// default rerank-2.5-lite), orders by relevance, and applies the
    /// heuristic type/temporal multipliers after; on rerank API failure it
    /// falls back to heuristic and reports `degraded.rerank_unavailable`.
    /// "heuristic" skips the cross-encoder (multipliers only, no API call,
    /// lower latency). "none" returns raw fusion order.
    #[serde(default)]
    pub rerank: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HybridSearchResponse {
    pub text: String,
    /// Response breadcrumbs: the next tools in the locate-information funnel,
    /// carrying the actual top-seed refs. Mirrors inspect_entity's
    /// `recommended_next_hops` — injected at the decision point so the agent is
    /// pulled through discover → inspect → (paths) → bundle rather than
    /// recalling the opening sequence from memory.
    pub next_steps: Vec<String>,
    pub results: Vec<HybridResult>,
    /// Vector-lane health. Omitted entirely when no vector partitions were
    /// searched (the common BM25-only / vectors-disabled path), so a healthy
    /// response doesn't carry three empty collections.
    #[serde(skip_serializing_if = "HybridVectorStatus::is_empty")]
    pub vector_status: HybridVectorStatus,
    /// Per-route vector degradation. Omitted entirely when nothing degraded —
    /// the overwhelmingly common case — so green responses stay terse.
    #[serde(skip_serializing_if = "HybridDegraded::is_empty")]
    pub degraded: HybridDegraded,
}

#[derive(Debug, Clone, Serialize)]
pub struct HybridResult {
    pub rank: usize,
    pub entity_id: String,
    pub score: f32,
    pub base_score: f32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Structured project identity (governing section 10.2). Present on code
    /// results; callers no longer have to string-parse `entity_id` for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Normalized project-relative path. Never a host root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
    /// Stable machine identifier; unchanged when aliases or attachments change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Graph vertex identity (unified-retrieval design 6.1). Present only on
    /// `project_graph_vertex` documents; non-graph results are unchanged on
    /// the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_id: Option<String>,
    /// The authority plane this hit was indexed under: `published`,
    /// `provisional`, or `connector`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_source: Option<String>,
    /// Owning connector identity; present on connector-plane hits only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_source_connector: Option<String>,
    /// The schema type name of the vertex, e.g. `repo:Record`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_vertex_type: Option<String>,
    /// The generation identity content hash the document was indexed under;
    /// compare against `bbox_project_graph_describe`'s accepted generation to
    /// detect staleness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_generation: Option<String>,
    /// The pasteable `project_graph_vertex` form of the hit. Provisional hits
    /// carry a compound ref as `entity_id` whose scope segments make a poor
    /// handle; the logical form resolves through the read plane directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_logical_ref: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HybridVectorStatus {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub queues: BTreeMap<String, bbox_embed::embed::queue::RouteStatus>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub partitions: BTreeMap<String, PartitionMetrics>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub searched_partitions: Vec<String>,
}

impl HybridVectorStatus {
    /// True when no vector lane activity is worth surfacing — every sub-field
    /// empty. Gates whether the wrapper is serialized at all.
    pub fn is_empty(&self) -> bool {
        self.queues.is_empty() && self.partitions.is_empty() && self.searched_partitions.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HybridDegraded {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub vector_errors: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped_partitions: BTreeMap<String, String>,
    /// Set when rerank="model" was requested but the rerank API call
    /// failed; scoring fell back to the heuristic path for this response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_unavailable: Option<String>,
}

impl HybridDegraded {
    /// True when nothing degraded — no vector errors, no skipped
    /// partitions, and no rerank fallback.
    pub fn is_empty(&self) -> bool {
        self.vector_errors.is_empty()
            && self.skipped_partitions.is_empty()
            && self.rerank_unavailable.is_none()
    }
}

/// Hybrid search over an EXPLICITLY PINNED read view: the caller supplies
/// both the active-selector map and the searcher, and every downstream
/// consistency check - the lexical selector gate and the per-hit vector
/// filter [`retain_active_code_vectors`] - reads that pin instead of live
/// index state (Phase 3 plan section 4.5). The unpinned convenience
/// wrappers that used to sit here (`hybrid_search`,
/// `hybrid_search_with_active_selectors`, `hybrid_search_typed`,
/// `hybrid_search_typed_with_active_selectors`) are deliberately gone: each
/// minted a fresh searcher or read live selectors mid-call, so a commit
/// landing between the BM25 lane and the vector retain could filter vector
/// hits against a different index generation than the one that produced
/// them. Do not reintroduce them; take the pin from
/// `SharedState::code_read_view`.
pub fn hybrid_search_with_active_selectors_and_searcher(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    ctx: &ProviderContext<'_>,
    p: &HybridSearchParams,
    active_selectors: &BTreeMap<String, String>,
    searcher: &tantivy::Searcher,
    graph_policy: Option<&bbox_indexing::index::GraphWordPolicySnapshot>,
) -> Result<String> {
    Ok(serde_json::to_string_pretty(
        &hybrid_search_typed_with_active_selectors_and_searcher(
            index,
            knowledge,
            ctx,
            p,
            active_selectors,
            searcher,
            graph_policy,
        )?,
    )?)
}

pub fn hybrid_search_typed_with_active_selectors_and_searcher(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    ctx: &ProviderContext<'_>,
    p: &HybridSearchParams,
    active_selectors: &BTreeMap<String, String>,
    searcher: &tantivy::Searcher,
    graph_policy: Option<&bbox_indexing::index::GraphWordPolicySnapshot>,
) -> Result<HybridSearchResponse> {
    let query = p.query.trim();
    if query.is_empty() {
        anyhow::bail!("query is required");
    }
    // The word lane's graph authority (design 5.1): the caller supplies the
    // pinned policy snapshot (what the view catalog says is readable, taken
    // before the search started) and the parameters supply project scope,
    // plane selection, and named-graph selection. Everything composes into
    // the BM25 BooleanQuery BEFORE TopDocs.
    let graph_authority = bbox_indexing::index::GraphWordAuthority::from_parts(
        graph_policy,
        p.resolved_project_id.clone(),
        bbox_indexing::index::GraphWordAuthority::parse_graph_sources(
            p.graph_source.as_deref().unwrap_or_default(),
        )?,
        p.graph_ids
            .as_ref()
            .map(|graph_ids| graph_ids.iter().cloned().collect::<BTreeSet<_>>()),
    );
    let limit = p
        .limit
        .unwrap_or(DEFAULT_LIMIT as u64)
        .min(MAX_LIMIT as u64) as usize;
    // Widened to limit*8 so per-file collapse has enough depth to surface
    // `limit` distinct files even when the top-N is dominated by multiple
    // chunks of the same hot file.
    let fetch = DEFAULT_FETCH.max(limit * 8);
    // BM25 fetches deeper (limit*32) specifically to feed the file-level
    // aggregation pass. A high-mention file with chunks spread across many
    // sections (e.g. STATUS.md with 21 mentions across ~30 chunks) can
    // have every chunk individually rank below the chunk-level fetch
    // window, missing top-N entirely. Aggregating over a larger sample
    // lets the file's total query-term density surface even when no
    // single chunk is competitive.
    let bm25_fetch = (limit * 32).max(fetch);
    let (bm25_weight, vector_weight) = fusion_weights(p.vector_weight);
    let bm25_hits_full = index.hybrid_bm25_hits_with_graph_authority_and_searcher(
        query,
        bm25_fetch,
        p.doc_type.as_deref(),
        true,
        active_selectors,
        searcher,
        (!graph_authority.is_empty()).then_some(&graph_authority),
    )?;
    // Truncate the chunk-level list to `fetch` so it doesn't dilute RRF with
    // tail chunks that rank too low to matter. The full set still feeds
    // file-level aggregation below.
    let bm25_hits: Vec<_> = bm25_hits_full.iter().take(fetch).cloned().collect();
    let knowledge_hits = if p
        .doc_type
        .as_deref()
        .is_none_or(|doc_type| doc_type == "knowledge")
    {
        knowledge
            .search_hits(query, fetch)
            .into_iter()
            .enumerate()
            .map(|(rank, hit)| HybridBm25Hit {
                entity_id: hit.entity_id,
                score: hit.score,
                rank: rank + 1,
                doc_type: "knowledge".into(),
                chunk_kind: "knowledge_entry".into(),
                role: String::new(),
                title: Some(hit.title),
                excerpt: hit.excerpt,
                project_id: None,
                graph_id: None,
                graph_source: None,
                graph_source_connector: None,
                graph_vertex_type: None,
                graph_generation: None,
                logical_ref: None,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut ranked_hits = bm25_hits.clone();
    ranked_hits.extend(knowledge_hits.iter().cloned());
    let mut features = features_from_bm25(&ranked_hits);

    let mut lists = vec![RankedList {
        source: "bm25".into(),
        weight: bm25_weight,
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
    if !knowledge_hits.is_empty() {
        lists.push(RankedList {
            source: "knowledge".into(),
            weight: bm25_weight,
            hits: knowledge_hits
                .iter()
                .map(|hit| RankedHit {
                    entity_id: hit.entity_id.clone(),
                    rank: hit.rank,
                    score: hit.score,
                    source: "knowledge".into(),
                })
                .collect(),
        });
    }
    // File-level BM25 aggregation: sum chunk scores per (project_id,
    // rel_path_hash) over the FULL bm25 fetch (not the truncated chunk
    // list) and contribute the file-level ranking as a separate signal
    // into RRF. Lifts files with many sparse mentions across many chunks
    // (STATUS.md, ARCS.md, etc.) — without aggregation each chunk gets
    // a low individual score and the file falls off the top-N even when
    // its total query-term density is the highest in the corpus.
    let bm25_file_hits = aggregate_bm25_by_file(&bm25_hits_full);
    if !bm25_file_hits.is_empty() {
        lists.push(RankedList {
            source: "bm25_file".into(),
            weight: bm25_weight,
            hits: bm25_file_hits,
        });
    }

    let mut degraded = HybridDegraded::default();
    let mut vector_status = HybridVectorStatus::default();
    if vectors_requested(p.include_vectors, vector_weight) {
        // Search is a latency-sensitive hot path. Exact embedding coverage
        // walks the complete source corpus and belongs only on the explicit
        // bbox_embed_status surface. Queue-local status is constant-time, and
        // nonblocking metrics omit a partition currently held by compaction
        // instead of stalling the query behind its write lock.
        vector_status.queues = embed_queue::status_response().routes;
        vector_status.partitions = match vectors::metrics_nonblocking() {
            Some(partitions) => partitions,
            None => {
                degraded.skipped_partitions.insert(
                    "vector_store".into(),
                    "vector store is still warming; returning BM25-only results".into(),
                );
                BTreeMap::new()
            }
        };
        let mut vector_lists = vector_ranked_lists(
            query,
            p.query_vector.as_deref(),
            fetch,
            vector_weight,
            &vector_status.partitions,
            &mut degraded,
        )?;
        for list in &mut vector_lists {
            retain_authorized_knowledge_vectors(list, knowledge);
            retain_active_code_vectors(list, index, active_selectors, searcher);
            list.hits.truncate(fetch);
        }
        vector_status.searched_partitions = vector_lists
            .iter()
            .map(|list| list.source.trim_start_matches("vector:").to_string())
            .collect();
        lists.extend(vector_lists);
    }

    if let Some(doc_type) = p.doc_type.as_deref().filter(|value| !value.is_empty()) {
        scope_lists_to_doc_type(&mut lists, doc_type);
    }
    let fused = rrf::fuse_rrf(&lists, RRF_K, fetch);
    let mut loaded_properties = BTreeMap::new();
    enrich_fused_features(
        index,
        knowledge,
        fused.iter().map(|hit| hit.entity_id.as_str()),
        &mut features,
        &mut loaded_properties,
        searcher,
    )?;
    let now = Utc::now();
    let rerank_cap = p
        .rerank_cap
        .unwrap_or(rerank::DEFAULT_COMBINED_CAP)
        .clamp(1.0, 4.0);
    let rerank_mode = parse_rerank_mode(p.rerank.as_deref())?;
    let model_scores = if rerank_mode == RerankMode::Model {
        let config = EmbeddingRouter::load_default()
            .unwrap_or_default()
            .rerank_config();
        match model_rerank_scores(
            query,
            &fused,
            &ranked_hits,
            &loaded_properties,
            &config,
            |q, docs| rerank_blocking(config.clone(), q, docs),
        ) {
            Ok(scores) => Some(scores),
            Err(err) => {
                // Model stage degrades to the heuristic path — never fail
                // the whole search because a hosted reranker hiccuped.
                degraded.rerank_unavailable = Some(sanitize_error(&err));
                None
            }
        }
    } else {
        None
    };
    let mut results = fused
        .into_iter()
        .map(|hit| {
            let feature = features.get(&hit.entity_id).cloned().unwrap_or_default();
            let score = stage_score(
                rerank_mode,
                model_scores.as_ref(),
                &hit.entity_id,
                hit.score,
                &feature,
                now,
                rerank_cap,
            );
            let bm25 = ranked_hits
                .iter()
                .find(|bm25| bm25.entity_id == hit.entity_id);
            let properties = loaded_properties.get(&hit.entity_id);
            let stored = |key: &str| {
                properties
                    .and_then(|properties| properties.get(key))
                    .filter(|value| !value.is_empty())
                    .cloned()
            };
            let graph_logical_ref = (feature.doc_type.as_deref() == Some(GRAPH_VERTEX_DOC_TYPE))
                .then(|| {
                    stored("logical_ref").or_else(|| bm25.and_then(|hit| hit.logical_ref.clone()))
                })
                .flatten();
            HybridResult {
                rank: 0,
                entity_id: hit.entity_id.clone(),
                score,
                base_score: hit.score,
                label: label_for_entity(
                    ctx,
                    &hit.entity_id,
                    properties,
                    bm25.and_then(|hit| hit.title.as_deref()),
                ),
                doc_type: feature.doc_type,
                chunk_kind: feature.chunk_kind,
                role: feature.role,
                // Structured triple straight off the stored fields (P3-E item
                // 4): no entity-id string parsing, no path de-fabrication.
                // The BM25 hit is the fallback because pure word-lane hits
                // skip the provider property walk when their features were
                // already built from the lane.
                project_id: stored("project_id")
                    .or_else(|| bm25.and_then(|hit| hit.project_id.clone())),
                relative_path: stored("relative_path"),
                source_uri: stored("source_uri"),
                // Graph identity straight off the stored fields of the exact
                // document the hit was ranked from: no entity-id parsing, and
                // absent on every non-graph document so the wire stays
                // unchanged.
                graph_id: stored("graph_id").or_else(|| bm25.and_then(|hit| hit.graph_id.clone())),
                graph_source: stored("graph_source")
                    .or_else(|| bm25.and_then(|hit| hit.graph_source.clone())),
                graph_source_connector: stored("graph_source_connector")
                    .or_else(|| bm25.and_then(|hit| hit.graph_source_connector.clone())),
                graph_vertex_type: stored("graph_vertex_type")
                    .or_else(|| bm25.and_then(|hit| hit.graph_vertex_type.clone())),
                graph_generation: stored("graph_generation")
                    .or_else(|| bm25.and_then(|hit| hit.graph_generation.clone())),
                graph_logical_ref,
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
    // Project scoping: when the caller passed `project`, drop scoped entity
    // results from other projects so cross-project keyword pollution
    // (e.g. erlang-test/voyage.ex outranking transcript-search/voyage.rs
    // for "voyage" queries on the local repo) doesn't dominate top-N.
    // Project files encode their project id in the EntityRef; threads carry
    // the source project in their store record. Other entity types pass
    // through unfiltered — commits / knowledge / transcripts are project-
    // agnostic enough that the agent can decide relevance on its own.
    if let Some(target_project_id) = p.resolved_project_id.as_deref() {
        results.retain(|hit| keep_under_project_filter(&hit.entity_id, target_project_id, ctx));
    }
    if let Some(doc_type) = p.doc_type.as_deref().filter(|value| !value.is_empty()) {
        results.retain(|hit| hit.doc_type.as_deref() == Some(doc_type));
    }
    // Per-file collapse: keep only the best-scoring chunk per file. Without
    // this the top-N gets dominated by 3-5 chunks of the same .rs file when
    // the query matches multiple symbols in one file, starving the user of
    // breadth. Daystrom-mk2's AgenticTools used this pattern to lift recall
    // from 23% to 97% vs naive rerank. Keys off the (project_id,
    // rel_path_hash) prefix of project_file refs; commits / transcripts /
    // knowledge entities are not deduped (different files entirely).
    let mut seen_files = std::collections::HashSet::<String>::new();
    results.retain(|hit| {
        let Some(key) = file_dedup_key(&hit.entity_id) else {
            return true;
        };
        seen_files.insert(key)
    });
    // Modal diversification: when the top-`limit` would be entirely
    // doc_section (or entirely code_block, or entirely commits), pull the
    // highest-scoring entry of each missing kind from the rest of `results`
    // and substitute it for the lowest-scoring kept entry of the dominant
    // kind. Aim for at least 1 of each present (code_block, doc_section,
    // git_message) when the fetch set has them. Mirrors Daystrom-mk2
    // AgenticTools' diversity-by-type pass: a query like
    // "triad implementation" should surface BOTH the design markdown and
    // the .ex implementation file in top-N, not docs only.
    diversify_by_chunk_kind(&mut results, limit);
    results.truncate(limit);
    for (idx, result) in results.iter_mut().enumerate() {
        result.rank = idx + 1;
    }

    let next_steps = build_next_steps(&results);
    let text = render_text(query, &results, &next_steps, &vector_status, &degraded);
    Ok(HybridSearchResponse {
        text,
        next_steps,
        results,
        vector_status,
        degraded,
    })
}

fn retain_authorized_knowledge_vectors(list: &mut RankedList, knowledge: &Knowledge) {
    list.hits
        .retain(|hit| match EntityRef::parse(&hit.entity_id) {
            Ok(EntityRef::Knowledge { id }) => knowledge.entry(&id).is_some(),
            Ok(EntityRef::ProvisionalKnowledge { .. }) => knowledge.entry(&hit.entity_id).is_some(),
            Err(_)
                if hit.entity_id.starts_with("knowledge:")
                    || hit.entity_id.starts_with("provisional_knowledge:") =>
            {
                false
            }
            _ => true,
        });
}

fn retain_active_code_vectors(
    list: &mut RankedList,
    index: &TranscriptIndex,
    active_selectors: &BTreeMap<String, String>,
    searcher: &tantivy::Searcher,
) {
    list.hits.retain(|hit| {
        index.is_active_code_entity_for_with_searcher(&hit.entity_id, active_selectors, searcher)
    });
}

/// Pre-fusion doc_type scoping: the BM25 lane is already filtered at the
/// tantivy query, but the vector lanes are not, so without this the fused
/// pool (capped at `fetch`) fills with off-type vector hits and the
/// post-fusion doc_type retain empties small-limit results entirely
/// (observed live: doc_type=transcript at limit 3 returned 0 while 50+
/// filtered BM25 matches existed; limit 20 returned 20). doc_type equals
/// the entity-ref type prefix for every current doc_type value, so a
/// prefix retain per lane is exact; original per-lane ranks are kept (RRF
/// handles sparse ranks fine). The post-fusion doc_type retain stays as
/// the authoritative backstop.
///
/// Visual chunks (image/pdf_figure) need no special case here: they are
/// `project_file` entities like Code/Docs chunks — chunk_kind, not
/// doc_type, is what distinguishes them — so `doc_type="project_file"`
/// already includes visual vector-lane hits by prefix match, and any other
/// doc_type value correctly excludes them. Filtering visual results down to
/// "only images" is a chunk_kind concern the caller can apply post-hoc, not
/// a doc_type one.
fn scope_lists_to_doc_type(lists: &mut [RankedList], doc_type: &str) {
    let prefix = format!("{doc_type}:");
    for list in lists {
        list.hits.retain(|hit| {
            hit.entity_id.starts_with(&prefix)
                || (doc_type == "project_file" && hit.entity_id.starts_with("project_file_v2:"))
        });
    }
}

/// Build response breadcrumbs naming the top-seed ref(s) and the next tools in
/// the funnel. Carries the exact refs so the agent can paste them onward
/// (Daystrom's "Type:Id for direct use in other tools" principle).
fn build_next_steps(results: &[HybridResult]) -> Vec<String> {
    if results.is_empty() {
        return vec![
            "No seeds. Broaden the query, drop the doc_type filter, or raise vector_weight toward 1.0 for paraphrase recall.".to_string(),
        ];
    }
    let top = &results[0].entity_id;
    let bundle_refs = results
        .iter()
        .take(3)
        .map(|r| format!("\"{}\"", r.entity_id))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "Confirm the top seed: bbox_inspect_entity(entity_ref=\"{top}\") — properties + targeted edges in one call."
        ),
        format!(
            "Multi-hop question? bbox_find_paths(from=\"{top}\", to_type=<target>), then pass the path_ids onward."
        ),
        format!(
            "Package the answer: bbox_bundle_evidence(question=<q>, entity_refs=[{bundle_refs}])."
        ),
    ]
}

/// Aggregates per-chunk BM25 scores up to per-file scores and returns a
/// ranked list keyed on the highest-scoring chunk per file. Chunks of
/// non-project_file entities (commits, transcripts, knowledge) pass through
/// individually so they're not double-counted in the file aggregation.
fn aggregate_bm25_by_file(chunks: &[bbox_indexing::index::HybridBm25Hit]) -> Vec<RankedHit> {
    use std::collections::HashMap;
    // Group project_file chunks by (project_id, rel_path_hash). Track
    // sum-of-scores AND count, then rank by `sum * sqrt(count)` so a file
    // with many matching chunks (high topical coverage even when each
    // chunk's individual TF-IDF is mediocre, e.g. STATUS.md sprawled
    // across many sections) ranks above a file with fewer but slightly
    // denser chunks. Score sum alone underweights breadth; sqrt(count)
    // alone overweights it. The geometric blend lifts coverage cleanly.
    let mut by_file: HashMap<String, (f32, usize, &bbox_indexing::index::HybridBm25Hit)> =
        HashMap::new();
    let mut non_file_hits: Vec<&bbox_indexing::index::HybridBm25Hit> = Vec::new();
    for hit in chunks {
        let Some(key) = file_dedup_key(&hit.entity_id) else {
            non_file_hits.push(hit);
            continue;
        };
        by_file
            .entry(key)
            .and_modify(|(score, count, repr)| {
                *score += hit.score;
                *count += 1;
                if hit.score > repr.score {
                    *repr = hit;
                }
            })
            .or_insert((hit.score, 1, hit));
    }
    if by_file.len() <= 1 {
        // Aggregation only contributes signal when multiple files appear in
        // the chunk-level results. With one or zero, there's nothing to
        // re-rank — return empty so the existing chunk-level RRF pass owns
        // the ranking.
        return Vec::new();
    }
    let mut aggregated: Vec<(String, f32, &bbox_indexing::index::HybridBm25Hit)> = by_file
        .into_iter()
        .map(|(_, (score, count, repr))| {
            let combined = score * (count as f32).sqrt();
            (repr.entity_id.clone(), combined, repr)
        })
        .collect();
    aggregated.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    aggregated
        .into_iter()
        .enumerate()
        .map(|(idx, (entity_id, score, _repr))| RankedHit {
            entity_id,
            rank: idx + 1,
            score,
            source: "bm25_file".into(),
        })
        .collect()
}

/// Decides whether a search hit survives the project filter. Project-file
/// refs must match the target project_id; thread refs must resolve through
/// their stored `project`; other entity types pass through (commits/knowledge/
/// transcripts/sessions/etc are project-agnostic enough that the agent can
/// prune them itself if needed).
fn keep_under_project_filter(
    entity_id: &str,
    target_project_id: &str,
    ctx: &ProviderContext<'_>,
) -> bool {
    let mut parts = entity_id.split(':');
    match parts.next() {
        Some("project_file" | "project_file_v2") => parts.next() == Some(target_project_id),
        // Graph vertices are project-scoped: segment 1 of the logical ref is
        // the project id (design 5.1). The provisional ref form carries scope
        // and checkout segments instead, so it passes here and relies on the
        // pre-ranking authority clause, which filters on the project_id field
        // stamped into the document at index time (Q6 ruling).
        Some("project_graph_vertex") => parts.next() == Some(target_project_id),
        Some("thread") => thread_matches_project_filter(parts.next(), target_project_id, ctx),
        _ => true,
    }
}

fn thread_matches_project_filter(
    thread_id: Option<&str>,
    target_project_id: &str,
    ctx: &ProviderContext<'_>,
) -> bool {
    let (Some(thread_id), Some(stores)) = (thread_id, ctx.stores()) else {
        return true;
    };
    let threads = stores.threads.read();
    let Some(thread) = threads.all().iter().find(|thread| thread.id == thread_id) else {
        return true;
    };
    thread_project_matches(stores.project_authority, &thread.project, target_project_id)
}

/// Whether a thread's stored `project` denotes `target_project_id`.
///
/// A thread stores a HOST PATH in `project`. Deriving identity from that path
/// is a version-1 lane: in catalog mode identity comes from the catalog store,
/// never from a path hash (Phase 6 plan section 5.1).
///
/// Both arms keep the shipped failure semantics: a stored path that does not
/// resolve to an identity EXCLUDES the thread, exactly as the path-hash lane
/// already did for a path that no longer canonicalizes.
fn thread_project_matches(
    authority: ProviderProjectAuthority<'_>,
    thread_project: &str,
    target_project_id: &str,
) -> bool {
    match authority {
        ProviderProjectAuthority::Catalog { catalog } => {
            let Ok(state) = catalog.snapshot() else {
                return false;
            };
            let engine = bbox_indexing::project_resolver::ProjectResolverEngine::v2(
                state.catalog(),
                state.attachments(),
            );
            let request = bbox_corpus_core::project_selector::ProjectSelectorRequest::filter(
                thread_project.to_owned(),
            );
            engine
                .resolve(&request)
                .ok()
                .and_then(|resolution| resolution.project_id().map(str::to_owned))
                .as_deref()
                == Some(target_project_id)
        }
        // Version-1 bridge lane, retained through Phase 6 by FD-8.
        ProviderProjectAuthority::Bridge => {
            bbox_corpus_core::entity_ref::project_id_for_path(thread_project)
                .ok()
                .as_deref()
                == Some(target_project_id)
        }
    }
}

/// Returns a per-file dedup key when `entity_id` refers to a project_file
/// chunk: `project_file:<proj>:<rel_path_hash>` — i.e. the file path identity
/// minus chunk_hash + occurrence_idx. Returns `None` for any other entity
/// type so commits / transcripts / knowledge entries are passed through
/// without being collapsed against each other.
fn file_dedup_key(entity_id: &str) -> Option<String> {
    let mut parts = entity_id.split(':');
    match parts.next()? {
        "project_file" => {
            let project = parts.next()?;
            let relative_path_hash = parts.next()?;
            Some(format!("project_file:{project}:{relative_path_hash}"))
        }
        "project_file_v2" => {
            let project = parts.next()?;
            let snapshot = parts.next()?;
            let relative_path_hash = parts.next()?;
            Some(format!(
                "project_file_v2:{project}:{snapshot}:{relative_path_hash}"
            ))
        }
        // Graph vertex documents are excluded from file dedup and file
        // aggregation EXPLICITLY (design 4.1), not by falling through the
        // catch-all: a graph vertex has no file, and a key that fell back to
        // the entity id would pollute the aggregate lane with singleton
        // groups. Keeping the arm visible also documents that the source-path
        // pseudo-path stamped for provenance must never become a dedup key.
        "project_graph_vertex" | "provisional_project_graph_vertex" => None,
        _ => None,
    }
}

/// Promotes the highest-scoring entry of each chunk_kind into the top-`limit`
/// window so the surface returned to the caller covers code + docs + commits
/// when the fetch set has them. Approach: keep results sorted by score, walk
/// from the back of the kept window swapping the lowest-ranked entry of the
/// dominant kind out for the highest-ranked entry of an absent kind sitting
/// just below the cutoff. Conservative — never displaces an entry of a kind
/// that's already underrepresented.
fn diversify_by_chunk_kind(results: &mut [HybridResult], limit: usize) {
    if results.len() <= limit {
        return;
    }
    // Kinds we deliberately balance. `None` chunk_kind is left alone (it's
    // mostly transcripts and synthetic entities — pure-vector fallback hits
    // that don't fit into the modal taxonomy).
    const TARGET_KINDS: &[&str] = &["code_block", "doc_section", "git_message"];
    for &target in TARGET_KINDS {
        let already_in_top = results[..limit]
            .iter()
            .any(|r| r.chunk_kind.as_deref() == Some(target));
        if already_in_top {
            continue;
        }
        // Find the best below-cutoff entry of the missing kind.
        let Some(promote_idx) = results[limit..]
            .iter()
            .position(|r| r.chunk_kind.as_deref() == Some(target))
            .map(|i| limit + i)
        else {
            continue; // not present in fetch set; skip
        };
        // Find the displaceable kept entry: the lowest-ranked one whose
        // kind is over-represented (i.e. not the target and not the only
        // representative of its kind in the kept window).
        let mut kind_counts = std::collections::HashMap::<Option<&str>, usize>::new();
        for r in &results[..limit] {
            *kind_counts.entry(r.chunk_kind.as_deref()).or_default() += 1;
        }
        let displaceable = (0..limit).rev().find(|&i| {
            let kind = results[i].chunk_kind.as_deref();
            kind != Some(target) && kind_counts.get(&kind).copied().unwrap_or(0) > 1
        });
        if let Some(displace_idx) = displaceable {
            results.swap(displace_idx, promote_idx);
        }
    }
}

fn vectors_requested(include_vectors: Option<bool>, vector_weight: f32) -> bool {
    include_vectors.unwrap_or(true) && vector_weight > 0.0
}

fn vector_ranked_lists(
    query: &str,
    supplied_query_vector: Option<&[f32]>,
    fetch: usize,
    vector_weight: f32,
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
    // Visual routes (`[embed.routes.visual]`) are chunk-kind-keyed, never
    // `Bucket`-keyed (see `EmbeddingRouter::visual_route`), so they never
    // land in `route_buckets` above. Without this second map, a visual
    // partition present in `partitions` (vectors already indexed) fell into
    // the "no configured bucket maps to this partition" skip branch below —
    // the shipped sidecar embedded content but retrieval never searched it.
    // `configured_visual_routes()` dedupes by partition id: `image` and
    // `pdf_figure` sharing one multimodal alias search once, not twice.
    // Empty when no `[embed.routes.visual]` kind is configured, so an
    // unconfigured host falls through to the exact pre-existing skip branch
    // — zero behavior change, zero extra calls.
    let visual_routes: BTreeMap<String, (String, VisualRouteMeta)> = router
        .configured_visual_routes()
        .into_iter()
        .map(|(route_id, kind, meta)| (route_id, (kind, meta)))
        .collect();

    let mut lists = Vec::new();
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
        } else if let Some(buckets) = route_buckets.get(route) {
            let bucket = *buckets.iter().next().unwrap_or(&Bucket::Knowledge);
            match query_cache::embed_query_cached(&router, bucket, None, query) {
                Ok(vector) => vector,
                Err(err) => {
                    degraded
                        .vector_errors
                        .insert(route.clone(), sanitize_error(&err));
                    continue;
                }
            }
        } else if let Some((kind, meta)) = visual_routes.get(route) {
            // Embeds the query once via the multimodal alias, through the
            // same process-wide query cache the text lanes use — a repeat
            // query does not re-bill. A failure here degrades only this
            // lane (never the whole search): API-down / rate-limited /
            // credential-missing all land in `degraded.vector_errors`.
            match query_cache::embed_query_cached_visual(&router, meta, kind, query) {
                Ok(vector) => vector,
                Err(err) => {
                    degraded
                        .vector_errors
                        .insert(route.clone(), sanitize_error(&err));
                    continue;
                }
            }
        } else {
            degraded.skipped_partitions.insert(
                route.clone(),
                "no configured bucket maps to this partition".into(),
            );
            continue;
        };
        let candidate_fetch = fetch.saturating_mul(8).min(10_000);
        let hits = vectors::search(route, &query_vector, candidate_fetch)
            .with_context(|| format!("searching vector partition {route}"))?;
        lists.push(RankedList {
            source: format!("vector:{route}"),
            weight: vector_weight,
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

fn fusion_weights(vector_weight: Option<f32>) -> (f32, f32) {
    let vector_weight = vector_weight.unwrap_or(VECTOR_WEIGHT).clamp(0.0, 1.0);
    (1.0 - vector_weight, vector_weight)
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
        let Some(entry) = knowledge_entry_for_entity(knowledge, entity_id) else {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankMode {
    Heuristic,
    Model,
    None,
}

fn parse_rerank_mode(raw: Option<&str>) -> Result<RerankMode> {
    // Model rerank is the DEFAULT since 2026-07-11: the eval A/B measured
    // MRR 0.1667 vs heuristic 0.1067 (recall@1 2.5x) and the operator
    // accepted the added per-search cross-encoder latency. API failure
    // still degrades to the heuristic path per call, so a missing key or
    // provider outage costs the boost, never the search.
    match raw.map(str::trim).unwrap_or("model") {
        "" | "model" => Ok(RerankMode::Model),
        "heuristic" => Ok(RerankMode::Heuristic),
        "none" => Ok(RerankMode::None),
        other => anyhow::bail!("unknown rerank mode `{other}`; expected model, heuristic, or none"),
    }
}

/// Send the fused top-k to the cross-encoder and map its relevance scores
/// back to entity ids. The rerank call is injected so tests exercise the
/// candidate/document assembly and score mapping without a live provider.
fn model_rerank_scores(
    query: &str,
    fused: &[rrf::FusedHit],
    bm25_hits: &[HybridBm25Hit],
    loaded_properties: &BTreeMap<String, BTreeMap<String, String>>,
    config: &RerankConfig,
    rerank_call: impl FnOnce(&str, &[String]) -> Result<Vec<RerankHit>>,
) -> Result<BTreeMap<String, f32>> {
    let candidates = &fused[..config.top_k.clamp(1, 1_000).min(fused.len())];
    if candidates.is_empty() {
        return Ok(BTreeMap::new());
    }
    let documents = candidates
        .iter()
        .map(|hit| {
            candidate_document(
                &hit.entity_id,
                bm25_hits
                    .iter()
                    .find(|bm25| bm25.entity_id == hit.entity_id),
                loaded_properties.get(&hit.entity_id),
            )
        })
        .collect::<Vec<_>>();
    let hits = rerank_call(query, &documents)?;
    Ok(hits
        .into_iter()
        .map(|hit| (candidates[hit.index].entity_id.clone(), hit.relevance_score))
        .collect())
}

/// Final per-candidate score for one rerank mode. Model-reranked
/// candidates score in a strictly higher band (base 1.0 + relevance vs
/// RRF's <=~0.05) so the cross-encoder ordering owns the top-k and the
/// unsent tail keeps its fusion order below; heuristic multipliers apply
/// after, under the same cap machinery (multiplier bounds — type >= 0.85,
/// temporal clamped to [0.50, 1.25] — keep the bands from crossing).
#[allow(clippy::too_many_arguments)]
fn stage_score(
    mode: RerankMode,
    model_scores: Option<&BTreeMap<String, f32>>,
    entity_id: &str,
    fused_score: f32,
    feature: &RerankFeatures,
    now: chrono::DateTime<Utc>,
    cap: f32,
) -> f32 {
    match (mode, model_scores) {
        (RerankMode::None, _) => fused_score,
        (RerankMode::Model, Some(scores)) => match scores.get(entity_id) {
            Some(relevance) => rerank::apply_rerank_with_cap(1.0 + relevance, feature, now, cap),
            None => rerank::apply_rerank_with_cap(fused_score, feature, now, cap),
        },
        // Heuristic, or model requested but degraded.
        _ => rerank::apply_rerank_with_cap(fused_score, feature, now, cap),
    }
}

/// Best text available for one rerank candidate without extra index round
/// trips: title/path identity, the stored content preview, and the BM25
/// excerpt when the BM25 lane surfaced this entity. Vector-only hits are
/// thinner (preview only) — acceptable for a cross-encoder, and the eval
/// suite is the judge of whether richer loading is worth it.
fn candidate_document(
    entity_id: &str,
    bm25: Option<&HybridBm25Hit>,
    properties: Option<&BTreeMap<String, String>>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(properties) = properties {
        // P3-E: the reranker sees the relative path, not a host absolute path.
        // Feeding host-root components to a cross-encoder both leaks them into
        // the rerank input and lets an unrelated query token match a machine's
        // directory names. `relative_path` and `file_path` carry the same value
        // after the bump, so this takes whichever is present, once.
        for key in ["title"] {
            if let Some(value) = properties.get(key) {
                parts.push(value.clone());
            }
        }
        if let Some(value) = properties
            .get("relative_path")
            .or_else(|| properties.get("file_path"))
        {
            parts.push(value.clone());
        }
        if let Some(value) = properties.get("symbol") {
            parts.push(value.clone());
        }
        for key in ["content_preview", "content"] {
            if let Some(value) = properties.get(key) {
                parts.push(value.clone());
                break;
            }
        }
    }
    if let Some(bm25) = bm25 {
        if let Some(title) = &bm25.title {
            if !parts.iter().any(|part| part == title) {
                parts.push(title.clone());
            }
        }
        if !bm25.excerpt.is_empty() {
            parts.push(bm25.excerpt.replace("**", ""));
        }
    }
    if parts.is_empty() {
        parts.push(entity_id.to_string());
    }
    let mut document = parts.join("\n");
    // Stay far inside the 32K-combined-token pair cap even at k=100.
    document.truncate(
        document
            .char_indices()
            .nth(2_000)
            .map(|(idx, _)| idx)
            .unwrap_or(document.len()),
    );
    document
}

fn enrich_fused_features<'a>(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    entity_ids: impl Iterator<Item = &'a str>,
    features: &mut BTreeMap<String, RerankFeatures>,
    loaded_properties: &mut BTreeMap<String, BTreeMap<String, String>>,
    searcher: &tantivy::Searcher,
) -> Result<()> {
    for entity_id in entity_ids {
        if !features.contains_key(entity_id) {
            let indexed_properties = index.entity_properties_with_searcher(entity_id, searcher)?;
            let mut feature = indexed_properties
                .as_ref()
                .map(features_from_properties)
                .unwrap_or_default();
            if let Some(properties) = indexed_properties {
                loaded_properties.insert(entity_id.to_string(), properties);
            }
            if (entity_id.starts_with("knowledge:")
                || entity_id.starts_with("provisional_knowledge:"))
                && feature.doc_type.is_none()
            {
                feature.doc_type = Some("knowledge".into());
            }
            if feature.doc_type.is_none() {
                if let Ok(entity_ref) = EntityRef::parse(entity_id) {
                    feature.doc_type = Some(entity_ref.entity_type().as_str().into());
                }
            }
            features.insert(entity_id.to_string(), feature);
        }
        if let Some(properties) = knowledge_properties(knowledge, entity_id) {
            loaded_properties
                .entry(entity_id.to_string())
                .or_default()
                .extend(properties);
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

fn knowledge_properties(
    knowledge: &Knowledge,
    entity_id: &str,
) -> Option<BTreeMap<String, String>> {
    let entry = knowledge_entry_for_entity(knowledge, entity_id)?;
    let mut properties = BTreeMap::new();
    properties.insert("title".into(), entry.title.clone());
    properties.insert("doc_type".into(), "knowledge".into());
    properties.insert("approval".into(), format!("{:?}", entry.approval));
    properties.insert("created_at".into(), entry.created_at.clone());
    if let Some(last_recalled) = &entry.last_recalled {
        properties.insert("last_recalled".into(), last_recalled.clone());
    }
    Some(properties)
}

fn knowledge_entry_for_entity<'a>(
    knowledge: &'a Knowledge,
    entity_id: &str,
) -> Option<&'a bbox_knowledge::knowledge::KnowledgeEntry> {
    if let Some(id) = entity_id.strip_prefix("knowledge:") {
        knowledge.entry(id)
    } else if entity_id.starts_with("provisional_knowledge:") {
        knowledge.entry(entity_id)
    } else {
        None
    }
}

fn label_for_entity(
    ctx: &ProviderContext<'_>,
    entity_id: &str,
    loaded: Option<&BTreeMap<String, String>>,
    bm25_title: Option<&str>,
) -> String {
    EntityRef::parse(entity_id)
        .ok()
        .and_then(|r| {
            // A graph vertex's label is the first line of its indexed
            // content (design 4.1) and the BM25 lane already materializes it
            // as the hit title. Preferring it keeps label resolution off the
            // provider registry for a hit whose identity came from the index,
            // and keeps the label on the generation the hit was ranked from.
            if matches!(
                r,
                EntityRef::ProjectGraphVertex { .. }
                    | EntityRef::ProvisionalProjectGraphVertex { .. }
            ) {
                return bm25_title.map(str::to_string);
            }
            entity_loader::compact_label(ctx, &r, loaded)
        })
        .or_else(|| bm25_title.map(str::to_string))
        .unwrap_or_else(|| compact_entity_label(entity_id))
}

fn render_text(
    query: &str,
    results: &[HybridResult],
    next_steps: &[String],
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
    if !next_steps.is_empty() {
        out.push_str("\nNext steps:\n");
        for step in next_steps {
            out.push_str(&format!("  → {step}\n"));
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
    let unhealthy_routes = vector_status
        .queues
        .iter()
        .filter(|(_, status)| status.health != "ok")
        .collect::<Vec<_>>();
    if !unhealthy_routes.is_empty() {
        out.push_str("Vector route health:\n");
        for (route, status) in unhealthy_routes {
            let reason = status.health_reason.as_deref().unwrap_or("unavailable");
            out.push_str(&format!(
                "  - {route}: {} ({reason}; queue_depth={}, indexed_count={})\n",
                status.health, status.queue_depth, status.indexed_count
            ));
            if let Some(err) = status.last_error.as_deref() {
                out.push_str(&format!("    last_error: {}\n", sanitize_status_error(err)));
            }
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

fn sanitize_status_error(err: &str) -> String {
    let mut value = err.to_string();
    if let Some((first, _)) = value.split_once('\n') {
        value = first.to_string();
    }
    if value.len() > 160 {
        value.truncate(157);
        value.push_str("...");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use bbox_corpus_core::search::rrf::{FusedHit, RankedList};
    use bbox_knowledge::knowledge::{
        Approval, Category, KnowledgeEntry, KnowledgeStore, Priority, Scope, Status,
    };

    #[test]
    fn rerank_mode_parses_and_rejects_unknown() {
        assert_eq!(parse_rerank_mode(None).unwrap(), RerankMode::Model);
        assert_eq!(
            parse_rerank_mode(Some("heuristic")).unwrap(),
            RerankMode::Heuristic
        );
        assert_eq!(parse_rerank_mode(Some("model")).unwrap(), RerankMode::Model);
        assert_eq!(parse_rerank_mode(Some("none")).unwrap(), RerankMode::None);
        assert!(parse_rerank_mode(Some("llm")).is_err());
    }

    fn fused_hit(entity_id: &str, score: f32) -> FusedHit {
        FusedHit {
            entity_id: entity_id.into(),
            score,
            sources: BTreeMap::new(),
        }
    }

    fn visible_knowledge_entry(id: &str) -> KnowledgeEntry {
        KnowledgeEntry {
            id: id.into(),
            title: "visible".into(),
            content: "visible content".into(),
            cluster: None,
            variants: HashMap::new(),
            category: Category::Memory,
            scope: Scope::Project,
            project: Some("/tmp/project".into()),
            project_id: None,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            status: Status::Active,
            approval: Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-07-21T00:00:00Z".into(),
            updated_at: "2026-07-21T00:00:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        }
    }

    #[test]
    fn knowledge_vectors_are_kept_only_for_exactly_visible_entities() {
        let scope = bbox_corpus_core::identity::PublishedScope::try_new("repo", ".").unwrap();
        let provisional_ref =
            bbox_knowledge::overlay::provisional_entity_ref(&scope, "checkout", "changed");
        let mut provisional = visible_knowledge_entry("changed");
        provisional.id = provisional_ref.clone();
        let knowledge = Knowledge::detached_view(
            vec![visible_knowledge_entry("published"), provisional],
            BTreeMap::new(),
        );
        let hit = |entity_id: &str, rank| RankedHit {
            entity_id: entity_id.into(),
            rank,
            score: 1.0,
            source: "vector:test".into(),
        };
        let mut list = RankedList {
            source: "vector:test".into(),
            weight: 0.6,
            hits: vec![
                hit("knowledge:published", 1),
                hit("knowledge:hidden", 2),
                hit(&provisional_ref, 3),
                hit("project_file:p:f:h:1", 4),
            ],
        };

        retain_authorized_knowledge_vectors(&mut list, &knowledge);

        let ids = list
            .hits
            .iter()
            .map(|hit| hit.entity_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "knowledge:published",
                provisional_ref.as_str(),
                "project_file:p:f:h:1"
            ]
        );
    }

    #[test]
    fn doc_type_scoping_drops_off_type_lane_hits_before_fusion() {
        let ranked = |entity_id: &str, rank: usize| bbox_corpus_core::search::rrf::RankedHit {
            entity_id: entity_id.into(),
            rank,
            score: 1.0,
            source: "test".into(),
        };
        let mut lists = vec![
            RankedList {
                source: "bm25".into(),
                weight: 0.4,
                hits: vec![
                    ranked("transcript:claude:sess-1:100:0", 1),
                    ranked("transcript:claude:sess-1:200:0", 2),
                ],
            },
            RankedList {
                source: "vector:voyage-1024".into(),
                weight: 0.6,
                hits: vec![
                    ranked("project_file:p:f:h:1", 1),
                    ranked("knowledge:abc", 2),
                    ranked("transcript:claude:sess-2:300:0", 3),
                ],
            },
        ];
        scope_lists_to_doc_type(&mut lists, "transcript");
        assert_eq!(lists[0].hits.len(), 2, "on-type BM25 lane must survive");
        assert_eq!(
            lists[1]
                .hits
                .iter()
                .map(|hit| hit.entity_id.as_str())
                .collect::<Vec<_>>(),
            vec!["transcript:claude:sess-2:300:0"],
            "vector lane must keep only on-type hits"
        );
        // Original rank is preserved for surviving hits (RRF handles gaps).
        assert_eq!(lists[1].hits[0].rank, 3);
    }

    #[test]
    fn model_rerank_maps_scores_and_builds_documents() {
        let fused = vec![
            fused_hit("knowledge:a", 0.05),
            fused_hit("project_file:p:f:h:1", 0.04),
            fused_hit("commit:p:sha", 0.03),
        ];
        let bm25 = vec![HybridBm25Hit {
            entity_id: "knowledge:a".into(),
            score: 1.0,
            rank: 1,
            doc_type: "knowledge".into(),
            chunk_kind: String::new(),
            role: String::new(),
            title: Some("retry policy".into()),
            excerpt: "the **retry** policy is exponential".into(),
            project_id: None,
            graph_id: None,
            graph_source: None,
            graph_source_connector: None,
            graph_vertex_type: None,
            graph_generation: None,
            logical_ref: None,
        }];
        let properties = BTreeMap::from([(
            "project_file:p:f:h:1".to_string(),
            BTreeMap::from([
                ("file_path".to_string(), "src/retry.rs".to_string()),
                (
                    "content_preview".to_string(),
                    "fn retry_backoff()".to_string(),
                ),
            ]),
        )]);
        let config = RerankConfig {
            top_k: 2, // third candidate stays unsent
            ..RerankConfig::default()
        };
        let mut seen_documents = Vec::new();
        let scores = model_rerank_scores(
            "retry policy",
            &fused,
            &bm25,
            &properties,
            &config,
            |query, documents| {
                assert_eq!(query, "retry policy");
                seen_documents = documents.to_vec();
                Ok(vec![
                    RerankHit {
                        index: 1,
                        relevance_score: 0.9,
                    },
                    RerankHit {
                        index: 0,
                        relevance_score: 0.4,
                    },
                ])
            },
        )
        .unwrap();
        assert_eq!(seen_documents.len(), 2);
        assert!(seen_documents[0].contains("retry policy is exponential"));
        assert!(!seen_documents[0].contains("**"), "highlights stripped");
        assert!(seen_documents[1].contains("src/retry.rs"));
        assert!(seen_documents[1].contains("fn retry_backoff()"));
        assert_eq!(scores.get("project_file:p:f:h:1"), Some(&0.9));
        assert_eq!(scores.get("knowledge:a"), Some(&0.4));
        assert_eq!(scores.get("commit:p:sha"), None);
    }

    /// Model-reranked candidates must outrank the unsent tail regardless of
    /// heuristic multipliers, and relevance order must own the reranked
    /// band; rerank=none returns raw fusion scores.
    #[test]
    fn stage_score_bands_keep_model_order_above_tail() {
        let now = Utc::now();
        let cap = rerank::DEFAULT_COMBINED_CAP;
        let scores = BTreeMap::from([
            ("knowledge:a".to_string(), 0.9_f32),
            ("commit:c".to_string(), 0.1_f32),
        ]);
        let feature = RerankFeatures::default();
        let top = stage_score(
            RerankMode::Model,
            Some(&scores),
            "knowledge:a",
            0.01,
            &feature,
            now,
            cap,
        );
        let low_model = stage_score(
            RerankMode::Model,
            Some(&scores),
            "commit:c",
            0.05,
            &feature,
            now,
            cap,
        );
        let tail = stage_score(
            RerankMode::Model,
            Some(&scores),
            "note:tail",
            0.05,
            &feature,
            now,
            cap,
        );
        assert!(top > low_model, "relevance order owns the model band");
        assert!(low_model > tail, "unsent tail stays below the model band");

        // Degraded model (no scores) behaves exactly like heuristic.
        let degraded = stage_score(
            RerankMode::Model,
            None,
            "knowledge:a",
            0.01,
            &feature,
            now,
            cap,
        );
        let heuristic = stage_score(
            RerankMode::Heuristic,
            None,
            "knowledge:a",
            0.01,
            &feature,
            now,
            cap,
        );
        assert_eq!(degraded, heuristic);

        // rerank=none is raw fusion order.
        assert_eq!(
            stage_score(RerankMode::None, None, "x", 0.42, &feature, now, cap),
            0.42
        );
    }

    #[test]
    fn response_shape_includes_vector_status() {
        let results = [HybridResult {
            rank: 1,
            entity_id: "knowledge:a".into(),
            score: 0.1,
            base_score: 0.1,
            label: "A".into(),
            doc_type: Some("knowledge".into()),
            chunk_kind: None,
            role: None,
            project_id: None,
            relative_path: None,
            source_uri: None,
            sources: BTreeMap::new(),
            excerpt: None,
            graph_id: None,
            graph_source: None,
            graph_source_connector: None,
            graph_vertex_type: None,
            graph_generation: None,
            graph_logical_ref: None,
        }];
        let next_steps = build_next_steps(&results);
        let text = render_text(
            "fixture",
            &results,
            &next_steps,
            &HybridVectorStatus::default(),
            &HybridDegraded::default(),
        );
        assert!(text.contains("Hybrid search: fixture"));
        assert!(text.contains("Vector status"));
        // Breadcrumb names the top seed ref and points into the funnel.
        assert!(
            text.contains("Next steps:"),
            "render should append breadcrumbs: {text}"
        );
        assert!(
            text.contains("bbox_inspect_entity(entity_ref=\"knowledge:a\")"),
            "breadcrumb should carry the top seed ref: {text}"
        );
    }

    #[test]
    fn healthy_response_omits_empty_status_and_null_fields() {
        // A green BM25-only result: no vector lane activity, no degradation,
        // and a result whose optional facets are all absent. None of the
        // empty containers or null option fields should reach the wire.
        let response = HybridSearchResponse {
            text: "fixture".into(),
            next_steps: vec![],
            results: vec![HybridResult {
                rank: 1,
                entity_id: "knowledge:a".into(),
                score: 0.1,
                base_score: 0.1,
                label: "A".into(),
                doc_type: None,
                chunk_kind: None,
                role: None,
                project_id: None,
                relative_path: None,
                source_uri: None,
                sources: BTreeMap::new(),
                excerpt: None,
                graph_id: None,
                graph_source: None,
                graph_source_connector: None,
                graph_vertex_type: None,
                graph_generation: None,
                graph_logical_ref: None,
            }],
            vector_status: HybridVectorStatus::default(),
            degraded: HybridDegraded::default(),
        };
        let value = serde_json::to_value(&response).unwrap();
        assert!(
            value.get("vector_status").is_none(),
            "empty vector_status should be omitted: {value}"
        );
        assert!(
            value.get("degraded").is_none(),
            "empty degraded should be omitted: {value}"
        );
        let first = &value["results"][0];
        for absent in ["doc_type", "chunk_kind", "role", "sources", "excerpt"] {
            assert!(
                first.get(absent).is_none(),
                "empty/null result field {absent} should be omitted: {first}"
            );
        }
    }

    #[test]
    fn render_text_names_unhealthy_vector_route_reasons() {
        let mut queues = BTreeMap::new();
        queues.insert(
            "code".into(),
            bbox_embed::embed::queue::RouteStatus {
                available: false,
                health: "unavailable".into(),
                health_reason: Some("queue_full".into()),
                queue_depth: 10_000,
                indexed_count: 12,
                last_error: Some("embedding route queue full: depth=10000".into()),
                ..bbox_embed::embed::queue::RouteStatus::default()
            },
        );
        queues.insert(
            "notes".into(),
            bbox_embed::embed::queue::RouteStatus {
                available: false,
                health: "unavailable".into(),
                health_reason: Some("credential_missing".into()),
                last_error: Some("VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required".into()),
                ..bbox_embed::embed::queue::RouteStatus::default()
            },
        );
        let vector_status = HybridVectorStatus {
            queues,
            partitions: BTreeMap::new(),
            searched_partitions: Vec::new(),
        };

        let text = render_text(
            "fixture",
            &[],
            &[],
            &vector_status,
            &HybridDegraded::default(),
        );

        assert!(text.contains("Vector route health:"));
        assert!(text.contains("code: unavailable (queue_full; queue_depth=10000"));
        assert!(text.contains("notes: unavailable (credential_missing;"));
    }

    #[test]
    fn next_steps_handle_empty_and_carry_refs() {
        assert!(
            build_next_steps(&[])[0].contains("No seeds"),
            "empty results should yield a broaden-the-query hint"
        );

        let results = [
            HybridResult {
                rank: 1,
                entity_id: "knowledge:top".into(),
                score: 0.9,
                base_score: 0.9,
                label: "top".into(),
                doc_type: Some("knowledge".into()),
                chunk_kind: None,
                role: None,
                project_id: None,
                relative_path: None,
                source_uri: None,
                sources: BTreeMap::new(),
                excerpt: None,
                graph_id: None,
                graph_source: None,
                graph_source_connector: None,
                graph_vertex_type: None,
                graph_generation: None,
                graph_logical_ref: None,
            },
            HybridResult {
                rank: 2,
                entity_id: "thread:abc".into(),
                score: 0.5,
                base_score: 0.5,
                label: "t".into(),
                doc_type: Some("thread".into()),
                chunk_kind: None,
                role: None,
                project_id: None,
                relative_path: None,
                source_uri: None,
                sources: BTreeMap::new(),
                excerpt: None,
                graph_id: None,
                graph_source: None,
                graph_source_connector: None,
                graph_vertex_type: None,
                graph_generation: None,
                graph_logical_ref: None,
            },
        ];
        let steps = build_next_steps(&results);
        assert!(
            steps
                .iter()
                .any(|s| s.contains("bbox_inspect_entity(entity_ref=\"knowledge:top\")"))
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("bbox_find_paths(from=\"knowledge:top\""))
        );
        // Bundle step lists the top refs for direct paste.
        assert!(
            steps
                .iter()
                .any(|s| s.contains("\"knowledge:top\", \"thread:abc\""))
        );
    }

    #[test]
    fn reranked_results_collapse_same_entity() {
        let fused = [FusedHit {
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
            bbox_corpus_core::search::rrf::fuse_rrf(&lists, 60.0, 10)[0].entity_id,
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
                state: bbox_vectors::PartitionState::Active { dims: 3 },
                dims: 3,
                wal_records: 1,
                active_count: 1,
                deleted_count: 0,
                deleted_ratio: 0.0,
                hnsw_rebuilds: 1,
                hnsw: None,
            },
        )]);
        let lists = vector_ranked_lists("q", Some(&[1.0, 0.0]), 5, 0.6, &partitions, &mut degraded)
            .unwrap();
        assert!(lists.is_empty());
        assert!(degraded.skipped_partitions["route-a"].contains("do not match"));
    }

    /// Builds a local loopback mock voyage-multimodal endpoint that always
    /// returns `dims`-length vectors, for network-free exercise of the
    /// query-side visual auto-embed path (never a real provider).
    fn spawn_mock_multimodal_server(dims: usize) -> String {
        use axum::{Json, Router, routing::post};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = Router::new().route(
                    "/v1/multimodalembeddings",
                    post(move |Json(body): Json<serde_json::Value>| async move {
                        let n = body["inputs"].as_array().map(Vec::len).unwrap_or(1);
                        Json(serde_json::json!({
                            "data": (0..n)
                                .map(|_| serde_json::json!({"embedding": vec![0.25_f32; dims]}))
                                .collect::<Vec<_>>()
                        }))
                    }),
                );
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://{addr}/v1/multimodalembeddings")
    }

    /// Deliverable 1 (visual lane wiring): with `[embed.routes.visual]`
    /// configured and vectors already present in that partition,
    /// `vector_ranked_lists` must embed the query via the multimodal alias
    /// and search the partition — the exact gap this task closes (embedding
    /// shipped, retrieval didn't). Asserts the returned `RankedList` carries
    /// the visual partition (the same field `hybrid_search_typed` trims into
    /// `searched_partitions`), no degradation is recorded, and the shared
    /// `vector_weight` is applied like every other lane.
    #[test]
    fn visual_lane_is_searched_when_configured_and_vectors_exist() {
        const DIMS: usize = 256;
        let vector_tmp = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(bbox_vectors::VectorStore::open(vector_tmp.path()).unwrap());
        let _store_guard = bbox_vectors::install_test_global(store.clone());

        let endpoint = spawn_mock_multimodal_server(DIMS);
        let _env = bbox_util::util::test_env_lock();
        // SAFETY: held under test_env_lock() for the mutation window.
        unsafe {
            std::env::set_var("BBOX_HYBRID_SEARCH_VISUAL_TEST_KEY", "test-key");
        }
        let router = bbox_embed::embed::EmbeddingRouter::from_toml_str(&format!(
            r#"
[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "BBOX_HYBRID_SEARCH_VISUAL_TEST_KEY"
output_dimension = {DIMS}
endpoint = "{endpoint}"

[embed.routes.visual]
pdf_figure = "voyage_visual"
"#
        ))
        .unwrap();
        let route_id = router
            .visual_route("pdf_figure")
            .unwrap()
            .unwrap()
            .vector_route_id();
        let _router_guard = bbox_embed::embed::install_test_router(router);

        let entity_id = "project_file:proj1234:filehash01:abcd1234:0";
        store
            .upsert(&route_id, entity_id, "chunkhash", vec![0.25; DIMS])
            .unwrap();

        let mut degraded = HybridDegraded::default();
        let partitions = BTreeMap::from([(
            route_id.clone(),
            PartitionMetrics {
                route: route_id.clone(),
                state: bbox_vectors::PartitionState::Active { dims: DIMS },
                dims: DIMS,
                wal_records: 1,
                active_count: 1,
                deleted_count: 0,
                deleted_ratio: 0.0,
                hnsw_rebuilds: 1,
                hnsw: None,
            },
        )]);
        let lists = vector_ranked_lists(
            "figure of a triad",
            None,
            5,
            0.6,
            &partitions,
            &mut degraded,
        )
        .unwrap();

        unsafe {
            std::env::remove_var("BBOX_HYBRID_SEARCH_VISUAL_TEST_KEY");
        }

        assert!(degraded.is_empty(), "no degradation expected: {degraded:?}");
        assert_eq!(lists.len(), 1, "the visual partition must be searched");
        assert_eq!(lists[0].source, format!("vector:{route_id}"));
        assert_eq!(lists[0].weight, 0.6, "same vector_weight as other lanes");
        assert!(
            lists[0].hits.iter().any(|hit| hit.entity_id == entity_id),
            "expected the upserted visual chunk to surface: {:?}",
            lists[0].hits
        );
    }

    /// Deliverable 1 (zero behavior change when unconfigured): a partition
    /// that matches neither a `Bucket` route nor a visual route (no
    /// `[embed.routes.visual]` entry at all) must fall through to the
    /// exact pre-existing skip — no visual lookup attempted, no extra call.
    #[test]
    fn visual_lane_is_absent_when_no_visual_route_is_configured() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(bbox_vectors::VectorStore::open(vector_tmp.path()).unwrap());
        let _store_guard = bbox_vectors::install_test_global(store.clone());
        let _router_guard =
            bbox_embed::embed::install_test_router(bbox_embed::embed::EmbeddingRouter::default());

        let mut degraded = HybridDegraded::default();
        let partitions = BTreeMap::from([(
            "voyage-visual-orphan".to_string(),
            PartitionMetrics {
                route: "voyage-visual-orphan".into(),
                state: bbox_vectors::PartitionState::Active { dims: 4 },
                dims: 4,
                wal_records: 1,
                active_count: 1,
                deleted_count: 0,
                deleted_ratio: 0.0,
                hnsw_rebuilds: 1,
                hnsw: None,
            },
        )]);
        let lists =
            vector_ranked_lists("figure", None, 5, 0.6, &partitions, &mut degraded).unwrap();

        assert!(lists.is_empty(), "no route maps to this partition");
        assert_eq!(
            degraded.skipped_partitions.get("voyage-visual-orphan"),
            Some(&"no configured bucket maps to this partition".to_string()),
            "unconfigured partitions keep the pre-existing skip message"
        );
        assert!(degraded.vector_errors.is_empty(), "no embed call attempted");
    }

    /// Deliverable 2 (graceful degradation): a multimodal query-embed
    /// failure (provider unreachable) must degrade only the visual lane —
    /// the function still returns `Ok`, and the failure is recorded in
    /// `degraded.vector_errors` like every other vector-lane failure, never
    /// propagated as a search-ending `Err`.
    #[test]
    fn visual_lane_embed_failure_degrades_without_failing_the_search() {
        const DIMS: usize = 256;
        let vector_tmp = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(bbox_vectors::VectorStore::open(vector_tmp.path()).unwrap());
        let _store_guard = bbox_vectors::install_test_global(store.clone());

        let router = bbox_embed::embed::EmbeddingRouter::from_toml_str(&format!(
            r#"
[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "BBOX_HYBRID_SEARCH_VISUAL_UNREACHABLE_KEY"
output_dimension = {DIMS}
endpoint = "http://127.0.0.1:9/v1/multimodalembeddings"

[embed.routes.visual]
pdf_figure = "voyage_visual"
"#
        ))
        .unwrap();
        let route_id = router
            .visual_route("pdf_figure")
            .unwrap()
            .unwrap()
            .vector_route_id();
        let _router_guard = bbox_embed::embed::install_test_router(router);

        let mut degraded = HybridDegraded::default();
        let partitions = BTreeMap::from([(
            route_id.clone(),
            PartitionMetrics {
                route: route_id.clone(),
                state: bbox_vectors::PartitionState::Active { dims: DIMS },
                dims: DIMS,
                wal_records: 1,
                active_count: 1,
                deleted_count: 0,
                deleted_ratio: 0.0,
                hnsw_rebuilds: 1,
                hnsw: None,
            },
        )]);
        let lists =
            vector_ranked_lists("figure", None, 5, 0.6, &partitions, &mut degraded).unwrap();

        assert!(lists.is_empty(), "degraded lane contributes no ranked list");
        assert!(
            degraded.vector_errors.contains_key(&route_id),
            "embed failure must be recorded as a degraded vector error: {degraded:?}"
        );
        assert!(
            !degraded.skipped_partitions.contains_key(&route_id),
            "a configured-but-failing route is a degrade, not a skip"
        );
    }

    #[test]
    fn vector_weight_is_clamped_and_complemented() {
        assert_eq!(fusion_weights(Some(1.8)), (0.0, 1.0));
        assert_eq!(fusion_weights(Some(-0.2)), (1.0, 0.0));

        let (bm25, vector) = fusion_weights(None);
        assert!((bm25 - 0.4).abs() < f32::EPSILON);
        assert!((vector - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn bm25_only_requests_skip_all_vector_status_work() {
        assert!(!vectors_requested(None, 0.0));
        assert!(!vectors_requested(Some(false), 0.6));
        assert!(vectors_requested(None, 0.6));
    }

    #[test]
    fn vector_only_knowledge_features_receive_approval_multiplier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.json");
        let store = KnowledgeStore {
            version: 1,
            built_from: Default::default(),
            provenance: Default::default(),
            entries: vec![KnowledgeEntry {
                id: "vector-only".into(),
                title: "Vector only".into(),
                content: "semantic-only content".into(),
                cluster: None,
                variants: HashMap::new(),
                category: Category::Memory,
                scope: Scope::Project,
                project: Some("/tmp/project".into()),
                project_id: None,
                providers: Vec::new(),
                priority: Priority::Standard,
                weight: 100,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                render: true,
                decay: true,
                review_at: None,
                supersedes: None,
                links: Vec::new(),
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

    #[test]
    fn label_for_entity_prefers_loaded_properties() {
        let ctx = ProviderContext::empty_for_tests();
        let loaded = BTreeMap::from([("title".into(), "Loaded Knowledge Title".into())]);

        assert_eq!(
            label_for_entity(&ctx, "knowledge:abc12345", Some(&loaded), None),
            "Loaded Knowledge Title"
        );
    }

    #[test]
    fn project_file_v2_vectors_share_project_file_scope_and_file_collapse() {
        let hit = |entity_id: &str, rank| RankedHit {
            entity_id: entity_id.into(),
            rank,
            score: 1.0,
            source: "vector:test".into(),
        };
        let mut lists = vec![RankedList {
            source: "vector:test".into(),
            weight: 1.0,
            hits: vec![
                hit("project_file:p:pathhash:chunk:0", 1),
                hit("project_file_v2:p:snapshot:pathhash:chunk:0", 2),
                hit("transcript:codex:s:0:0", 3),
            ],
        }];
        scope_lists_to_doc_type(&mut lists, "project_file");
        assert_eq!(lists[0].hits.len(), 2);
        assert_eq!(
            file_dedup_key("project_file_v2:p:snapshot:pathhash:chunk:0").as_deref(),
            Some("project_file_v2:p:snapshot:pathhash")
        );
    }
}

/// Phase 6 P6-A exit gate: the thread project filter is the one production
/// site that derived project identity from a host path, and in catalog mode
/// it must not.
#[cfg(test)]
mod catalog_thread_filter_exit_gate {
    use super::*;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
        CorpusProject, ProjectId, ProjectScope,
    };
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;

    const PROJECT: &str = "p_000000000000000000000000000000a1";
    const ATTACHMENT: &str = "att_11111111111111111111111111111111";
    const CHECKOUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01";

    struct Fixture {
        _directory: tempfile::TempDir,
        checkout_dir: std::path::PathBuf,
        store: ProjectCatalogStore,
    }

    /// One catalog project with one attached checkout at a REAL directory, so
    /// the same host path is simultaneously (a) resolvable through the catalog
    /// and (b) derivable to a version-1 path hash. Both lanes being available
    /// is what makes the assertions below discriminating rather than vacuous.
    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let checkout_dir = root.join("checkout");
        std::fs::create_dir_all(checkout_dir.join(".bbox/local")).unwrap();
        std::fs::write(
            checkout_dir.join(".bbox/local/checkout-id"),
            format!("{CHECKOUT}\n"),
        )
        .unwrap();
        let store = ProjectCatalogStore::initialize_empty(root.join("projects.json")).unwrap();
        let project_id = ProjectId::parse(PROJECT.to_string()).unwrap();
        let attachment_id = AttachmentId::parse(ATTACHMENT.to_string()).unwrap();
        let scope = PublishedScope::try_new("repo_example", ".").unwrap();
        let dir_string = checkout_dir.to_string_lossy().into_owned();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog, attachments| {
                catalog.projects.insert(
                    project_id.clone(),
                    CorpusProject {
                        project_id: project_id.clone(),
                        scope: ProjectScope::Published(scope.clone()),
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: project_id.as_str().to_string(),
                        created_at: "2026-08-04T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                attachments.attachments.insert(
                    attachment_id.clone(),
                    CheckoutAttachment {
                        attachment_id: attachment_id.clone(),
                        project_id: project_id.clone(),
                        checkout_id: CHECKOUT.into(),
                        checkout_dir: dir_string.clone(),
                        checkout_project_dir: dir_string.clone(),
                        project_root_relpath: scope.bbox_root_relpath().to_string(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: Some("refs/heads/main".into()),
                        capabilities: AttachmentCapabilities::default(),
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-08-04T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        Fixture {
            _directory: directory,
            checkout_dir,
            store,
        }
    }

    /// The version-1 id the SAME host path derives to. Its existence is the
    /// non-vacuity premise: the catalog assertions below are only meaningful
    /// because a path hash was available and went unused.
    fn path_hash_id(fixture: &Fixture) -> String {
        bbox_corpus_core::entity_ref::project_id_for_path(&fixture.checkout_dir)
            .expect("the fixture checkout must be path-hash derivable")
    }

    /// The premise, asserted rather than assumed: on the bridge the stored
    /// thread path DOES resolve through the path hash. Without this, a catalog
    /// arm that simply always returned `false` would pass the gate below.
    #[test]
    fn the_bridge_lane_still_derives_identity_from_the_stored_path() {
        let fixture = fixture();
        let hashed = path_hash_id(&fixture);

        assert!(
            thread_project_matches(
                ProviderProjectAuthority::Bridge,
                &fixture.checkout_dir.to_string_lossy(),
                &hashed,
            ),
            "the retained bridge lane (FD-8) must still match on the path hash"
        );
    }

    /// The gate: catalog mode resolves the stored path through the CATALOG.
    /// A path-hash implementation cannot satisfy this, because the catalog id
    /// is not any hash of the path.
    #[test]
    fn catalog_mode_resolves_a_thread_path_through_the_catalog() {
        let fixture = fixture();

        assert!(
            thread_project_matches(
                ProviderProjectAuthority::Catalog {
                    catalog: &fixture.store
                },
                &fixture.checkout_dir.to_string_lossy(),
                PROJECT,
            ),
            "catalog mode must resolve the attached checkout to its catalog project id"
        );
    }

    /// The negative half: the path-hash id is not an identity catalog mode
    /// will answer to, even though the path derives to it.
    #[test]
    fn catalog_mode_never_answers_to_the_path_hash_identity() {
        let fixture = fixture();
        let hashed = path_hash_id(&fixture);

        assert!(
            !thread_project_matches(
                ProviderProjectAuthority::Catalog {
                    catalog: &fixture.store
                },
                &fixture.checkout_dir.to_string_lossy(),
                &hashed,
            ),
            "catalog mode must not derive project identity from a host path"
        );
    }
}

/// End-to-end word-lane tests over graph vertex documents (unified-retrieval
/// design 7.1 exit gates a and d): a plain query that names no ref finds a
/// project-authored record vertex carrying its full graph identity, and the
/// per-call selectors and the pinned policy snapshot compose BEFORE ranking.
#[cfg(test)]
mod graph_word_lane_pipeline {
    use super::*;
    use bbox_indexing::index::{
        GraphVertexIndexDocument, StaticProjectRecordsProvider, TranscriptIndex,
        build_graph_vertex_doc,
    };
    use std::sync::Arc;

    const PROJECT: &str = "p_000000000000000000000000000000a1";
    const FOREIGN_PROJECT: &str = "p_000000000000000000000000000000b2";
    const GENERATION: &str = "content-hash-gen-1";

    fn vertex_doc(
        project_id: &str,
        graph_id: &str,
        vertex_id: &str,
        label: &str,
    ) -> GraphVertexIndexDocument {
        GraphVertexIndexDocument {
            project_id: project_id.to_string(),
            graph_id: graph_id.to_string(),
            graph_source: "published".to_string(),
            graph_source_connector: None,
            graph_generation: GENERATION.to_string(),
            vertex_id: vertex_id.to_string(),
            vertex_type: "repo:Record".to_string(),
            label: label.to_string(),
            word_properties: Vec::new(),
            text_properties: vec!["quarterly settlement record".to_string()],
            entity_id: format!("project_graph_vertex:{project_id}:{graph_id}:{vertex_id}"),
            logical_ref: format!("project_graph_vertex:{project_id}:{graph_id}:{vertex_id}"),
            source_path: format!("/{project_id}/{graph_id}/vertices.jsonl"),
        }
    }

    /// A real TranscriptIndex on a leaked tempdir (the reader keeps files
    /// open, so the directory must outlive the test). No roots, no records:
    /// the graph documents are added directly through the writer, exactly the
    /// way the activation path emits them.
    fn index_with_documents(documents: &[GraphVertexIndexDocument]) -> TranscriptIndex {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.keep().canonicalize().unwrap();
        let state = root.join("state");
        let index = TranscriptIndex::open_or_create_with_records(
            &state.join("index"),
            Vec::new(),
            None,
            state.join("projects.json"),
            state.join("knowledge.json"),
            state.join("threads.json"),
            state.join("roadmap.json"),
            Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        {
            let fields = index.field_handles();
            let handle = index.index_handle();
            let mut writer = handle.writer(50_000_000).unwrap();
            for document in documents {
                writer
                    .add_document(build_graph_vertex_doc(document, fields))
                    .unwrap();
            }
            writer.commit().unwrap();
        }
        index.reader_reload_for_test();
        index
    }

    fn search(
        index: &TranscriptIndex,
        params: &HybridSearchParams,
        graph_policy: Option<&bbox_indexing::index::GraphWordPolicySnapshot>,
    ) -> HybridSearchResponse {
        let knowledge = Knowledge::detached_view(Vec::new(), BTreeMap::new());
        let ctx = ProviderContext::empty_for_tests();
        let active_selectors = BTreeMap::new();
        let searcher = index.searcher();
        hybrid_search_typed_with_active_selectors_and_searcher(
            index,
            &knowledge,
            &ctx,
            params,
            &active_selectors,
            &searcher,
            graph_policy,
        )
        .unwrap()
    }

    fn params(query: &str) -> HybridSearchParams {
        HybridSearchParams {
            query: query.to_string(),
            limit: Some(10),
            doc_type: None,
            include_vectors: None,
            vector_weight: Some(0.0),
            query_vector: None,
            project: None,
            resolved_project_id: None,
            provisional: None,
            graph_source: None,
            graph_ids: None,
            rerank_cap: None,
            rerank: Some("none".to_string()),
        }
    }

    /// Exit gate (a): a query naming no ref finds a project-authored record
    /// vertex, and the hit carries its graph id, source label, generation,
    /// type, and logical ref alongside the structured project id.
    #[test]
    fn plain_query_finds_graph_vertex_with_identity_fields() {
        let index = index_with_documents(&[vertex_doc(
            PROJECT,
            "governance-record",
            "record-1",
            "Alpha settlement record",
        )]);
        let response = search(&index, &params("quarterly settlement record"), None);

        assert_eq!(response.results.len(), 1, "{}", response.text);
        let hit = &response.results[0];
        assert_eq!(
            hit.entity_id,
            format!("project_graph_vertex:{PROJECT}:governance-record:record-1")
        );
        assert_eq!(hit.doc_type.as_deref(), Some("project_graph_vertex"));
        assert_eq!(hit.graph_id.as_deref(), Some("governance-record"));
        assert_eq!(hit.graph_source.as_deref(), Some("published"));
        assert_eq!(hit.graph_source_connector, None);
        assert_eq!(hit.graph_vertex_type.as_deref(), Some("repo:Record"));
        assert_eq!(hit.graph_generation.as_deref(), Some(GENERATION));
        assert_eq!(
            hit.graph_logical_ref.as_deref(),
            Some(hit.entity_id.as_str())
        );
        assert_eq!(hit.project_id.as_deref(), Some(PROJECT));
        assert_eq!(hit.label, "Alpha settlement record");
    }

    /// The Q6 filter: project scope admits graph documents from the stamped
    /// project_id and drops foreign ones before ranking, and a named-graph
    /// selection narrows within the project.
    #[test]
    fn project_scope_and_graph_selection_reach_the_word_lane() {
        let index = index_with_documents(&[
            vertex_doc(
                PROJECT,
                "governance-record",
                "record-1",
                "Alpha settlement record",
            ),
            vertex_doc(
                PROJECT,
                "other-record",
                "record-2",
                "Beta settlement record",
            ),
            vertex_doc(
                FOREIGN_PROJECT,
                "governance-record",
                "record-9",
                "Foreign settlement record",
            ),
        ]);

        let mut scoped = params("settlement record");
        scoped.resolved_project_id = Some(PROJECT.to_string());
        let response = search(&index, &scoped, None);
        let ids: Vec<&str> = response
            .results
            .iter()
            .map(|hit| hit.entity_id.as_str())
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.iter().all(|id| id.contains(&format!(":{PROJECT}:"))));

        let mut named = params("settlement record");
        named.resolved_project_id = Some(PROJECT.to_string());
        named.graph_ids = Some(vec!["other-record".to_string()]);
        let response = search(&index, &named, None);
        assert_eq!(response.results.len(), 1, "{}", response.text);
        assert!(
            response.results[0]
                .entity_id
                .contains(":other-record:record-2")
        );

        // The keep-filter arm is the backstop, not the filter itself: without
        // a resolved project the foreign vertex is findable.
        let unscoped = search(&index, &params("Foreign settlement record"), None);
        assert!(
            unscoped
                .results
                .iter()
                .any(|hit| hit.entity_id.contains(FOREIGN_PROJECT))
        );
    }

    /// Plane selection and the pinned policy snapshot compose into the same
    /// pre-ranking conjunct: a provisional-plane request sees no published
    /// documents, and a lane the snapshot disables never enters the ranked
    /// list even though its documents sit in the index.
    #[test]
    fn plane_selection_and_policy_snapshot_filter_before_ranking() {
        let index = index_with_documents(&[
            vertex_doc(
                PROJECT,
                "governance-record",
                "record-1",
                "Alpha settlement record",
            ),
            vertex_doc(
                PROJECT,
                "secret-record",
                "record-2",
                "Beta settlement record",
            ),
        ]);

        let mut provisional = params("settlement record");
        provisional.graph_source = Some(vec!["provisional".to_string()]);
        let response = search(&index, &provisional, None);
        assert!(
            response.results.is_empty(),
            "provisional-plane selection must match no published lane: {}",
            response.text
        );

        let policy = bbox_indexing::index::GraphWordPolicySnapshot {
            disabled_graph_lanes: BTreeSet::from([(
                PROJECT.to_string(),
                "secret-record".to_string(),
            )]),
            ..Default::default()
        };
        let response = search(&index, &params("settlement record"), Some(&policy));
        let ids: Vec<&str> = response
            .results
            .iter()
            .map(|hit| hit.entity_id.as_str())
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert!(ids[0].contains(":governance-record:"));
    }

    /// Design 4.1: graph documents are excluded from file dedup and file
    /// aggregation explicitly. Two vertices stamped with the same provenance
    /// path are two addressable units and must both surface, not collapse to
    /// the better-scoring one the way same-file chunks do.
    #[test]
    fn graph_vertices_are_not_collapsed_by_file_dedup() {
        let mut first = vertex_doc(PROJECT, "governance-record", "record-1", "Alpha settlement");
        let mut second = vertex_doc(PROJECT, "governance-record", "record-2", "Beta settlement");
        second.graph_generation = GENERATION.to_string();
        first.source_path = "/same/vertices.jsonl".to_string();
        second.source_path = "/same/vertices.jsonl".to_string();
        let index = index_with_documents(&[first, second]);

        let response = search(&index, &params("settlement"), None);
        assert_eq!(response.results.len(), 2, "{}", response.text);
        assert!(
            response
                .results
                .iter()
                .any(|hit| hit.entity_id.ends_with(":record-1"))
        );
        assert!(
            response
                .results
                .iter()
                .any(|hit| hit.entity_id.ends_with(":record-2"))
        );
    }
}
