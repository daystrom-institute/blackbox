use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::Utc;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::search::rerank::{self, RerankFeatures};
use bbox_corpus_core::search::rrf::{self, RankedHit, RankedList};
use bbox_embed::embed::queue::EmbedStatusResponse;
use bbox_embed::embed::rerank::{RerankConfig, RerankHit, rerank_blocking};
use bbox_embed::embed::{Bucket, EmbeddingRouter, query_cache};
use bbox_embed::embed_queue;
use bbox_indexing::index::{HybridBm25Hit, TranscriptIndex};
use bbox_indexing::projects::ProjectRecord;
use bbox_knowledge::knowledge::Knowledge;
use bbox_providers::entity_loader;
use bbox_providers::providers::ProviderContext;
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
    /// Operator-probe override for the combined rerank multiplier cap
    /// (default 1.5, clamped to [1.0, 4.0]). Exists so eval sweeps can
    /// measure ranking quality per candidate cap (gap-39b3ce16 protocol in
    /// bbox_corpus_core::search::metrics); not intended for normal callers.
    #[serde(default)]
    pub rerank_cap: Option<f32>,
    /// Rerank stage selection. "heuristic" (default) applies the
    /// type/temporal multipliers to the fused RRF scores. "model" sends the
    /// fused top-k candidates to the configured cross-encoder
    /// (`[embed.rerank]`, default rerank-2.5-lite), orders by relevance,
    /// and applies the heuristic multipliers after; on rerank API failure
    /// it falls back to heuristic and reports
    /// `degraded.rerank_unavailable`. "none" returns raw fusion order.
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

pub fn hybrid_search(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    ctx: &ProviderContext<'_>,
    p: &HybridSearchParams,
) -> Result<String> {
    Ok(serde_json::to_string_pretty(&hybrid_search_typed(
        index, knowledge, ctx, p,
    )?)?)
}

pub fn hybrid_search_typed(
    index: &TranscriptIndex,
    knowledge: &Knowledge,
    ctx: &ProviderContext<'_>,
    p: &HybridSearchParams,
) -> Result<HybridSearchResponse> {
    let query = p.query.trim();
    if query.is_empty() {
        anyhow::bail!("query is required");
    }
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
    let bm25_hits_full = index.hybrid_bm25_hits(query, bm25_fetch, p.doc_type.as_deref())?;
    // Truncate the chunk-level list to `fetch` so it doesn't dilute RRF with
    // tail chunks that rank too low to matter. The full set still feeds
    // file-level aggregation below.
    let bm25_hits: Vec<_> = bm25_hits_full.iter().take(fetch).cloned().collect();
    let mut features = features_from_bm25(&bm25_hits);

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
    let partitions = match vectors::try_metrics() {
        Some(partitions) => partitions,
        None => {
            degraded.skipped_partitions.insert(
                "vector_store".into(),
                "vector store is still warming; returning BM25-only results".into(),
            );
            BTreeMap::new()
        }
    };
    let mut vector_status = HybridVectorStatus {
        queues: queue_status_for_hybrid(ctx, p.doc_type.as_deref()).routes,
        partitions,
        searched_partitions: Vec::new(),
    };
    if p.include_vectors.unwrap_or(true) && vector_weight > 0.0 {
        let vector_lists = vector_ranked_lists(
            query,
            p.query_vector.as_deref(),
            fetch,
            vector_weight,
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
    let mut loaded_properties = BTreeMap::new();
    enrich_fused_features(
        index,
        knowledge,
        fused.iter().map(|hit| hit.entity_id.as_str()),
        &mut features,
        &mut loaded_properties,
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
            &bm25_hits,
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
            let bm25 = bm25_hits
                .iter()
                .find(|bm25| bm25.entity_id == hit.entity_id);
            HybridResult {
                rank: 0,
                entity_id: hit.entity_id.clone(),
                score,
                base_score: hit.score,
                label: label_for_entity(
                    ctx,
                    &hit.entity_id,
                    loaded_properties.get(&hit.entity_id),
                    bm25.and_then(|hit| hit.title.as_deref()),
                ),
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
    // Project scoping: when the caller passed `project`, drop scoped entity
    // results from other projects so cross-project keyword pollution
    // (e.g. erlang-test/voyage.ex outranking transcript-search/voyage.rs
    // for "voyage" queries on the local repo) doesn't dominate top-N.
    // Project files encode their project id in the EntityRef; threads carry
    // the source project in their store record. Other entity types pass
    // through unfiltered — commits / knowledge / transcripts are project-
    // agnostic enough that the agent can decide relevance on its own.
    if let Some(target_project_id) = resolve_project_filter(p.project.as_deref(), ctx) {
        results.retain(|hit| keep_under_project_filter(&hit.entity_id, &target_project_id, ctx));
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

/// Resolves the caller's `project` parameter to a canonical project_id
/// (8-hex). Accepts:
///   - a bare 8-hex project_id (returned as-is)
///   - an absolute path that a registered project owns — the registered root
///     itself, any descendant (subdirectory or in-tree worktree), or any git
///     worktree sharing the registered repo's common dir (fleet / agent /
///     workflow worktrees) — resolved to the BASE project_id, since that is
///     the id the indexed corpus lives under
///   - any other absolute path (computed via `entity_ref::project_id_for_path`)
/// Returns `None` when no parameter was supplied or resolution failed (the
/// caller treats `None` as "no scoping").
fn resolve_project_filter(raw: Option<&str>, ctx: &ProviderContext<'_>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    // Bare project_id pass-through.
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(raw.to_lowercase());
    }
    let projects = ctx
        .stores()
        .map(|stores| stores.projects.read().list())
        .unwrap_or_default();
    resolve_project_filter_path(raw, &projects)
}

/// Selector arm of [`resolve_project_filter`], parameterized over the
/// registry list for testability. Registered selectors — alias, canonical
/// path, or any path inside a registered checkout/worktree — collapse to the
/// registered base project_id via the shared Read-intent resolver. A worktree
/// path must NOT fall through to the deterministic hash, which would derive a
/// different id than the base and silently return empty results.
fn resolve_project_filter_path(raw: &str, projects: &[ProjectRecord]) -> Option<String> {
    if let Some(ctx) = bbox_indexing::projects::resolve_project_context(
        raw,
        projects,
        bbox_indexing::projects::ResolveIntent::Read,
    ) {
        return Some(ctx.project_id);
    }
    // Fall back to the deterministic path-derived id even when the project
    // hasn't been registered yet — useful for one-shot scoped searches.
    bbox_corpus_core::entity_ref::project_id_for_path(raw).ok()
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
        Some("project_file") => parts.next() == Some(target_project_id),
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
    bbox_corpus_core::entity_ref::project_id_for_path(&thread.project)
        .ok()
        .as_deref()
        == Some(target_project_id)
}

/// Returns a per-file dedup key when `entity_id` refers to a project_file
/// chunk: `project_file:<proj>:<rel_path_hash>` — i.e. the file path identity
/// minus chunk_hash + occurrence_idx. Returns `None` for any other entity
/// type so commits / transcripts / knowledge entries are passed through
/// without being collapsed against each other.
fn file_dedup_key(entity_id: &str) -> Option<String> {
    let mut parts = entity_id.split(':');
    if parts.next()? != "project_file" {
        return None;
    }
    let proj = parts.next()?;
    let rel_path_hash = parts.next()?;
    Some(format!("project_file:{proj}:{rel_path_hash}"))
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

/// Coverage-status hook: the daemon registers
/// `embed_runtime::status_response_for_buckets` here at SharedState
/// construction (dependency inversion — coverage walks daemon-side
/// reembed routing this layer must not name). Unregistered means
/// queue-local status only.
type CoverageStatusFn = fn(
    &bbox_providers::providers::CorpusStores<'_>,
    &[Bucket],
) -> anyhow::Result<EmbedStatusResponse>;
static COVERAGE_STATUS_HOOK: std::sync::OnceLock<CoverageStatusFn> = std::sync::OnceLock::new();

/// Register the embedding coverage-status source. Idempotent; first wins.
pub fn register_coverage_status_hook(hook: CoverageStatusFn) {
    let _ = COVERAGE_STATUS_HOOK.set(hook);
}

fn queue_status_for_hybrid(
    ctx: &ProviderContext<'_>,
    doc_type: Option<&str>,
) -> bbox_embed::embed::queue::EmbedStatusResponse {
    let Some(stores) = ctx.stores() else {
        return embed_queue::status_response();
    };
    let Some(buckets) = status_buckets_for_doc_type(doc_type) else {
        return embed_queue::status_response();
    };
    (match COVERAGE_STATUS_HOOK.get() {
        Some(hook) => hook(stores, &buckets),
        None => Ok(embed_queue::status_response()),
    }).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "embedding coverage status failed; falling back to queue-local status");
        embed_queue::status_response()
    })
}

fn status_buckets_for_doc_type(doc_type: Option<&str>) -> Option<Vec<Bucket>> {
    match doc_type?.trim() {
        "knowledge" | "roadmap" => Some(vec![Bucket::Knowledge]),
        "thread" => Some(vec![Bucket::Threads]),
        "note" => Some(vec![Bucket::Notes]),
        "commit" => Some(vec![Bucket::GitMessage]),
        "project_file" => Some(vec![Bucket::Code, Bucket::Docs]),
        _ => None,
    }
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
        } else {
            let Some(buckets) = route_buckets.get(route) else {
                degraded.skipped_partitions.insert(
                    route.clone(),
                    "no configured bucket maps to this partition".into(),
                );
                continue;
            };
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
        };
        let hits = vectors::search(route, &query_vector, fetch)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankMode {
    Heuristic,
    Model,
    None,
}

fn parse_rerank_mode(raw: Option<&str>) -> Result<RerankMode> {
    match raw.map(str::trim).unwrap_or("heuristic") {
        "" | "heuristic" => Ok(RerankMode::Heuristic),
        "model" => Ok(RerankMode::Model),
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
        for key in ["title", "file_path", "symbol"] {
            if let Some(value) = properties.get(key) {
                parts.push(value.clone());
            }
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
) -> Result<()> {
    for entity_id in entity_ids {
        if !features.contains_key(entity_id) {
            let indexed_properties = index.entity_properties(entity_id)?;
            let mut feature = indexed_properties
                .as_ref()
                .map(features_from_properties)
                .unwrap_or_default();
            if let Some(properties) = indexed_properties {
                loaded_properties.insert(entity_id.to_string(), properties);
            }
            if entity_id.starts_with("knowledge:") && feature.doc_type.is_none() {
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
    let id = entity_id.strip_prefix("knowledge:")?;
    let entry = knowledge.entry(id)?;
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

fn label_for_entity(
    ctx: &ProviderContext<'_>,
    entity_id: &str,
    loaded: Option<&BTreeMap<String, String>>,
    bm25_title: Option<&str>,
) -> String {
    EntityRef::parse(entity_id)
        .ok()
        .and_then(|r| entity_loader::compact_label(ctx, &r, loaded))
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
        assert_eq!(parse_rerank_mode(None).unwrap(), RerankMode::Heuristic);
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
            sources: BTreeMap::new(),
            excerpt: None,
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
                sources: BTreeMap::new(),
                excerpt: None,
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
                sources: BTreeMap::new(),
                excerpt: None,
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
                sources: BTreeMap::new(),
                excerpt: None,
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

    #[test]
    fn vector_weight_is_clamped_and_complemented() {
        assert_eq!(fusion_weights(Some(1.8)), (0.0, 1.0));
        assert_eq!(fusion_weights(Some(-0.2)), (1.0, 0.0));

        let (bm25, vector) = fusion_weights(None);
        assert!((bm25 - 0.4).abs() < f32::EPSILON);
        assert!((vector - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn vector_only_knowledge_features_receive_approval_multiplier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.json");
        let store = KnowledgeStore {
            version: 1,
            write_redirects: Default::default(),
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
    fn project_filter_passes_bare_hex_id_through() {
        let ctx = ProviderContext::empty_for_tests();
        assert_eq!(
            resolve_project_filter(Some("ABCD1234"), &ctx).as_deref(),
            Some("abcd1234")
        );
        assert_eq!(resolve_project_filter(Some("  "), &ctx), None);
        assert_eq!(resolve_project_filter(None, &ctx), None);
    }

    fn init_git_repo(path: &std::path::Path) {
        use std::process::Command;
        for args in [
            vec!["init"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Blackbox Test",
                "-c",
                "user.email=blackbox@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(&args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn project_filter_resolves_worktree_and_descendant_paths_to_base_project_id() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("repo");
        std::fs::create_dir_all(&base).unwrap();
        init_git_repo(&base);
        let base_canon = base.canonicalize().unwrap();
        let registered = vec![bbox_indexing::projects::ProjectRecord {
            project_id: "feedbeef".into(),
            repo_id: None,
            canonical_path: base_canon.to_string_lossy().into_owned(),
            registered_at: "2026-01-01T00:00:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        }];

        // The registered root resolves to the registry id.
        assert_eq!(
            resolve_project_filter_path(base_canon.to_str().unwrap(), &registered).as_deref(),
            Some("feedbeef")
        );

        // A descendant path resolves to the ROOT project's id, not a
        // deterministic hash of the subdirectory.
        let subdir = base_canon.join("src");
        std::fs::create_dir_all(&subdir).unwrap();
        assert_eq!(
            resolve_project_filter_path(subdir.to_str().unwrap(), &registered).as_deref(),
            Some("feedbeef")
        );

        // A linked worktree (any branch) resolves to the BASE project's id —
        // the id the indexed corpus lives under — instead of hashing the
        // worktree path to a foreign id with silently-empty results.
        let worktree = tmp.path().join("wt");
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&base)
            .args([
                "worktree",
                "add",
                "-b",
                "arc/x",
                worktree.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let worktree_canon = worktree.canonicalize().unwrap();
        assert_eq!(
            resolve_project_filter_path(worktree_canon.to_str().unwrap(), &registered).as_deref(),
            Some("feedbeef")
        );

        // An unregistered plain directory keeps the deterministic
        // path-derived id fallback.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let expected =
            bbox_corpus_core::entity_ref::project_id_for_path(plain.to_str().unwrap()).unwrap();
        assert_eq!(
            resolve_project_filter_path(plain.to_str().unwrap(), &registered),
            Some(expected)
        );

        // A registered alias resolves to the registry id.
        let mut aliased = registered.clone();
        aliased[0].aliases = ["blackbox".to_string()].into();
        assert_eq!(
            resolve_project_filter_path("blackbox", &aliased).as_deref(),
            Some("feedbeef")
        );
        // An unknown non-path selector resolves to nothing.
        assert_eq!(resolve_project_filter_path("not-an-alias", &aliased), None);
    }
}
