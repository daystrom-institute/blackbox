//! Daemon-side embedding runtime — the upward-coupled half of the embed
//! surface. The contract (Bucket, EmbeddingRouter, the queue handle and
//! enqueue helpers) lives in `crate::embed` / `crate::embed_queue`; this
//! module owns everything that needs `SharedState`, orchestration agent
//! types, or routing-verdict dispatch: reembed orchestration, embedding
//! route coverage, agent-manifest embeddings, and the knowledge
//! contradiction detector (registered into the queue worker's hook at
//! SharedState construction).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;
use std::sync::OnceLock;

use crate::embed::queue::{EmbedRequest, EmbedStatusResponse};
use crate::embed::{Bucket, EmbeddingRouter};
use crate::embed_queue::status_response;
use crate::orchestration::agents::types::{AgentManifest, AgentRef};
use crate::routing::RoutingVerdict;
use crate::server::dispatch::dispatch_routing_verdict_direct;
use crate::server::state::SharedState;
use std::path::PathBuf;

use crate::embed::queue;
use crate::embed_queue::content_hash;
use crate::orchestration::agents::types::{AgentEmbedding, AgentEmbeddingComponents};
use bbox_chunker::Chunk;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_index::index::EmbeddingSourceDoc;
use bbox_knowledge::knowledge::KnowledgeEntry;
use bbox_threads::notes::NoteParams;

/// Adapter with the queue worker's hook signature; registered via
/// `embed::queue::register_contradiction_hook` at SharedState construction.
pub(crate) fn contradiction_hook(request: &EmbedRequest, vector_route: &str, vector: &[f32]) {
    maybe_detect_knowledge_contradiction(request, vector_route, vector);
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReembedParams {
    pub route: String,
    #[serde(default)]
    pub include_transcripts: bool,
    #[serde(default)]
    pub max_entities: Option<usize>,
}

pub fn reembed_start(p: &ReembedParams, state: Arc<SharedState>) -> Result<String> {
    let buckets = buckets_for_reembed_route(&p.route)?;
    if buckets.contains(&Bucket::Transcripts) && !p.include_transcripts {
        bail!(
            "transcript re-embed is intentionally guarded because it reads the transcript corpus; rerun with include_transcripts=true only when you explicitly want that heavy rebuild"
        );
    }
    let route = p.route.trim().to_string();
    let max_entities = p.max_entities;
    // Kick the residue sweeper immediately: bbox_reembed stays "start
    // convergence now" (its own wave below still fires), and the sweeper
    // takes over across-wave persistence so residue past the queue cap is
    // driven to convergence instead of stranded (gap-7323e96c).
    sweep_notify().notify_one();
    tokio::task::spawn_blocking(move || {
        match enqueue_reembed_routes(&state, &buckets, max_entities) {
            Ok(enqueued) => tracing::info!(
                route = %route,
                ?max_entities,
                enqueued,
                "embedding rebuild queue refill completed"
            ),
            Err(err) => tracing::warn!(
                route = %route,
                error = %err,
                "embedding rebuild queue refill failed"
            ),
        }
    });
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "route": p.route,
        "max_entities": p.max_entities,
        "message": "rebuild queue refill started; residue past the queue cap converges automatically via the background sweeper. Final enqueue count will be logged.",
    }))?)
}

fn buckets_for_reembed_route(route: &str) -> Result<Vec<Bucket>> {
    let route = route.trim();
    if route.is_empty() {
        bail!("route is required");
    }
    if route == "all" {
        return Ok(Bucket::ALL.to_vec());
    }
    // Every route except the transcript corpus (whose rebuild is guarded
    // behind include_transcripts because it is a heavy scan). This is the
    // residue-convergence sweep the nightly embed-compaction arc runs so
    // items that predate a route or were dropped after retries eventually
    // embed (gap-b9d39c10): enqueue dedupes already-embedded items, so the
    // sweep is idempotent.
    if route == "backfill" {
        return Ok(Bucket::ALL
            .iter()
            .copied()
            .filter(|bucket| *bucket != Bucket::Transcripts)
            .collect());
    }
    Bucket::ALL
        .iter()
        .copied()
        .find(|bucket| bucket.as_str() == route)
        .map(|bucket| vec![bucket])
        .with_context(|| {
            format!(
                "unknown embedding route `{route}`; expected one of: all, backfill, {}",
                Bucket::ALL
                    .iter()
                    .map(|bucket| bucket.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbedPartitionsParams {
    /// "list" (default) reports every partition with its route mapping;
    /// "prune" deletes orphaned partitions older than `older_than_days`.
    #[serde(default)]
    pub action: Option<String>,
    /// Required for prune: only partitions whose last write is older than
    /// this many days are candidates. There is no default on purpose —
    /// the age threshold is an operator decision.
    #[serde(default)]
    pub older_than_days: Option<u64>,
    /// Prune is dry-run by default; pass apply=true to delete.
    #[serde(default)]
    pub apply: bool,
    /// Required for scrub: the mapped partition to sweep for vectors whose
    /// entities now attribute to a different route (e.g. after a bucket
    /// attribution rule change).
    #[serde(default)]
    pub route: Option<String>,
}

/// Partition lifecycle (`bbox_embed_partitions`). Deliberately separate
/// from `bbox_reembed`, which never prunes: re-embedding and reclaiming
/// orphaned vector spaces are different operator decisions
/// (design/corpus/agentic-corpus/multimodal-embedding-routing.md Layer 5).
pub fn embed_partitions(p: &EmbedPartitionsParams, state: &SharedState) -> Result<String> {
    let router = EmbeddingRouter::load_default().unwrap_or_default();
    if p.action.as_deref().map(str::trim) == Some("scrub") {
        return embed_partitions_scrub(p, state, &router);
    }
    let infos = crate::vectors::partition_infos()?
        .context("vector store is still warming up; retry shortly")?;
    embed_partitions_with(
        p,
        &router,
        infos,
        chrono::Utc::now(),
        crate::vectors::remove_partition,
    )
}

/// How one vector row in a scrubbed partition classifies against CURRENT
/// bucket attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrubClass {
    /// Entity attributes to this partition; keep.
    Matched,
    /// Entity attributes to a different route now; delete on apply.
    Mismatched,
    /// Entity no longer resolvable in the index; kept (conservative:
    /// index lag must not delete vectors).
    Missing,
    /// Not a project_file entity; scrub only reclassifies project files.
    Foreign,
}

/// Pure classification pass, injected lookups for testability.
fn partition_scrub_plan(
    entities: &[String],
    classify: impl Fn(&str) -> ScrubClass,
) -> (Vec<String>, usize, usize, usize) {
    let mut mismatched = Vec::new();
    let (mut matched, mut missing, mut foreign) = (0usize, 0usize, 0usize);
    for entity in entities {
        match classify(entity) {
            ScrubClass::Matched => matched += 1,
            ScrubClass::Missing => missing += 1,
            ScrubClass::Foreign => foreign += 1,
            ScrubClass::Mismatched => mismatched.push(entity.clone()),
        }
    }
    (mismatched, matched, missing, foreign)
}

fn project_file_project_id(entity: &str) -> Option<String> {
    match EntityRef::parse(entity).ok()? {
        EntityRef::ProjectFile { project_id, .. } | EntityRef::ProjectFileV2 { project_id, .. } => {
            Some(project_id)
        }
        _ => None,
    }
}

/// `action="scrub"`: sweep one MAPPED partition for vectors whose entities
/// attribute to a different route under the current shared code-vs-prose
/// rule (gap-42fa1d68: attribution changes strand vectors in the wrong
/// partition; prune only handles whole unmapped partitions). Dry-run by
/// default; apply=true deletes the mismatched rows.
fn embed_partitions_scrub(
    p: &EmbedPartitionsParams,
    state: &SharedState,
    router: &EmbeddingRouter,
) -> Result<String> {
    let Some(route) = p.route.as_deref().map(str::trim).filter(|r| !r.is_empty()) else {
        bail!("scrub requires `route` (the mapped partition to sweep)");
    };
    let mapped: std::collections::BTreeSet<String> = router
        .configured_routes()
        .iter()
        .map(|r| r.vector_route_id())
        .collect();
    if !mapped.contains(route) {
        bail!(
            "partition `{route}` is not mapped by current route config;              unmapped partitions are pruned whole via action=\"prune\""
        );
    }
    let entities: Vec<String> = crate::vectors::iter_active(route, None)?
        .map(|entry| entry.entity_id)
        .collect();
    let idx = state.idx.read();
    let (mismatched, matched, missing, foreign) = partition_scrub_plan(&entities, |entity| {
        let Some(project_id) = project_file_project_id(entity) else {
            return ScrubClass::Foreign;
        };
        let Ok(Some(props)) = idx.entity_properties(entity) else {
            return ScrubClass::Missing;
        };
        let bucket = if crate::embed_queue::is_code_chunk(
            props.get("language").map(String::as_str),
            props.get("chunk_kind").map(String::as_str).unwrap_or(""),
        ) {
            Bucket::Code
        } else {
            Bucket::Docs
        };
        match router.route(bucket, Some(&project_id)) {
            Ok(expected) if expected.vector_route_id() == route => ScrubClass::Matched,
            Ok(_) => ScrubClass::Mismatched,
            Err(_) => ScrubClass::Missing,
        }
    });
    drop(idx);
    let mut deleted = 0usize;
    let mut errors = Vec::new();
    if p.apply {
        for entity in &mismatched {
            match crate::vectors::delete(route, entity) {
                Ok(()) => deleted += 1,
                Err(err) => errors.push(json!({
                    "entity": entity,
                    "error": format!("{err:#}"),
                })),
            }
        }
    }
    Ok(serde_json::to_string_pretty(&json!({
        "action": "scrub",
        "route": route,
        "dry_run": !p.apply,
        "examined": entities.len(),
        "matched": matched,
        "mismatched": mismatched.len(),
        "missing_from_index_kept": missing,
        "foreign_kept": foreign,
        "deleted": deleted,
        "errors": errors,
        "mismatched_sample": mismatched.iter().take(10).collect::<Vec<_>>(),
    }))?)
}

fn embed_partitions_with(
    p: &EmbedPartitionsParams,
    router: &EmbeddingRouter,
    infos: Vec<crate::vectors::PartitionInfo>,
    now: chrono::DateTime<chrono::Utc>,
    mut remove: impl FnMut(&str) -> Result<bool>,
) -> Result<String> {
    let action = p.action.as_deref().map(str::trim).unwrap_or("list");
    if !matches!(action, "list" | "prune") {
        bail!("unknown action `{action}`; expected `list` or `prune`");
    }

    // Which buckets claim each partition under the CURRENT config —
    // vector_route_id is the join key on both sides.
    #[derive(Clone)]
    struct Mapping {
        labels: Vec<String>,
        provider: String,
        endpoint_kind: crate::embed::EmbedEndpointKind,
        document_model: String,
        query_model: String,
        output_dtype: String,
        compatibility_family: String,
    }

    let mut mapped: BTreeMap<String, Mapping> = BTreeMap::new();
    for route in router.configured_routes() {
        let label = match &route.project_id {
            Some(project) => format!("{}@{project}", route.bucket.as_str()),
            None => route.bucket.as_str().to_string(),
        };
        mapped
            .entry(route.vector_route_id())
            .or_insert_with(|| Mapping {
                labels: Vec::new(),
                provider: route.provider_id.clone(),
                endpoint_kind: route.endpoint_kind,
                document_model: route.document_model.clone(),
                query_model: route.query_model.clone(),
                output_dtype: route.output_dtype.as_str().to_string(),
                compatibility_family: route.compatibility_family.clone(),
            })
            .labels
            .push(label);
    }
    // Visual routes are chunk-kind-keyed rather than Bucket-keyed. They are
    // still live partition mappings and must participate in both lifecycle
    // reporting and prune protection.
    for (route_id, kind, meta) in router.configured_visual_routes() {
        mapped
            .entry(route_id)
            .or_insert_with(|| Mapping {
                labels: Vec::new(),
                provider: meta.provider_id.clone(),
                endpoint_kind: crate::embed::EmbedEndpointKind::Multimodal,
                document_model: meta.document_model.clone(),
                query_model: meta.document_model.clone(),
                output_dtype: meta.output_dtype.as_str().to_string(),
                compatibility_family: meta.compatibility_family.clone(),
            })
            .labels
            .push(format!("visual:{kind}"));
    }

    let partitions = infos
        .iter()
        .map(|info| {
            let mapping = mapped.get(&info.route);
            json!({
                "route": info.route,
                "dims": info.dims,
                "active_count": info.active_count,
                "last_write": info.last_write.map(|ts| ts.to_rfc3339()),
                "disk_bytes": info.disk_bytes,
                "mapped": mapping.is_some(),
                "mapped_buckets": mapping.map(|mapping| mapping.labels.clone()).unwrap_or_default(),
                "provider": mapping.map(|mapping| mapping.provider.clone()),
                "endpoint_kind": mapping.map(|mapping| mapping.endpoint_kind),
                "document_model": mapping.map(|mapping| mapping.document_model.clone()),
                "query_model": mapping.map(|mapping| mapping.query_model.clone()),
                "output_dtype": mapping.map(|mapping| mapping.output_dtype.clone()),
                "compatibility_family": mapping.map(|mapping| mapping.compatibility_family.clone()),
            })
        })
        .collect::<Vec<_>>();

    if action == "list" {
        return Ok(serde_json::to_string_pretty(&json!({
            "action": "list",
            "partitions": partitions,
        }))?);
    }

    let Some(older_than_days) = p.older_than_days else {
        bail!(
            "prune requires older_than_days: only partitions unmapped by current route \
             config AND idle beyond that age are deleted"
        );
    };
    let cutoff = now - chrono::Duration::days(older_than_days as i64);
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for info in &infos {
        if mapped.contains_key(&info.route) {
            continue; // a configured bucket still writes/searches here
        }
        match info.last_write {
            Some(last_write) if last_write < cutoff => candidates.push(info.route.clone()),
            Some(_) => skipped.push(json!({
                "route": info.route,
                "reason": format!("unmapped but written within {older_than_days} day(s)"),
            })),
            // No file timestamps — age unknowable; stay conservative.
            None => skipped.push(json!({
                "route": info.route,
                "reason": "unmapped but last_write is unknown",
            })),
        }
    }

    let mut pruned = Vec::new();
    let mut errors = Vec::new();
    if p.apply {
        for route in &candidates {
            match remove(route) {
                Ok(true) => pruned.push(route.clone()),
                Ok(false) => errors.push(json!({
                    "route": route,
                    "error": "partition disappeared before prune",
                })),
                Err(err) => errors.push(json!({
                    "route": route,
                    "error": format!("{err:#}"),
                })),
            }
        }
    }

    Ok(serde_json::to_string_pretty(&json!({
        "action": "prune",
        "dry_run": !p.apply,
        "older_than_days": older_than_days,
        "prune_candidates": candidates,
        "pruned": pruned,
        "skipped": skipped,
        "errors": errors,
        "partitions": partitions,
    }))?)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RouteCoverage {
    pub source_count: u64,
    pub indexed_count: u64,
}

pub(crate) fn route_coverage(
    stores: &crate::providers::CorpusStores<'_>,
    buckets: &[Bucket],
) -> Result<BTreeMap<String, RouteCoverage>> {
    let router = EmbeddingRouter::load_default()?;
    let mut coverage = BTreeMap::new();
    let mut active_by_route = BTreeMap::new();
    if buckets.contains(&Bucket::Knowledge) {
        for entry in stores.kb.read().all_entries() {
            // Non-indexable entries (Deleted/Draft/Disabled) aren't in the
            // searchable index — skip so coverage reflects indexable knowledge.
            if !crate::index::indexable_knowledge_entry(entry) {
                continue;
            }
            record_coverage(
                &router,
                &mut coverage,
                &mut active_by_route,
                Bucket::Knowledge,
                None,
                &crate::index::knowledge_entity_id(&entry.id),
                &crate::index::knowledge_chunk_hash(entry),
            )?;
        }
        for item in stores.roadmap.read().all_items() {
            // Rejected items are not indexed — skip
            if matches!(item.status, crate::roadmap::RoadmapStatus::Rejected) {
                continue;
            }
            record_coverage(
                &router,
                &mut coverage,
                &mut active_by_route,
                Bucket::Knowledge,
                None,
                &crate::index::roadmap_entity_id(&item.id),
                &crate::index::roadmap_chunk_hash(item),
            )?;
        }
    }
    if buckets.contains(&Bucket::Notes) {
        for note in stores.notes.read().all() {
            record_coverage(
                &router,
                &mut coverage,
                &mut active_by_route,
                Bucket::Notes,
                None,
                &EntityRef::Note {
                    note_id: note.id.clone(),
                }
                .to_string(),
                &crate::embed_queue::note_chunk_hash(note),
            )?;
        }
    }
    if buckets.contains(&Bucket::Threads) {
        for thread in stores.threads.read().all() {
            record_coverage(
                &router,
                &mut coverage,
                &mut active_by_route,
                Bucket::Threads,
                None,
                &EntityRef::Thread {
                    thread_id: thread.id.clone(),
                }
                .to_string(),
                &crate::embed_queue::thread_chunk_hash(thread),
            )?;
        }
    }
    if buckets.contains(&Bucket::AgentManifest) {
        record_agent_manifest_coverage(stores, &router, &mut coverage, &mut active_by_route)?;
    }
    if buckets.contains(&Bucket::Graph) {
        let views = stores.project_graph_views.read();
        for (project_id, view) in views.iter_published() {
            for (graph_id, entry) in &view.graphs {
                let Some(graph) = entry.graph() else {
                    continue;
                };
                for projection in bbox_project_graph::graph_embed_projections(graph) {
                    record_coverage(
                        &router,
                        &mut coverage,
                        &mut active_by_route,
                        Bucket::Graph,
                        Some(project_id.as_str()),
                        &EntityRef::ProjectGraphVertex {
                            project_id: project_id.as_str().to_string(),
                            graph_id: graph_id.clone(),
                            vertex_id: projection.vertex_id.clone(),
                        }
                        .to_string(),
                        &projection.content_hash(),
                    )?;
                }
            }
        }
    }
    let doc_types = reembed_index_doc_types(buckets);
    if !doc_types.is_empty() {
        stores
            .idx
            .read()
            .for_each_embedding_source_doc_for_doc_types(&doc_types, None, |doc| {
                record_index_doc_coverage(
                    &router,
                    &mut coverage,
                    &mut active_by_route,
                    buckets,
                    &doc,
                )?;
                Ok(())
            })?;
    }
    Ok(coverage)
}

fn record_agent_manifest_coverage(
    stores: &crate::providers::CorpusStores<'_>,
    router: &EmbeddingRouter,
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
) -> Result<()> {
    let catalog = stores.artifacts.read();
    let entries = catalog.list(&crate::artifacts::ArtifactListParams {
        kind: Some(crate::artifacts::ArtifactKind::Agent),
        name: None,
        include_superseded: true,
    })?;
    for entry in entries {
        // TODO(phase-4-shadowing): plumb project_id when caller has it to enable local shadowing.
        let Some(value) =
            catalog.load_artifact_value(crate::artifacts::ArtifactKind::Agent, &entry.name)?
        else {
            continue;
        };
        let manifest_value = value.get("manifest").unwrap_or(&value);
        let Ok(manifest) = serde_json::from_value::<
            crate::orchestration::agents::types::AgentManifest,
        >(manifest_value.clone()) else {
            continue;
        };
        let Ok(version) = entry.version.parse::<u32>() else {
            continue;
        };
        let agent = crate::orchestration::agents::types::AgentRef {
            name: entry.name,
            version,
        };
        for component in [
            AgentManifestComponent::Primary,
            AgentManifestComponent::WhenToUse,
            AgentManifestComponent::AntiPatterns,
        ] {
            let Some(chunk_hash) = agent_component_hash(&manifest, component) else {
                continue;
            };
            record_coverage(
                router,
                coverage,
                active_by_route,
                Bucket::AgentManifest,
                None,
                &agent_component_entity_id(&agent, component),
                &chunk_hash,
            )?;
        }
    }
    Ok(())
}

fn record_index_doc_coverage(
    router: &EmbeddingRouter,
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
    buckets: &[Bucket],
    doc: &EmbeddingSourceDoc,
) -> Result<()> {
    // Empty-text docs are not embeddable (providers reject empty input;
    // the queue skips them at enqueue) — excluding them here keeps
    // coverage convergent instead of reporting permanent phantom residue
    // (gap-e3e033ce).
    if !queue::embeddable_text(&doc.content) {
        return Ok(());
    }
    // Visual chunks (X-IMG's `image`, X-PDF's `pdf_figure`) ride the same
    // `project_file` doc_type as Code/Docs chunks but have no `Bucket` of
    // their own: they route through `[embed.routes.visual]`,
    // chunk-kind-keyed. This runs whenever the
    // project_file scan reaches one (i.e. whenever Code or Docs is in
    // `buckets`, since that's what makes `reembed_index_doc_types` include
    // "project_file"), independent of which of Code/Docs was requested:
    // visual chunks aren't either bucket, so there is no narrower
    // `buckets` gate to check them against.
    if doc.doc_type == "project_file" && crate::embed_queue::is_visual_chunk_kind(&doc.chunk_kind) {
        let Some(entity_id) = &doc.entity_id else {
            return Ok(());
        };
        let chunk_hash = doc
            .chunk_hash
            .clone()
            .unwrap_or_else(|| crate::embed_queue::content_hash(&doc.content));
        return record_visual_coverage(
            router,
            coverage,
            active_by_route,
            &doc.chunk_kind,
            entity_id,
            &chunk_hash,
        );
    }
    let Some(bucket) = reembed_index_doc_bucket(doc) else {
        return Ok(());
    };
    if !buckets.contains(&bucket) {
        return Ok(());
    }
    match bucket {
        Bucket::Code | Bucket::Docs => {
            let Some(chunk) = chunk_from_embedding_doc(doc) else {
                return Ok(());
            };
            let Some(entity_id) = doc.entity_id.as_deref() else {
                return Ok(());
            };
            // The envelope crosses every boundary that compares project-file
            // vector hashes (plan section 9 item 5). The vector record's
            // freshness hash for a text row IS the envelope hash, so comparing
            // the raw `chunk_hash` against `active_entity_hashes` would read a
            // permanent phantom zero after the version bump - masking real
            // embedding outages and turning every residue sweep into
            // full-corpus churn. The visual arm above keeps raw comparison
            // because that lane is outside the envelope.
            record_coverage(
                router,
                coverage,
                active_by_route,
                bucket,
                Some(&chunk.project_id),
                entity_id,
                &crate::embed_queue::project_file_text_content_hash(&chunk.chunk_hash),
            )
        }
        Bucket::Transcripts => {
            let chunk_hash = doc
                .chunk_hash
                .clone()
                .unwrap_or_else(|| crate::embed_queue::content_hash(&doc.content));
            record_coverage(
                router,
                coverage,
                active_by_route,
                Bucket::Transcripts,
                None,
                &EntityRef::Transcript {
                    provider: doc.account.clone(),
                    session_id: doc.session_id.clone(),
                    line_offset: doc.byte_offset,
                    event_idx: 0,
                }
                .to_string(),
                &chunk_hash,
            )
        }
        Bucket::GitMessage => {
            let (Some(entity_id), Some(chunk_hash)) = (&doc.entity_id, &doc.chunk_hash) else {
                return Ok(());
            };
            record_coverage(
                router,
                coverage,
                active_by_route,
                Bucket::GitMessage,
                None,
                entity_id,
                chunk_hash,
            )
        }
        Bucket::Knowledge
        | Bucket::Notes
        | Bucket::Threads
        | Bucket::AgentManifest
        | Bucket::Graph => Ok(()),
    }
}

fn record_coverage(
    router: &EmbeddingRouter,
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
    bucket: Bucket,
    project_id: Option<&str>,
    entity_id: &str,
    chunk_hash: &str,
) -> Result<()> {
    let (queue_route, vector_route) = router.queue_and_vector_route(bucket, project_id)?;
    record_coverage_at(
        coverage,
        active_by_route,
        queue_route,
        vector_route,
        entity_id,
        chunk_hash,
    )
}

/// Visual-lane analog of `record_coverage`: routes are chunk-kind-keyed
/// (`[embed.routes.visual]`), not `Bucket`-keyed, so this resolves through
/// `EmbeddingRouter::visual_route` instead of `queue_and_vector_route`. An
/// unconfigured visual kind is skipped rather than counted: visual
/// embedding is opt-in per kind, so an unrouted kind has no partition to
/// report coverage against.
fn record_visual_coverage(
    router: &EmbeddingRouter,
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
    chunk_kind: &str,
    entity_id: &str,
    chunk_hash: &str,
) -> Result<()> {
    let Some(meta) = router.visual_route(chunk_kind)? else {
        return Ok(());
    };
    record_coverage_at(
        coverage,
        active_by_route,
        format!("visual:{chunk_kind}"),
        meta.vector_route_id(),
        entity_id,
        chunk_hash,
    )
}

fn record_coverage_at(
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
    queue_route: String,
    vector_route: String,
    entity_id: &str,
    chunk_hash: &str,
) -> Result<()> {
    let entry = coverage.entry(queue_route).or_default();
    entry.source_count = entry.source_count.saturating_add(1);
    if !active_by_route.contains_key(&vector_route) {
        active_by_route.insert(
            vector_route.clone(),
            crate::vectors::active_entity_hashes(&vector_route)?
                .into_iter()
                .collect(),
        );
    }
    if active_by_route
        .get(&vector_route)
        .is_some_and(|active| active.contains(&(entity_id.to_string(), chunk_hash.to_string())))
    {
        entry.indexed_count = entry.indexed_count.saturating_add(1);
    }
    Ok(())
}

fn enqueue_reembed_routes(
    state: &Arc<SharedState>,
    buckets: &[Bucket],
    max_entities: Option<usize>,
) -> Result<usize> {
    let mut enqueued = 0usize;
    if buckets.contains(&Bucket::Knowledge) {
        let published = crate::server::routes::published_knowledge_for_embedding(state, None)?;
        for entry in &published {
            // Don't re-embed retired entries — `all_entries()` includes Deleted,
            // which would revive a forgotten entry in vector search. Mirror the
            // tantivy reindex's indexable filter (Active|Superseded only).
            if !crate::index::indexable_knowledge_entry(entry) {
                continue;
            }
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            let entity_id = crate::index::knowledge_entity_id(&entry.id);
            let chunk_hash = crate::index::knowledge_chunk_hash(entry);
            if crate::embed_queue::enqueue_knowledge(entry, &entity_id, &chunk_hash) {
                enqueued += 1;
            }
        }
        for item in state.roadmap.read().all_items() {
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            if matches!(item.status, crate::roadmap::RoadmapStatus::Rejected) {
                continue;
            }
            let entity_id = crate::index::roadmap_entity_id(&item.id);
            let chunk_hash = crate::index::roadmap_chunk_hash(item);
            if crate::embed_queue::enqueue_roadmap(item, &entity_id, &chunk_hash) {
                enqueued += 1;
            }
        }
    }
    if buckets.contains(&Bucket::Notes) {
        for note in state.notes.read().all() {
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            if crate::embed_queue::enqueue_note(note) {
                enqueued += 1;
            }
        }
    }
    if buckets.contains(&Bucket::Threads) {
        for thread in state.threads.read().all() {
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            if crate::embed_queue::enqueue_thread(thread) {
                enqueued += 1;
            }
        }
    }
    if buckets.contains(&Bucket::AgentManifest) {
        let remaining = max_entities.map(|max| max.saturating_sub(enqueued));
        enqueued += enqueue_agent_manifest_artifacts(state, remaining)?;
        if limit_reached(max_entities, enqueued) {
            return Ok(enqueued);
        }
    }
    if buckets.contains(&Bucket::Graph) {
        let remaining = max_entities.map(|max| max.saturating_sub(enqueued));
        enqueued += enqueue_graph_vertices(state, remaining)?;
        if limit_reached(max_entities, enqueued) {
            return Ok(enqueued);
        }
    }
    let doc_types = reembed_index_doc_types(buckets);
    if !doc_types.is_empty() {
        let remaining = max_entities.map(|max| max.saturating_sub(enqueued));
        let mut index_enqueued = 0usize;
        let mut index_docs_seen = 0usize;
        let mut visual_docs_seen = 0usize;
        let mut visual_docs_enqueued = 0usize;
        state
            .idx
            .read()
            .for_each_embedding_source_doc_for_doc_types(&doc_types, None, |doc| {
                if limit_reached(remaining, index_enqueued) {
                    return Ok(());
                }
                index_docs_seen += 1;
                // Classification must not reconstruct the chunk: decoding a
                // visual payload here allocated the full image for a counter,
                // and at 14k visual docs per sweep that was a multi-GB
                // allocation burst every interval while the docs themselves
                // were never enqueued from this pass.
                let visual = doc.doc_type == "project_file"
                    && crate::embed_queue::is_visual_chunk_kind(&doc.chunk_kind);
                if visual {
                    visual_docs_seen += 1;
                }
                let accepted = enqueue_reembed_index_doc(buckets, &doc);
                if accepted {
                    index_enqueued += 1;
                    if visual {
                        visual_docs_enqueued += 1;
                    }
                }
                Ok(())
            })?;
        tracing::info!(
            index_docs_seen,
            index_enqueued,
            visual_docs_seen,
            visual_docs_enqueued,
            "embedding rebuild index-source refill classified"
        );
        enqueued += index_enqueued;
    }
    Ok(enqueued)
}

// ===========================================================================
// Residue sweeper (gap-7323e96c, gap-d102fca9)
//
// A single manual bbox_reembed (or a first-time project index) enqueues one
// queue-capped wave: the enqueue pass fills each route to MAX_ROUTE_QUEUE_DEPTH
// and everything past the cap is rejected. Before this sweeper that rejection
// was a silent drop with no signal — the queue drained the accepted wave and
// idled at depth 0 while unembedded residue was never re-enqueued, so a route
// parked at `stalled` and the operator had to re-run bbox_reembed by hand.
//
// The sweeper is a daemon-side convergence loop that re-runs the existing
// enqueue pass until every non-guarded route has no enqueueable residue left.
// It wakes on a timer AND promptly when a route's queue drains to empty (the
// mark_success -> QUEUE_DRAIN_HOOK path), so bbox_reembed becomes "kick
// convergence now" without changing its API.
//
// Cost / termination invariants:
//   * The enqueue pass dedups already-embedded items at the queue
//     (should_embed vs the vector store), so a sweep over a converged corpus
//     costs zero provider calls.
//   * A pass runs only from a fully-idle sweepable state (no route draining),
//     so in-flight items are never re-enqueued as duplicates.
//   * Enqueueable residue excludes permanently dropped items (poison) and
//     unavailable routes (outages): the sweeper never re-hammers a
//     provider-rejected payload or a credential-missing route. Those keep
//     their existing stall/poison/unavailable health and are cleared by a
//     manual bbox_reembed.
//   * The transcript corpus is never auto-swept; only an explicit
//     bbox_reembed(route="transcripts", include_transcripts=true) touches it.

/// Wake signal shared by the queue-drain hook, the sweep timer, and a manual
/// bbox_reembed kick. Lives here (not in the queue crate) so the queue stays
/// free of daemon types; the queue reaches it only through the registered
/// `fn(&str)` drain hook.
static SWEEP_NOTIFY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

fn sweep_notify() -> &'static Arc<tokio::sync::Notify> {
    SWEEP_NOTIFY.get_or_init(|| Arc::new(tokio::sync::Notify::new()))
}

/// Registered into the queue as `QUEUE_DRAIN_HOOK`: fires when a route's
/// pending depth returns to zero so the sweeper refills the next wave
/// promptly. A plain `fn` (not a closure) because the hook slot is a bare
/// function pointer; it reaches the sweeper through the module-level Notify.
pub(crate) fn queue_drain_wake(_route: &str) {
    sweep_notify().notify_one();
}

const DEFAULT_EMBED_SWEEP_INTERVAL_SECS: u64 = 300;
/// Short boot delay before the first coverage probe. If vector warmup is still
/// active, the probe waits for the global store; the provider-capable queue is
/// installed before that store is published, so the pass can never enqueue
/// work into a non-persistent startup queue.
const EMBED_SWEEP_STARTUP_DELAY: Duration = Duration::from_secs(15);

/// `BLACKBOX_EMBED_SWEEP_INTERVAL_SECS` overrides the sweep timer (default
/// 300s). `0` disables the sweeper entirely (residue then converges only via
/// manual bbox_reembed). Mirrors the env-override convention the sibling
/// background maintenance tasks use (storage GC, account probes).
fn sweep_interval_from_env() -> Duration {
    let secs = std::env::var("BLACKBOX_EMBED_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_EMBED_SWEEP_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Every route the sweeper may drive: all buckets except the guarded
/// transcript corpus. Visual `visual:<kind>` routes need no entry here — they
/// ride the project_file scan whenever Code/Docs is enqueued.
fn sweepable_buckets() -> Vec<Bucket> {
    Bucket::ALL
        .iter()
        .copied()
        .filter(|bucket| *bucket != Bucket::Transcripts)
        .collect()
}

/// Enqueueable residue for one route: items in the source corpus that are not
/// yet embedded and are neither permanently dropped nor blocked by an
/// unavailable/mid-drain route. Zero for a route that is unavailable (leave
/// outages to their own health) or currently draining (wait for it to finish
/// rather than re-enqueue in-flight items).
fn route_enqueueable_residue(status: &queue::RouteStatus) -> u64 {
    if !status.available || status.queue_depth > 0 {
        return 0;
    }
    let source = status.source_count.unwrap_or(0);
    source
        .saturating_sub(status.indexed_count)
        .saturating_sub(status.dropped_count)
}

/// Aggregate residue + busy state across every sweepable route. Pure over a
/// status response so the decision can be tested without the corpus.
struct SweepSnapshot {
    /// Total enqueueable residue over available, idle routes.
    residue: u64,
    /// Any sweepable route currently draining a wave.
    busy: bool,
}

fn sweep_snapshot(response: &EmbedStatusResponse) -> SweepSnapshot {
    let mut residue = 0u64;
    let mut busy = false;
    for (route, status) in &response.routes {
        // The transcript corpus is guarded — never counted toward residue and
        // never allowed to mark the sweep "busy".
        if route == Bucket::Transcripts.as_str() {
            continue;
        }
        if status.queue_depth > 0 {
            busy = true;
        }
        residue = residue.saturating_add(route_enqueueable_residue(status));
    }
    SweepSnapshot { residue, busy }
}

fn total_capped_count(response: &EmbedStatusResponse) -> u64 {
    response
        .routes
        .iter()
        .filter(|(route, _)| *route != Bucket::Transcripts.as_str())
        .map(|(_, status)| status.capped_count)
        .sum()
}

/// One sweep's result, consumed by the wake decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SweepReport {
    /// Enqueueable residue observed this wake.
    enqueueable_residue: u64,
    /// Items admitted to a queue by this wake's enqueue pass.
    enqueued: usize,
    /// The pass hit a route's depth cap: residue remains for the next wave.
    capped: bool,
    /// The pass actually ran (idle state with residue).
    ran_pass: bool,
    /// A wave was still draining, so no pass ran this wake.
    busy: bool,
}

/// Whether the sweeper should wake on a queue-drain nudge (fast refill) or
/// back off to the plain timer. Pure so termination/no-hot-loop is unit
/// tested: a stuck pass (residue remains but nothing was admitted and no cap
/// was hit — i.e. un-enqueueable/poison residue) drops to timer-only so it
/// cannot spin on drain nudges.
fn sweep_should_listen_for_drain(report: &SweepReport) -> bool {
    if report.busy {
        // A wave is draining; its drain nudge is exactly what to wait for.
        return true;
    }
    if report.enqueueable_residue == 0 {
        // Converged: idle until a timer tick or a manual reembed kick.
        return true;
    }
    // Residue remains: fast-refill only if this pass made forward progress
    // (admitted a wave) or was capped (more waves to come). Otherwise the
    // residue is un-enqueueable and we back off to the slow timer.
    report.enqueued > 0 || report.capped
}

/// Run one sweep pass. Reads coverage + queue status, and if there is
/// enqueueable residue on an otherwise-idle sweepable route, re-runs the
/// existing enqueue pass (dedup + cap enforced by the queue). Synchronous
/// (store reads + enqueue); the caller runs it on the blocking pool.
fn run_sweep_once(state: &Arc<SharedState>) -> SweepReport {
    let buckets = sweepable_buckets();
    let response = match status_response_for_buckets(&state.corpus_stores(), &buckets) {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "embed residue sweep: coverage scan failed; skipping");
            return SweepReport::default();
        }
    };
    let snap = sweep_snapshot(&response);
    if snap.residue == 0 || snap.busy {
        return SweepReport {
            enqueueable_residue: snap.residue,
            enqueued: 0,
            capped: false,
            ran_pass: false,
            busy: snap.busy,
        };
    }
    let capped_before = total_capped_count(&response);
    let enqueued = match enqueue_reembed_routes(state, &buckets, None) {
        Ok(enqueued) => enqueued,
        Err(err) => {
            tracing::warn!(error = %err, "embed residue sweep: enqueue pass failed");
            0
        }
    };
    // Re-read the live queue status: the pass may have hit a route cap, which
    // is now counted (not silently dropped) in capped_count.
    let capped = total_capped_count(&status_response()) > capped_before;
    if enqueued > 0 || capped {
        tracing::info!(
            enqueued,
            capped,
            residue = snap.residue,
            "embed residue sweep: refilled queue"
        );
    }
    SweepReport {
        enqueueable_residue: snap.residue,
        enqueued,
        capped,
        ran_pass: true,
        busy: false,
    }
}

/// Start the residue sweeper background task and register the queue-drain wake
/// hook. Idempotent-safe registration; disabled when the interval env is 0.
pub(crate) fn spawn_embed_residue_sweeper(state: Arc<SharedState>) {
    let interval = sweep_interval_from_env();
    if interval.is_zero() {
        tracing::info!("embed residue sweeper: disabled (interval=0)");
        return;
    }
    // Wire the drain nudge so a finished wave refills promptly.
    queue::register_queue_drain_hook(queue_drain_wake);
    let notify = sweep_notify().clone();
    tracing::info!(
        interval_secs = interval.as_secs(),
        "embed residue sweeper: enabled"
    );
    tokio::spawn(async move {
        tokio::time::sleep(EMBED_SWEEP_STARTUP_DELAY).await;
        loop {
            let state_for_pass = state.clone();
            let report = match tokio::task::spawn_blocking(move || run_sweep_once(&state_for_pass))
                .await
            {
                Ok(report) => report,
                Err(err) => {
                    tracing::warn!(error = %err, "embed residue sweep task panicked; backing off");
                    SweepReport::default()
                }
            };
            if sweep_should_listen_for_drain(&report) {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(interval) => {}
                }
            } else {
                // Un-enqueueable residue (poison / retry-exhausted): back off
                // to the slow timer and let the existing stall/poison health
                // own the reporting. Ignoring the drain nudge here is what
                // prevents a hot loop on a route that cannot make progress.
                tokio::time::sleep(interval).await;
            }
        }
    });
}

fn enqueue_agent_manifest_artifacts(
    state: &Arc<SharedState>,
    max_entities: Option<usize>,
) -> Result<usize> {
    let catalog = state.artifacts.read();
    let entries = catalog.list(&crate::artifacts::ArtifactListParams {
        kind: Some(crate::artifacts::ArtifactKind::Agent),
        name: None,
        include_superseded: true,
    })?;
    let mut enqueued = 0usize;
    for entry in entries {
        if limit_reached(max_entities, enqueued) {
            break;
        }
        let Some(value) =
            catalog.load_artifact_value(crate::artifacts::ArtifactKind::Agent, &entry.name)?
        else {
            continue;
        };
        let manifest_value = value.get("manifest").unwrap_or(&value);
        let Ok(manifest) = serde_json::from_value::<
            crate::orchestration::agents::types::AgentManifest,
        >(manifest_value.clone()) else {
            continue;
        };
        let Ok(version) = entry.version.parse::<u32>() else {
            continue;
        };
        let agent = crate::orchestration::agents::types::AgentRef {
            name: entry.name,
            version,
        };
        enqueued += enqueue_agent_manifest(&agent, &manifest);
    }
    Ok(enqueued)
}

#[cfg(test)]
fn count_reembed_index_docs(buckets: &[Bucket], docs: &[EmbeddingSourceDoc]) -> usize {
    docs.iter()
        .filter(|doc| reembed_index_doc_bucket(doc).is_some_and(|bucket| buckets.contains(&bucket)))
        .count()
}

#[cfg(test)]
fn enqueue_reembed_index_docs(
    buckets: &[Bucket],
    docs: &[EmbeddingSourceDoc],
    max_entities: Option<usize>,
) -> usize {
    let mut enqueued = 0usize;
    for doc in docs {
        if limit_reached(max_entities, enqueued) {
            break;
        }
        if enqueue_reembed_index_doc(buckets, doc) {
            enqueued += 1;
        }
    }
    enqueued
}

fn enqueue_reembed_index_doc(buckets: &[Bucket], doc: &EmbeddingSourceDoc) -> bool {
    // Mirror of the coverage-side skip: the queue would reject the empty
    // text anyway; skipping here keeps reembed's enqueued count honest.
    if !queue::embeddable_text(&doc.content) {
        return false;
    }
    // Visual chunks piggyback on the project_file scan (see the matching
    // comment in `record_index_doc_coverage`) rather than gating on
    // `buckets`, since they have no `Bucket` of their own.
    if doc.doc_type == "project_file" && crate::embed_queue::is_visual_chunk_kind(&doc.chunk_kind) {
        let Some(chunk) = chunk_from_embedding_doc(doc) else {
            return false;
        };
        let Some(entity_id) = doc.entity_id.as_deref() else {
            return false;
        };
        return crate::embed_queue::enqueue_visual_project_file(&chunk, entity_id);
    }
    let Some(bucket) = reembed_index_doc_bucket(doc) else {
        return false;
    };
    if !buckets.contains(&bucket) {
        return false;
    }
    match bucket {
        Bucket::Code | Bucket::Docs => {
            let Some(chunk) = chunk_from_embedding_doc(doc) else {
                return false;
            };
            let Some(entity_id) = doc.entity_id.as_deref() else {
                return false;
            };
            // The stored `project` field is the display name after the P3-E
            // cut, so the backfill lane reproduces the same embedding input
            // the index-time enqueue produced. Reading it from the document
            // rather than re-resolving the catalog keeps this pass free of a
            // project-authority dependency.
            // Preserve the stored V2 identity. Reconstructing it from Chunk
            // would silently drop the snapshot id and deduplicate against a
            // stale V1 vector instead of embedding the active generation.
            crate::embed_queue::enqueue_project_file_as(&chunk, &doc.project, entity_id, bucket)
        }
        Bucket::Transcripts => {
            let chunk_hash = doc
                .chunk_hash
                .clone()
                .unwrap_or_else(|| crate::embed_queue::content_hash(&doc.content));
            crate::embed_queue::enqueue_transcript(
                &doc.account,
                &doc.session_id,
                doc.byte_offset,
                &doc.content,
                &chunk_hash,
            )
        }
        Bucket::GitMessage => {
            let (Some(entity_id), Some(chunk_hash)) = (&doc.entity_id, &doc.chunk_hash) else {
                return false;
            };
            crate::embed_queue::enqueue_git_message(entity_id, chunk_hash, &doc.content)
        }
        Bucket::Knowledge
        | Bucket::Notes
        | Bucket::Threads
        | Bucket::AgentManifest
        | Bucket::Graph => false,
    }
}

/// The graph route's backfill (unified-retrieval design 4.4): walk every
/// installed published graph view and enqueue each embed-eligible vertex's
/// composed projection. Graph vertices are NOT sourced from the word index:
/// the indexed document carries only the label and `index: text` values, so
/// the `embed: true` projection can only be rebuilt from the in-memory
/// accepted generation, the same source the install-time converge walks.
///
/// Also the exact reconciliation the install-time converge cannot do at
/// boot: every active graph vector on the route whose vertex is no longer
/// embed-eligible (removed while the daemon was down, graph unpublished,
/// annotation withdrawn) is tombstoned, so the partition converges to the
/// eligible set rather than merely hiding stale vectors at query time.
fn enqueue_graph_vertices(state: &Arc<SharedState>, max_entities: Option<usize>) -> Result<usize> {
    let router = EmbeddingRouter::load_default()?;
    let mut enqueued = 0usize;
    let mut eligible: HashSet<String> = HashSet::new();
    let mut vector_routes: std::collections::BTreeSet<String> = Default::default();
    let views = state.project_graph_views.read();
    for (project_id, view) in views.iter_published() {
        let project_id = project_id.as_str();
        for (graph_id, entry) in &view.graphs {
            let Some(graph) = entry.graph() else {
                continue;
            };
            let projections = bbox_project_graph::graph_embed_projections(graph);
            if projections.is_empty() {
                continue;
            }
            if let Ok((_, vector_route)) =
                router.queue_and_vector_route(Bucket::Graph, Some(project_id))
            {
                vector_routes.insert(vector_route);
            }
            for projection in projections {
                let entity_id = EntityRef::ProjectGraphVertex {
                    project_id: project_id.to_string(),
                    graph_id: graph_id.clone(),
                    vertex_id: projection.vertex_id.clone(),
                }
                .to_string();
                if !limit_reached(max_entities, enqueued)
                    && crate::embed_queue::enqueue_graph_vertex(project_id, &entity_id, &projection)
                {
                    enqueued += 1;
                }
                eligible.insert(entity_id);
            }
        }
    }
    drop(views);
    // Orphan sweep: only ids of this entity family, only on routes the graph
    // bucket actually maps to, so a partition shared with another bucket is
    // never touched beyond its graph rows.
    let mut orphans = Vec::new();
    for vector_route in &vector_routes {
        for entry in crate::vectors::iter_active(vector_route, None)? {
            if entry.entity_id.starts_with("project_graph_vertex:")
                && !eligible.contains(&entry.entity_id)
            {
                orphans.push(entry.entity_id);
            }
        }
    }
    orphans.sort();
    orphans.dedup();
    if !orphans.is_empty() {
        tracing::info!(
            orphans = orphans.len(),
            "graph embedding backfill tombstoning vectors whose vertices are no longer embed-eligible"
        );
        crate::embed_queue::tombstone_graph_vertices(&orphans);
    }
    Ok(enqueued)
}

fn limit_reached(max_entities: Option<usize>, enqueued: usize) -> bool {
    max_entities.is_some_and(|max| enqueued >= max)
}

fn reembed_index_doc_types(buckets: &[Bucket]) -> Vec<&'static str> {
    let mut doc_types = Vec::new();
    if buckets.contains(&Bucket::Code) || buckets.contains(&Bucket::Docs) {
        doc_types.push("project_file");
    }
    if buckets.contains(&Bucket::Transcripts) {
        doc_types.push("transcript");
    }
    if buckets.contains(&Bucket::GitMessage) {
        doc_types.push("commit");
    }
    doc_types
}

fn reembed_index_doc_bucket(doc: &EmbeddingSourceDoc) -> Option<Bucket> {
    match doc.doc_type.as_str() {
        "transcript" if !doc.session_id.is_empty() && !doc.content.is_empty() => {
            Some(Bucket::Transcripts)
        }
        "commit" if doc.chunk_kind == "git_message" && !doc.content.is_empty() => {
            Some(Bucket::GitMessage)
        }
        "knowledge" | "roadmap" => Some(Bucket::Knowledge),
        "project_file" => {
            // Mirror enqueue_project_file's routing EXACTLY (language /
            // chunk_kind on the stored doc, not path extension). The old
            // path rule only knew md/rst/txt and code-language extensions,
            // so chunks from the document chunkers (pdf_page,
            // office_section, spreadsheet_sheet, slide, web_section,
            // transcript_segment, notebook_cell) were invisible to
            // coverage AND to bbox_reembed backfill: index-time enqueues
            // dropped during vector-store warmup could never be repaired.
            if crate::embed_queue::is_code_chunk(doc.language.as_deref(), &doc.chunk_kind) {
                Some(Bucket::Code)
            } else {
                Some(Bucket::Docs)
            }
        }
        _ => None,
    }
}

fn chunk_from_embedding_doc(doc: &EmbeddingSourceDoc) -> Option<Chunk> {
    let entity = doc
        .entity_id
        .as_deref()
        .and_then(|id| EntityRef::parse(id).ok())?;
    let (project_id, rel_path_hash, chunk_hash, occurrence_idx) = match entity {
        EntityRef::ProjectFile {
            project_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        }
        | EntityRef::ProjectFileV2 {
            project_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
            ..
        } => (project_id, rel_path_hash, chunk_hash, occurrence_idx),
        _ => return None,
    };
    let byte_end = doc.byte_offset.saturating_add(doc.content.len() as u64);
    // Visual chunks (X-IMG's `image`, X-PDF's `pdf_figure`) encode their
    // VisualPayloadRef into `symbol` at chunk time (see `bbox_chunker::ximg`
    // and `bbox_chunker::pdf_figure`) precisely so this reconstruction
    // path, which rebuilds a Chunk from stored tantivy fields rather than
    // re-chunking the file, can recover it without a schema change.
    // Non-visual chunk kinds' `symbol` never happens to parse as a visual
    // ref (versioned prefix), so this is a safe no-op for every other
    // chunker.
    let visual_payload = crate::embed_queue::is_visual_chunk_kind(&doc.chunk_kind)
        .then(|| {
            doc.symbol
                .as_deref()
                .and_then(bbox_visual_store::VisualPayloadRef::decode)
        })
        .flatten();
    // P3-E: rehydrate the normalized RELATIVE path (governing section 10.2).
    // `relative_path` is the authority; `file_path` is the compat fallback for
    // a pre-bump document still in a segment mid-migration, and after the
    // paired bump it carries the same relative value anyway.
    let relative_path = if doc.relative_path.is_empty() {
        doc.file_path.clone()
    } else {
        doc.relative_path.clone()
    };
    Some(Chunk {
        project_id,
        file_path: PathBuf::from(&relative_path),
        rel_path_hash,
        chunk_kind: doc.chunk_kind.clone(),
        chunk_hash,
        occurrence_idx,
        language: doc.language.clone(),
        symbol: doc.symbol.clone(),
        symbol_exact: doc.symbol_exact.clone(),
        // The new chunk-metadata fields are not yet stored in tantivy
        // (lands in CN-D3) and not yet projected onto the document
        // model used here. Leave them None until the schema bump.
        symbol_kind: None,
        parent_kind: None,
        line_start: None,
        line_end: None,
        content: doc.content.clone(),
        byte_start: doc.byte_offset,
        byte_end,
        visual_payload,
    })
}

static CONTRADICTION_STATE: OnceLock<RwLock<Option<std::sync::Arc<SharedState>>>> = OnceLock::new();
static CONTRADICTION_THRESHOLD: OnceLock<RwLock<f32>> = OnceLock::new();
const DEFAULT_TIER0_COSINE_THRESHOLD: f32 = 0.85;

pub(crate) fn install_contradiction_state(state: std::sync::Arc<SharedState>) {
    *CONTRADICTION_STATE
        .get_or_init(|| RwLock::new(None))
        .write() = Some(state);
}

pub(crate) fn install_contradiction_threshold(threshold: f32) {
    *CONTRADICTION_THRESHOLD
        .get_or_init(|| RwLock::new(DEFAULT_TIER0_COSINE_THRESHOLD))
        .write() = threshold.clamp(0.0, 1.0);
}

fn contradiction_threshold() -> f32 {
    *CONTRADICTION_THRESHOLD
        .get_or_init(|| RwLock::new(DEFAULT_TIER0_COSINE_THRESHOLD))
        .read()
}

/// Coverage below this with an idle queue marks a route `stalled`: the
/// residue exists but nothing is enqueueing it, so it will never converge
/// without an explicit backfill (gap-b9d39c10 — git_message sat at 0%
/// coverage with queue_depth=0 and health=ok).
const STALLED_COVERAGE_THRESHOLD: f32 = 0.98;

pub(crate) fn status_response_for_buckets(
    stores: &crate::providers::CorpusStores<'_>,
    buckets: &[Bucket],
) -> Result<EmbedStatusResponse> {
    let mut response = status_response();
    let coverage = route_coverage(stores, buckets)?;
    for (route, counts) in coverage {
        let status = response.routes.entry(route).or_default();
        status.session_indexed_count = Some(status.indexed_count);
        status.indexed_count = counts.indexed_count;
        status.source_count = Some(counts.source_count);
        status.coverage_ratio = if counts.source_count == 0 {
            None
        } else {
            Some(counts.indexed_count as f32 / counts.source_count as f32)
        };
    }
    // Coverage seeds a `visual:<kind>` row via `.or_default()` whenever it
    // scans a matching chunk, independent of whether a queue worker for
    // that route has ever spawned in THIS process (workers spawn lazily on
    // first enqueue — `ensure_sender`). An unseeded row defaults to
    // `available: true` with provider/model/dim left `None`: cosmetically
    // broken (available=true but nothing describing what's available), not
    // functionally wrong. Visual routes are chunk-kind-keyed, not
    // `Bucket`-keyed, so they need their own metadata source; backfill from
    // `VisualRouteMeta` instead of leaving the queue-local snapshot as the
    // only source of truth.
    let router = EmbeddingRouter::load_default().unwrap_or_default();
    backfill_visual_route_metadata(&router, &mut response);
    queue::normalize_route_statuses(&mut response);
    apply_stall_health(&mut response);
    Ok(response)
}

/// Fills `provider`/`model`/`dim`/`endpoint_kind`/`output_dtype`/
/// `compatibility_family` on any `visual:<kind>` status row still missing
/// them, sourced from `EmbeddingRouter::visual_route`. Never overwrites a
/// row a live queue worker already populated (`provider_route_status`);
/// only backfills the gap left by coverage-only seeding.
fn backfill_visual_route_metadata(router: &EmbeddingRouter, response: &mut EmbedStatusResponse) {
    for (route, status) in response.routes.iter_mut() {
        let Some(kind) = route.strip_prefix("visual:") else {
            continue;
        };
        if status.provider.is_some() {
            continue;
        }
        if let Ok(Some(meta)) = router.visual_route(kind) {
            status.provider = Some(meta.provider_id);
            status.model = Some(meta.document_model);
            status.endpoint_kind = Some(crate::embed::EmbedEndpointKind::Multimodal);
            status.output_dtype = Some(meta.output_dtype.as_str().to_string());
            status.compatibility_family = Some(meta.compatibility_family);
            status.dim = Some(meta.dimensions);
        }
    }
}

/// Runs after `normalize_route_statuses` (which owns the error-driven
/// `ok`/`unavailable` states): an available route whose coverage sits under
/// the threshold with an EMPTY queue is not "ok" — it is residue that is
/// never being enqueued. Health only signalled errors before; convergence
/// failures were invisible (gap-b9d39c10).
fn apply_stall_health(response: &mut EmbedStatusResponse) {
    for (route, status) in response.routes.iter_mut() {
        if !status.available || status.queue_depth > 0 {
            continue;
        }
        let Some(ratio) = status.coverage_ratio else {
            continue;
        };
        if ratio < STALLED_COVERAGE_THRESHOLD {
            status.health = "stalled".into();
            // A nonzero dropped_count means the shortfall is poison —
            // payloads the provider permanently rejects (gap-e3e033ce) —
            // not un-enqueued residue. Reembed won't help; say so instead
            // of sending the operator to chase a backfill that can't close
            // the gap.
            status.health_reason = Some(if status.dropped_count > 0 {
                format!(
                    "coverage {ratio:.3} with idle queue; {} item(s) permanently dropped as unembeddable (provider-rejected payloads) — last: {}",
                    status.dropped_count,
                    status.last_dropped.as_deref().unwrap_or("(no detail)")
                )
            } else {
                format!(
                    "coverage {ratio:.3} with idle queue — unembedded residue not yet enqueued; the background residue sweeper converges it automatically, or run bbox_reembed(route=\"{route}\") to kick convergence now"
                )
            });
        }
    }
}

/// Structured status for `bbox_embed_status`. Exact coverage is an explicit
/// source-corpus scan; the default health path never performs that walk.
pub(crate) fn status_response_for_state(
    state: &SharedState,
    include_coverage: bool,
) -> Result<EmbedStatusResponse> {
    if !include_coverage {
        let mut response = status_response();
        let router = EmbeddingRouter::load_default().unwrap_or_default();
        backfill_visual_route_metadata(&router, &mut response);
        queue::normalize_route_statuses(&mut response);
        for status in response.routes.values_mut() {
            if status.coverage_ratio.is_none() {
                status.coverage_state = Some(
                    "guarded: exact coverage not requested; call bbox_embed_status(include_coverage=true) for a full source-corpus scan"
                        .into(),
                );
            }
        }
        return Ok(response);
    }
    const STATUS_COVERAGE_BUCKETS: &[Bucket] = &[
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::GitMessage,
        Bucket::Notes,
        Bucket::Threads,
        Bucket::AgentManifest,
        Bucket::Graph,
    ];
    let mut response =
        status_response_for_buckets(&state.corpus_stores(), STATUS_COVERAGE_BUCKETS)?;
    // Transcript coverage is deliberately not computed here (it is a heavy
    // corpus scan); say so explicitly instead of reporting a null that is
    // indistinguishable from a broken route (gap-b9d39c10).
    if let Some(status) = response.routes.get_mut(Bucket::Transcripts.as_str()) {
        if status.coverage_ratio.is_none() {
            status.coverage_state = Some(
                "guarded: coverage not computed (heavy corpus scan); rebuild via bbox_reembed(route=\"transcripts\", include_transcripts=true)"
                    .into(),
            );
        }
    }
    Ok(response)
}

/// Bounded health snapshot for `bbox_doctor`.
///
/// The full status surface intentionally computes exact coverage by walking
/// every embedding-source document. On a production corpus that took 142
/// seconds and made a nominal health read indistinguishable from a daemon
/// outage. Doctor reports live queue/provider state and marks coverage as
/// guarded; bbox_embed_status gets the exact scan only when its caller opts in.
pub(crate) fn status_response_for_doctor() -> EmbedStatusResponse {
    let mut response = status_response();
    let router = EmbeddingRouter::load_default().unwrap_or_default();
    backfill_visual_route_metadata(&router, &mut response);
    queue::normalize_route_statuses(&mut response);
    for status in response.routes.values_mut() {
        if status.coverage_ratio.is_none() {
            status.coverage_state = Some(
                "guarded: bbox_doctor does not run full-corpus embedding coverage; use bbox_embed_status for an explicit coverage scan"
                    .into(),
            );
        }
    }
    response
}

pub(crate) fn status_json_for_state(
    state: &SharedState,
    include_coverage: bool,
    debug: bool,
) -> Result<String> {
    let response = status_response_for_state(state, include_coverage)?;
    Ok(serde_json::to_string(&project_status_response(
        &response,
        include_coverage,
        debug,
    )?)?)
}

/// MCP presentation is separate from the diagnostic DTO used by doctor and
/// operator consumers. Keep failures and nonzero loss counters visible.
fn project_status_response(
    response: &EmbedStatusResponse,
    include_coverage: bool,
    debug: bool,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(response)?;
    if let Some(routes) = value["routes"].as_object_mut() {
        for route in routes
            .values_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            let exact_coverage = route
                .get("source_count")
                .is_some_and(|count| !count.is_null());
            if debug {
                route.insert(
                    "indexed_count_kind".into(),
                    json!(if exact_coverage {
                        "source_coverage"
                    } else {
                        "current_process_successes"
                    }),
                );
            } else if !exact_coverage {
                // Queue successes reset on daemon restart. They are not a
                // measurement of how many vectors exist in the corpus.
                if let Some(count) = route.remove("indexed_count") {
                    route.insert("session_indexed_count".into(), count);
                }
            }
        }
    }
    if debug {
        return Ok(value);
    }
    if let Some(routes) = value["routes"].as_object_mut() {
        for route in routes
            .values_mut()
            .filter_map(serde_json::Value::as_object_mut)
        {
            for key in [
                "provider",
                "model",
                "query_model",
                "endpoint_kind",
                "output_dtype",
                "compatibility_family",
                "dim",
                "queue_bytes",
            ] {
                route.remove(key);
            }
            if !include_coverage
                && route
                    .get("coverage_ratio")
                    .is_none_or(serde_json::Value::is_null)
            {
                route.remove("coverage_state");
            }
            route.retain(|key, value| {
                !value.is_null()
                    && !(matches!(
                        key.as_str(),
                        "retried_count" | "dropped_count" | "capped_count"
                    ) && value.as_u64() == Some(0))
            });
        }
    }
    if !include_coverage {
        value["coverage"] = json!({
            "requested": false,
            "hint": "include_coverage=true scans source coverage; transcript coverage remains excluded."
        });
    }
    Ok(value)
}

pub(crate) fn agent_manifest_embedding(
    agent: &AgentRef,
    manifest: &AgentManifest,
) -> AgentEmbedding {
    let route = crate::embed::EmbeddingRouter::load_default()
        .and_then(|router| router.route(Bucket::AgentManifest, None))
        .ok();
    let model = route
        .as_ref()
        .map(|route| route.document_model.clone())
        .unwrap_or_else(|| "unavailable".into());
    let vector_route = route.map(|route| route.vector_route_id());
    let primary = agent_component_entity_id(agent, AgentManifestComponent::Primary);
    let when_to_use = if manifest.when_to_use.is_empty() {
        None
    } else {
        Some(agent_component_entity_id(
            agent,
            AgentManifestComponent::WhenToUse,
        ))
    };
    let anti_patterns = if manifest.anti_patterns.is_empty() {
        None
    } else {
        Some(agent_component_entity_id(
            agent,
            AgentManifestComponent::AntiPatterns,
        ))
    };
    AgentEmbedding {
        model,
        computed_at: crate::util::now_iso(),
        vector_ref: primary.clone(),
        vector_route,
        components: AgentEmbeddingComponents {
            primary,
            when_to_use,
            anti_patterns,
        },
    }
}

pub(crate) fn enqueue_agent_manifest(agent: &AgentRef, manifest: &AgentManifest) -> usize {
    let mut enqueued = 0usize;
    for component in agent_manifest_components(manifest) {
        if crate::embed_queue::enqueue(EmbedRequest {
            bucket: Bucket::AgentManifest,
            project_id: None,
            entity_id: agent_component_entity_id(agent, component.kind),
            chunk_hash: content_hash(&component.text),
            text: component.text,
            visual_kind: None,
            visual_payload: None,
        }) {
            enqueued += 1;
        }
    }
    enqueued
}

pub(crate) fn agent_component_entity_id(
    agent: &AgentRef,
    component: AgentManifestComponent,
) -> String {
    format!(
        "agent_embed:{}:v{}:{}",
        agent.name,
        agent.version,
        component.as_str()
    )
}

pub(crate) fn parse_agent_component_entity_id(
    entity_id: &str,
) -> Option<(AgentRef, AgentManifestComponent)> {
    let (name, version, component) =
        crate::embed_queue::parse_agent_component_entity_id_parts(entity_id)?;
    Some((
        AgentRef { name, version },
        AgentManifestComponent::parse(&component)?,
    ))
}

pub(crate) fn agent_component_hash(
    manifest: &AgentManifest,
    component: AgentManifestComponent,
) -> Option<String> {
    let text = agent_manifest_component_text(manifest, component)?;
    Some(content_hash(&text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AgentManifestComponent {
    Primary,
    WhenToUse,
    AntiPatterns,
}

impl AgentManifestComponent {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::WhenToUse => "when_to_use",
            Self::AntiPatterns => "anti_patterns",
        }
    }

    pub(crate) fn parse(input: &str) -> Option<Self> {
        match input {
            "primary" => Some(Self::Primary),
            "when_to_use" => Some(Self::WhenToUse),
            "anti_patterns" => Some(Self::AntiPatterns),
            _ => None,
        }
    }
}

struct AgentComponentText {
    kind: AgentManifestComponent,
    text: String,
}

fn agent_manifest_components(manifest: &AgentManifest) -> Vec<AgentComponentText> {
    let mut components = Vec::new();
    for kind in [
        AgentManifestComponent::Primary,
        AgentManifestComponent::WhenToUse,
        AgentManifestComponent::AntiPatterns,
    ] {
        if let Some(text) = agent_manifest_component_text(manifest, kind) {
            components.push(AgentComponentText { kind, text });
        }
    }
    components
}

fn agent_manifest_component_text(
    manifest: &AgentManifest,
    component: AgentManifestComponent,
) -> Option<String> {
    match component {
        AgentManifestComponent::Primary => Some(format!(
            "description: {}\nwhen_to_use:\n{}\nanti_patterns:\n{}",
            manifest.description,
            manifest.when_to_use.join("\n"),
            manifest.anti_patterns.join("\n")
        )),
        AgentManifestComponent::WhenToUse => {
            if manifest.when_to_use.is_empty() {
                None
            } else {
                Some(manifest.when_to_use.join("\n"))
            }
        }
        AgentManifestComponent::AntiPatterns => {
            if manifest.anti_patterns.is_empty() {
                None
            } else {
                Some(manifest.anti_patterns.join("\n"))
            }
        }
    }
}

pub(crate) fn maybe_detect_knowledge_contradiction(
    request: &EmbedRequest,
    vector_route: &str,
    vector: &[f32],
) {
    if request.bucket != Bucket::Knowledge {
        return;
    }
    let Some(state) = CONTRADICTION_STATE
        .get_or_init(|| RwLock::new(None))
        .read()
        .clone()
    else {
        return;
    };
    let Some(entry_a) = request.entity_id.strip_prefix("knowledge:") else {
        return;
    };
    let hits = match crate::vectors::search(vector_route, vector, 5) {
        Ok(hits) => hits,
        Err(err) => {
            tracing::debug!(error = %err, "knowledge contradiction nearest-neighbor scan failed");
            return;
        }
    };
    let kb = state.kb.read();
    let Some(source) = kb.entry(entry_a).cloned() else {
        return;
    };
    let threshold = contradiction_threshold();
    let Some((entry_b, cosine)) = hits.into_iter().find_map(|hit| {
        let cosine = 1.0 - hit.distance;
        if hit.id == request.entity_id || cosine < threshold {
            return None;
        }
        let id = hit.id.strip_prefix("knowledge:")?;
        let target = kb.entry(id)?.clone();
        if supersession_related(&source, &target) {
            return None;
        }
        Some((target, cosine))
    }) else {
        return;
    };
    drop(kb);

    let payload = json!({
        "entry_a": format!("knowledge:{}", source.id),
        "entry_b": format!("knowledge:{}", entry_b.id),
        "cosine": cosine,
        "vector_route": vector_route,
    });
    if state
        .workflow_registry
        .read()
        .contains_key("contradiction-review-arc")
    {
        let state_for_task = state.clone();
        let mut initial_vars = serde_json::Map::new();
        initial_vars.insert("entry_a".into(), json!(format!("knowledge:{}", source.id)));
        initial_vars.insert("entry_b".into(), json!(format!("knowledge:{}", entry_b.id)));
        initial_vars.insert("cosine".into(), json!(cosine));
        tokio::spawn(async move {
            let _ = dispatch_routing_verdict_direct(
                state_for_task,
                "contradiction-detected",
                RoutingVerdict::StartArc {
                    workflow: "contradiction-review-arc".into(),
                    initial_vars,
                },
                payload,
            )
            .await;
        });
    } else {
        let project = source.project.clone().or(entry_b.project.clone());
        let body = format!(
            "Tier-0 contradiction detected between knowledge:{} and knowledge:{} (cosine {:.3}), but contradiction-review-arc is not installed.",
            source.id, entry_b.id, cosine
        );
        if let Err(err) = state.notes.write().create(&NoteParams {
            project_id: None,
            kind: "surprise".into(),
            body,
            task_id: None,
            session_id: None,
            project,
            thread_id: None,
            provider: None,
            bro: None,
        }) {
            tracing::warn!(error = %err, "failed to surface contradiction fallback note");
        }
    }
}

fn supersession_related(a: &KnowledgeEntry, b: &KnowledgeEntry) -> bool {
    a.supersedes.as_deref() == Some(b.id.as_str()) || b.supersedes.as_deref() == Some(a.id.as_str())
}

#[cfg(test)]
mod tests {
    #[test]
    fn embed_status_summary_keeps_failures_losses_and_explicit_coverage() {
        let response = super::EmbedStatusResponse {
            routes: std::collections::BTreeMap::from([(
                "docs".into(),
                super::queue::RouteStatus {
                    health: "degraded".into(),
                    health_reason: Some("permanent drops".into()),
                    provider: Some("test-provider".into()),
                    dropped_count: 2,
                    capped_count: 3,
                    last_dropped: Some("unsupported input".into()),
                    coverage_state: Some("not computed".into()),
                    ..Default::default()
                },
            )]),
        };
        let summary = super::project_status_response(&response, false, false).unwrap();
        let row = &summary["routes"]["docs"];
        assert_eq!(row["health_reason"], "permanent drops");
        assert_eq!(row["dropped_count"], 2);
        assert_eq!(row["capped_count"], 3);
        assert_eq!(row["last_dropped"], "unsupported input");
        assert_eq!(row["queue_depth"], 0);
        assert_eq!(row["session_indexed_count"], 0);
        assert!(row.get("indexed_count").is_none());
        for key in [
            "provider",
            "coverage_state",
            "coverage_ratio",
            "last_error",
            "retried_count",
        ] {
            assert!(row.get(key).is_none(), "unexpected summary field: {key}");
        }
        assert_eq!(summary["coverage"]["requested"], false);
        let coverage = super::project_status_response(&response, true, false).unwrap();
        assert_eq!(coverage["routes"]["docs"]["coverage_state"], "not computed");
        let debug = super::project_status_response(&response, false, true).unwrap();
        assert_eq!(debug["routes"]["docs"]["provider"], "test-provider");
        assert_eq!(
            debug["routes"]["docs"]["indexed_count_kind"],
            "current_process_successes"
        );
        let mut measured = response;
        measured.routes.get_mut("docs").unwrap().source_count = Some(5);
        measured.routes.get_mut("docs").unwrap().indexed_count = 3;
        let coverage = super::project_status_response(&measured, true, false).unwrap();
        assert_eq!(coverage["routes"]["docs"]["indexed_count"], 3);
    }
    use super::*;

    use crate::embed_queue::thread_chunk_hash;
    use crate::vectors::{VectorStore, install_test_global};
    use bbox_threads::threads::ThreadParams;

    fn partition_info(
        route: &str,
        days_old: i64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> crate::vectors::PartitionInfo {
        crate::vectors::PartitionInfo {
            route: route.to_string(),
            dims: Some(1024),
            active_count: Some(42),
            last_write: Some(now - chrono::Duration::days(days_old)),
            disk_bytes: 1000,
        }
    }

    #[test]
    fn embed_partitions_list_marks_mapped_and_orphaned() {
        let router = EmbeddingRouter::default();
        let now = chrono::Utc::now();
        let live = router
            .route(Bucket::Knowledge, None)
            .unwrap()
            .vector_route_id();
        let infos = vec![
            partition_info(&live, 1, now),
            partition_info("voyage-old-model-1024-deadbeef", 90, now),
        ];
        let params = EmbedPartitionsParams {
            action: Some("list".into()),
            older_than_days: None,
            apply: false,
            route: None,
        };
        let rendered =
            embed_partitions_with(&params, &router, infos, now, |_| unreachable!()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let rows = value["partitions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let live_row = rows.iter().find(|row| row["route"] == live).unwrap();
        assert_eq!(live_row["mapped"], true);
        assert_eq!(live_row["document_model"], "voyage-code-3");
        assert_eq!(live_row["compatibility_family"], "voyage-code-3:1024:float");
        assert!(
            live_row["mapped_buckets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|bucket| bucket == "knowledge")
        );
        let orphan = rows.iter().find(|row| row["mapped"] == false).unwrap();
        assert_eq!(orphan["route"], "voyage-old-model-1024-deadbeef");
        assert!(orphan["document_model"].is_null());
    }

    #[test]
    fn embed_partitions_list_maps_and_protects_visual_routes() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
image = "voyage_visual"
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let (visual_route, representative_kind, _) = router
            .configured_visual_routes()
            .into_iter()
            .next()
            .expect("configured visual partition");
        let now = chrono::Utc::now();
        let params = EmbedPartitionsParams {
            action: Some("prune".into()),
            older_than_days: Some(30),
            apply: true,
            route: None,
        };
        let mut removed = Vec::new();
        let rendered = embed_partitions_with(
            &params,
            &router,
            vec![partition_info(&visual_route, 365, now)],
            now,
            |route| {
                removed.push(route.to_string());
                Ok(true)
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let row = &value["partitions"][0];

        assert_eq!(row["mapped"], true);
        assert_eq!(row["provider"], "voyage_visual");
        assert_eq!(row["endpoint_kind"], "multimodal");
        assert_eq!(row["document_model"], "voyage-multimodal-3.5");
        assert_eq!(
            row["mapped_buckets"][0],
            format!("visual:{representative_kind}")
        );
        assert!(
            removed.is_empty(),
            "a mapped visual partition is not prunable"
        );
        assert!(value["prune_candidates"].as_array().unwrap().is_empty());
    }

    #[test]
    fn scrub_plan_classifies_and_only_mismatches_are_candidates() {
        let entities = vec![
            "project_file:p1:f:h:0".to_string(),
            "project_file:p1:f:h:1".to_string(),
            "project_file:p1:gone:h:2".to_string(),
            "knowledge:abcd1234".to_string(),
        ];
        let (mismatched, matched, missing, foreign) =
            partition_scrub_plan(&entities, |entity| match entity {
                "project_file:p1:f:h:0" => ScrubClass::Matched,
                "project_file:p1:f:h:1" => ScrubClass::Mismatched,
                "project_file:p1:gone:h:2" => ScrubClass::Missing,
                _ => ScrubClass::Foreign,
            });
        assert_eq!(mismatched, vec!["project_file:p1:f:h:1".to_string()]);
        assert_eq!((matched, missing, foreign), (1, 1, 1));
    }

    #[test]
    fn scrub_recognizes_legacy_and_collected_project_file_entities() {
        assert_eq!(
            project_file_project_id("project_file:p1:pathhash:chunkhash:0").as_deref(),
            Some("p1")
        );
        assert_eq!(
            project_file_project_id(
                "project_file_v2:p2:collected-0123456789abcdef:pathhash:chunkhash:0"
            )
            .as_deref(),
            Some("p2")
        );
        assert_eq!(project_file_project_id("knowledge:abcd1234"), None);
    }

    #[test]
    fn embed_partitions_prune_requires_age_threshold() {
        let router = EmbeddingRouter::default();
        let params = EmbedPartitionsParams {
            action: Some("prune".into()),
            older_than_days: None,
            apply: false,
            route: None,
        };
        let err = embed_partitions_with(
            &params,
            &router,
            Vec::new(),
            chrono::Utc::now(),
            |_| unreachable!(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("older_than_days"));
    }

    #[test]
    fn embed_partitions_prune_dry_run_deletes_nothing() {
        let router = EmbeddingRouter::default();
        let now = chrono::Utc::now();
        let infos = vec![partition_info("voyage-old-model-1024-deadbeef", 90, now)];
        let params = EmbedPartitionsParams {
            action: Some("prune".into()),
            older_than_days: Some(30),
            apply: false,
            route: None,
        };
        let rendered = embed_partitions_with(&params, &router, infos, now, |_| {
            panic!("dry run must not delete")
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["dry_run"], true);
        assert_eq!(
            value["prune_candidates"][0],
            "voyage-old-model-1024-deadbeef"
        );
        assert!(value["pruned"].as_array().unwrap().is_empty());
    }

    #[test]
    fn embed_partitions_prune_apply_deletes_only_old_orphans() {
        let router = EmbeddingRouter::default();
        let now = chrono::Utc::now();
        let live = router
            .route(Bucket::Knowledge, None)
            .unwrap()
            .vector_route_id();
        // A mapped-but-ancient partition, an old orphan, and a fresh orphan:
        // only the old orphan may go.
        let infos = vec![
            partition_info(&live, 365, now),
            partition_info("voyage-old-model-1024-deadbeef", 90, now),
            partition_info("voyage-new-model-1024-cafebabe", 2, now),
        ];
        let params = EmbedPartitionsParams {
            action: Some("prune".into()),
            older_than_days: Some(30),
            apply: true,
            route: None,
        };
        let mut removed = Vec::new();
        let rendered = embed_partitions_with(&params, &router, infos, now, |route| {
            removed.push(route.to_string());
            Ok(true)
        })
        .unwrap();
        assert_eq!(removed, vec!["voyage-old-model-1024-deadbeef"]);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["dry_run"], false);
        assert_eq!(value["pruned"][0], "voyage-old-model-1024-deadbeef");
        let skipped = value["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["route"], "voyage-new-model-1024-cafebabe");
    }

    #[test]
    fn reembed_route_validation_accepts_all_and_rejects_unknown() {
        assert_eq!(buckets_for_reembed_route("all").unwrap(), Bucket::ALL);
        assert_eq!(
            buckets_for_reembed_route("knowledge").unwrap(),
            vec![Bucket::Knowledge]
        );
        assert_eq!(
            buckets_for_reembed_route("threads").unwrap(),
            vec![Bucket::Threads]
        );
        let err = buckets_for_reembed_route("missing").unwrap_err();
        assert!(err.to_string().contains("unknown embedding route"));
    }

    /// `backfill` is the nightly residue-convergence sweep: every bucket
    /// except the guarded transcript corpus (gap-b9d39c10).
    #[test]
    fn reembed_backfill_route_covers_all_but_transcripts() {
        let buckets = buckets_for_reembed_route("backfill").unwrap();
        assert!(!buckets.contains(&Bucket::Transcripts));
        assert_eq!(buckets.len(), Bucket::ALL.len() - 1);
        for bucket in Bucket::ALL {
            if bucket != Bucket::Transcripts {
                assert!(buckets.contains(&bucket), "missing {bucket:?}");
            }
        }
    }

    /// An available route with residue (coverage under threshold) and an
    /// EMPTY queue is `stalled`, not `ok` — that state previously read as
    /// healthy while git_message sat at 0% forever (gap-b9d39c10). A busy
    /// queue, full coverage, or an error-driven `unavailable` all keep
    /// their existing health.
    #[test]
    fn stall_health_flags_low_coverage_with_idle_queue() {
        use crate::embed::queue::RouteStatus;

        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response.routes.insert(
            "git_message".into(),
            RouteStatus {
                coverage_ratio: Some(0.0),
                ..Default::default()
            },
        );
        response.routes.insert(
            "notes".into(),
            RouteStatus {
                coverage_ratio: Some(0.684),
                queue_depth: 120,
                ..Default::default()
            },
        );
        response.routes.insert(
            "code".into(),
            RouteStatus {
                coverage_ratio: Some(1.0),
                ..Default::default()
            },
        );
        response.routes.insert(
            "knowledge".into(),
            RouteStatus {
                available: false,
                health: "unavailable".into(),
                health_reason: Some("credentials".into()),
                coverage_ratio: Some(0.2),
                ..Default::default()
            },
        );
        apply_stall_health(&mut response);

        let git = &response.routes["git_message"];
        assert_eq!(git.health, "stalled");
        let git_reason = git.health_reason.as_deref().unwrap();
        assert!(
            git_reason.contains("bbox_reembed"),
            "still names the manual kick: {git_reason}"
        );
        assert!(
            git_reason.contains("sweeper"),
            "reflects automatic convergence, not the retired nightly backfill: {git_reason}"
        );
        assert!(
            !git_reason.contains("nightly"),
            "misleading nightly-backfill phrasing removed: {git_reason}"
        );
        assert_eq!(
            response.routes["notes"].health, "ok",
            "busy queue means residue is draining"
        );
        assert_eq!(response.routes["code"].health, "ok");
        assert_eq!(
            response.routes["knowledge"].health, "unavailable",
            "error-driven health wins over stall detection"
        );
    }

    /// A stalled route whose shortfall is poison (dropped_count > 0) must
    /// say so rather than send the operator to chase a backfill that can't
    /// close the gap (gap-e3e033ce).
    #[test]
    fn stall_reason_distinguishes_poison_drops_from_unenqueued_residue() {
        use crate::embed::queue::RouteStatus;

        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response.routes.insert(
            "code".into(),
            RouteStatus {
                coverage_ratio: Some(0.95),
                dropped_count: 5,
                last_dropped: Some("entity_id=project_file:abc: HTTP 400".into()),
                ..Default::default()
            },
        );
        apply_stall_health(&mut response);

        let code = &response.routes["code"];
        assert_eq!(code.health, "stalled");
        let reason = code.health_reason.as_deref().unwrap();
        assert!(reason.contains("permanently dropped"), "reason: {reason}");
        assert!(reason.contains("HTTP 400"), "names the cause: {reason}");
        assert!(
            !reason.contains("bbox_reembed"),
            "must not send the operator to a backfill that can't help: {reason}"
        );
    }

    #[test]
    fn reembed_empty_index_doc_enumeration_counts_zero() {
        assert_eq!(count_reembed_index_docs(&[Bucket::Code], &[]), 0);
        assert_eq!(count_reembed_index_docs(&Bucket::ALL, &[]), 0);
    }

    #[test]
    fn reembed_index_enqueue_counts_only_queue_accepted_items() {
        let docs = vec![
            EmbeddingSourceDoc {
                doc_type: "transcript".into(),
                account: "claude".into(),
                session_id: "s1".into(),
                project: String::new(),
                file_path: String::new(),
                relative_path: String::new(),
                byte_offset: 0,
                chunk_kind: String::new(),
                language: None,
                symbol: None,
                symbol_exact: None,
                chunk_hash: Some("h1".into()),
                entity_id: None,
                content: "one".into(),
            },
            EmbeddingSourceDoc {
                doc_type: "transcript".into(),
                account: "claude".into(),
                session_id: "s2".into(),
                project: String::new(),
                file_path: String::new(),
                relative_path: String::new(),
                byte_offset: 0,
                chunk_kind: String::new(),
                language: None,
                symbol: None,
                symbol_exact: None,
                chunk_hash: Some("h2".into()),
                entity_id: None,
                content: "two".into(),
            },
        ];
        assert_eq!(
            enqueue_reembed_index_docs(&[Bucket::Transcripts], &docs, Some(1)),
            0,
            "an uninstalled or dedup-skipped queue item must not consume the reembed cap"
        );
    }

    #[test]
    fn reembed_index_doc_types_only_selects_needed_sources() {
        assert_eq!(
            reembed_index_doc_types(&[Bucket::Knowledge]),
            Vec::<&str>::new()
        );
        assert_eq!(
            reembed_index_doc_types(&[Bucket::Code]),
            vec!["project_file"]
        );
        assert_eq!(
            reembed_index_doc_types(&[Bucket::Code, Bucket::Docs, Bucket::GitMessage]),
            vec!["project_file", "commit"]
        );
    }

    #[test]
    fn embed_status_reports_thread_coverage_from_vector_store() {
        let tmp = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(tmp.path());
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store.clone());
        let _router_guard = crate::embed::install_test_router(EmbeddingRouter::default());
        let created = state
            .threads
            .write()
            .thread(&ThreadParams {
                action: "open".into(),
                name: Some("coverage-thread".into()),
                id: None,
                topic: Some("status coverage thread".into()),
                project: Some("/repo".into()),
                project_id: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: Some("handoff marker".into()),
                note: Some("note marker".into()),
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("investigation".into()),
                origin: None,
            })
            .unwrap();
        let thread_id = regex::Regex::new(r"thread-[0-9a-f]{8}")
            .unwrap()
            .find(&created)
            .unwrap()
            .as_str()
            .to_string();
        let thread = state
            .threads
            .read()
            .all()
            .iter()
            .find(|thread| thread.id == thread_id)
            .unwrap()
            .clone();
        let route = EmbeddingRouter::default()
            .route(Bucket::Threads, None)
            .unwrap()
            .vector_route_id();
        let entity_id = EntityRef::Thread { thread_id }.to_string();
        store
            .upsert(
                &route,
                &entity_id,
                &thread_chunk_hash(&thread),
                vec![1.0, 0.0],
            )
            .unwrap();

        let status =
            status_response_for_buckets(&state.corpus_stores(), &[Bucket::Threads]).unwrap();
        let threads = status.routes.get("threads").unwrap();
        assert_eq!(threads.source_count, Some(1));
        assert_eq!(threads.indexed_count, 1);
        assert_eq!(threads.coverage_ratio, Some(1.0));
    }

    fn image_embedding_source_doc() -> EmbeddingSourceDoc {
        let payload = bbox_visual_store::VisualPayloadRef {
            content_hash: "deadbeef".into(),
            media_type: "image/png".into(),
            byte_len: 4096,
        };
        EmbeddingSourceDoc {
            doc_type: "project_file".into(),
            account: String::new(),
            session_id: String::new(),
            project: "proj1234".into(),
            file_path: "assets/figure.png".into(),
            relative_path: "assets/figure.png".into(),
            byte_offset: 0,
            chunk_kind: "image".into(),
            language: None,
            symbol: Some(payload.encode()),
            symbol_exact: None,
            chunk_hash: Some("f".repeat(64)),
            entity_id: Some(format!(
                "project_file_v2:proj1234:collected-{}:abcd1234:{}:0",
                "a".repeat(32),
                "f".repeat(64)
            )),
            content: "figure".into(),
        }
    }

    /// `chunk_from_embedding_doc` decodes the VisualPayloadRef X-IMG
    /// encoded into `symbol` back out: the round trip the backfill path
    /// depends on since it never re-chunks the source file.
    #[test]
    fn chunk_from_embedding_doc_decodes_visual_payload_from_symbol() {
        let doc = image_embedding_source_doc();
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        let payload = chunk
            .visual_payload
            .expect("visual payload decoded from symbol");
        assert_eq!(payload.content_hash, "deadbeef");
        assert_eq!(payload.media_type, "image/png");
        assert_eq!(payload.byte_len, 4096);
    }

    #[test]
    fn chunk_from_embedding_doc_decodes_visual_payload_from_collected_v2_entity() {
        let mut doc = image_embedding_source_doc();
        doc.entity_id = Some(format!(
            "project_file_v2:proj1234:collected-{}:abcd1234:{}:0",
            "a".repeat(32),
            "f".repeat(64)
        ));

        let chunk = chunk_from_embedding_doc(&doc).expect("collected V2 visual source doc");
        assert_eq!(chunk.project_id, "proj1234");
        assert_eq!(
            chunk
                .visual_payload
                .expect("visual payload decoded from stored symbol")
                .content_hash,
            "deadbeef"
        );
    }

    /// A non-visual chunk kind's `symbol` never decodes as a visual ref
    /// (versioned prefix), even when it happens to be `Some`.
    #[test]
    fn chunk_from_embedding_doc_never_decodes_visual_payload_for_text_kinds() {
        let mut doc = image_embedding_source_doc();
        doc.chunk_kind = "doc_section".into();
        doc.symbol = Some("KnowledgeStore".into());
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        assert_eq!(chunk.visual_payload, None);
    }

    /// Coverage: a visual project_file doc counts against the
    /// `visual:<kind>` route (chunk-kind-keyed), not a `Bucket` route:
    /// reached whenever the project_file scan runs (Code or Docs
    /// requested), independent of which text bucket was asked for.
    #[test]
    fn visual_chunks_ride_the_project_file_scan_into_the_visual_route() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
image = "voyage_visual"
"#,
        )
        .unwrap();
        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Docs],
            &image_embedding_source_doc(),
        )
        .unwrap();
        let entry = coverage.get("visual:image").expect("visual route counted");
        assert_eq!(entry.source_count, 1);
        assert_eq!(entry.indexed_count, 0, "vector store has nothing yet");
    }

    fn code_embedding_source_doc() -> EmbeddingSourceDoc {
        EmbeddingSourceDoc {
            doc_type: "project_file".into(),
            account: "project_file".into(),
            session_id: String::new(),
            // Post-P3-E: the DISPLAY NAME, which is also the backfill lane's
            // prepend value.
            project: "acme-service".into(),
            file_path: "src/helper.rs".into(),
            relative_path: "src/helper.rs".into(),
            byte_offset: 0,
            chunk_kind: "code_block".into(),
            language: Some("rust".into()),
            symbol: Some("Helper".into()),
            symbol_exact: Some("crate::Helper".into()),
            chunk_hash: Some("f".repeat(64)),
            entity_id: Some(format!(
                "project_file_v2:proj1234:collected-{}:abcd1234:{}:0",
                "a".repeat(32),
                "f".repeat(64)
            )),
            content: "pub struct Helper;".into(),
        }
    }

    fn code_route(router: &EmbeddingRouter) -> (String, String) {
        router
            .queue_and_vector_route(Bucket::Code, Some("proj1234"))
            .unwrap()
    }

    /// P3-E embed row: the Code/Docs coverage arm applies the SAME envelope the
    /// enqueue applies, so coverage converges to full after the version bump
    /// with zero phantom residue. A raw-hash comparison here would read a
    /// permanent zero, masking real embedding outages and turning every residue
    /// sweep into full-corpus churn.
    #[test]
    fn code_coverage_converges_to_full_against_the_enveloped_vector_hash() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let doc = code_embedding_source_doc();
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        let entity_id = doc.entity_id.clone().unwrap();
        let (_queue_route, vector_route) = code_route(&router);

        // The worker stores the vector under the ENVELOPE hash, exactly as the
        // enqueue keyed it.
        crate::vectors::upsert(
            &vector_route,
            &entity_id,
            &crate::embed_queue::project_file_text_content_hash(&chunk.chunk_hash),
            vec![0.5; 8],
        )
        .unwrap();

        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Code],
            &doc,
        )
        .unwrap();
        let entry = coverage
            .values()
            .find(|entry| entry.source_count > 0)
            .expect("the code route was counted");
        assert_eq!(entry.source_count, 1);
        assert_eq!(
            entry.indexed_count, 1,
            "coverage must be full, not a phantom zero"
        );
    }

    /// P3-E embed row: a vector stored under the PRE-bump raw hash reads as
    /// uncovered, which is exactly the dedup miss that drives the one-time
    /// re-embed. The same assertion is the regression guard against someone
    /// "fixing" the coverage arm by dropping the envelope.
    #[test]
    fn a_pre_bump_raw_hash_vector_reads_as_uncovered() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let doc = code_embedding_source_doc();
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        let entity_id = doc.entity_id.clone().unwrap();
        let (_queue_route, vector_route) = code_route(&router);
        crate::vectors::upsert(&vector_route, &entity_id, &chunk.chunk_hash, vec![0.5; 8]).unwrap();

        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Code],
            &doc,
        )
        .unwrap();
        let entry = coverage
            .values()
            .find(|entry| entry.source_count > 0)
            .expect("the code route was counted");
        assert_eq!(entry.indexed_count, 0);
    }

    /// P3-E embed row: the post-bump vector REPLACES the pre-bump one rather
    /// than duplicating it. The store keeps one active entry per
    /// `(route, entity_id)`, so no duplicate hit can surface during the
    /// one-time re-embed.
    #[test]
    fn the_post_bump_vector_replaces_rather_than_duplicates() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let doc = code_embedding_source_doc();
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        let entity_id = doc.entity_id.clone().unwrap();
        let (_queue_route, vector_route) = code_route(&router);
        let enveloped = crate::embed_queue::project_file_text_content_hash(&chunk.chunk_hash);

        crate::vectors::upsert(&vector_route, &entity_id, &chunk.chunk_hash, vec![0.1; 8]).unwrap();
        crate::vectors::upsert(&vector_route, &entity_id, &enveloped, vec![0.2; 8]).unwrap();

        let active = crate::vectors::active_entity_hashes(&vector_route).unwrap();
        let for_entity = active
            .iter()
            .filter(|(id, _)| id == &entity_id)
            .collect::<Vec<_>>();
        assert_eq!(
            for_entity.len(),
            1,
            "one active entry per entity id: {for_entity:?}"
        );
        assert_eq!(for_entity[0].1, enveloped);
    }

    #[test]
    fn legacy_project_file_vector_does_not_cover_collected_v2_source() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let doc = code_embedding_source_doc();
        let chunk = chunk_from_embedding_doc(&doc).unwrap();
        let legacy_entity_id = crate::embed_queue::project_file_entity_id(&chunk);
        let (_queue_route, vector_route) = code_route(&router);
        let enveloped = crate::embed_queue::project_file_text_content_hash(&chunk.chunk_hash);
        crate::vectors::upsert(&vector_route, &legacy_entity_id, &enveloped, vec![0.5; 8]).unwrap();

        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Code],
            &doc,
        )
        .unwrap();
        let entry = coverage
            .values()
            .find(|entry| entry.source_count > 0)
            .expect("the code route was counted");
        assert_eq!(entry.indexed_count, 0);
    }

    /// P3-E embed row: the visual lane is OUTSIDE the envelope. Its embedding
    /// input carries no text prepend and is unchanged by this milestone, so it
    /// neither re-embeds nor loses coverage - its vectors stay keyed by the raw
    /// `chunk_hash` and the coverage visual arm keeps raw comparison.
    #[test]
    fn the_visual_lane_stays_outside_the_envelope_and_keeps_coverage() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
image = "voyage_visual"
"#,
        )
        .unwrap();
        let doc = image_embedding_source_doc();
        let raw_hash = doc.chunk_hash.clone().unwrap();
        let entity_id = doc.entity_id.clone().unwrap();
        let vector_route = router
            .visual_route("image")
            .unwrap()
            .expect("configured visual route")
            .vector_route_id();
        crate::vectors::upsert(&vector_route, &entity_id, &raw_hash, vec![0.3; 8]).unwrap();

        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Docs],
            &doc,
        )
        .unwrap();
        let entry = coverage.get("visual:image").expect("visual route counted");
        assert_eq!(entry.source_count, 1);
        assert_eq!(
            entry.indexed_count, 1,
            "the visual lane keeps raw-hash coverage across the text-envelope bump"
        );
    }

    /// An unconfigured visual chunk kind is skipped, not counted: visual
    /// embedding is opt-in per kind (no `[embed.routes.visual]` entry means
    /// no partition to report coverage against).
    #[test]
    fn unconfigured_visual_kind_is_skipped_not_counted() {
        let vector_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(VectorStore::open(vector_tmp.path()).unwrap());
        let _guard = install_test_global(store);
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let mut coverage = BTreeMap::new();
        let mut active_by_route = BTreeMap::new();
        record_index_doc_coverage(
            &router,
            &mut coverage,
            &mut active_by_route,
            &[Bucket::Docs],
            &image_embedding_source_doc(),
        )
        .unwrap();
        assert!(coverage.is_empty());
    }

    /// Deliverable 4: a `visual:<kind>` status row seeded by coverage alone
    /// (`status_response_for_buckets`'s `.or_default()` — no live queue
    /// worker ever spawned in this process, so `provider_route_status`
    /// never ran) defaults to `available: true` with provider/model/dim
    /// left `None`: cosmetically broken, since `available=true` reads as
    /// "this route works" with nothing describing what it routes to.
    /// `backfill_visual_route_metadata` must fill those fields from
    /// `VisualRouteMeta` without disturbing coverage fields already set.
    #[test]
    fn backfill_visual_route_metadata_populates_provider_fields_from_config() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response.routes.insert(
            "visual:pdf_figure".into(),
            crate::embed::queue::RouteStatus {
                source_count: Some(3),
                indexed_count: 2,
                coverage_ratio: Some(0.666),
                ..Default::default()
            },
        );

        backfill_visual_route_metadata(&router, &mut response);

        let status = &response.routes["visual:pdf_figure"];
        assert!(status.available, "backfill must not touch availability");
        assert_eq!(status.provider.as_deref(), Some("voyage_visual"));
        assert_eq!(status.model.as_deref(), Some("voyage-multimodal-3.5"));
        assert_eq!(status.dim, Some(1024));
        assert_eq!(
            status.endpoint_kind,
            Some(crate::embed::EmbedEndpointKind::Multimodal)
        );
        assert_eq!(status.output_dtype.as_deref(), Some("float"));
        assert_eq!(
            status.compatibility_family.as_deref(),
            Some("voyage-multimodal-3.5:1024:float")
        );
        // Coverage fields computed earlier in status_response_for_buckets
        // are untouched by the backfill pass.
        assert_eq!(status.indexed_count, 2);
        assert_eq!(status.coverage_ratio, Some(0.666));
    }

    /// A route whose provider metadata a live queue worker already
    /// populated (`provider_route_status`) must not be overwritten by the
    /// coverage-only backfill.
    #[test]
    fn backfill_visual_route_metadata_never_overwrites_a_live_worker_row() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response.routes.insert(
            "visual:pdf_figure".into(),
            crate::embed::queue::RouteStatus {
                provider: Some("already-set".into()),
                ..Default::default()
            },
        );

        backfill_visual_route_metadata(&router, &mut response);

        assert_eq!(
            response.routes["visual:pdf_figure"].provider.as_deref(),
            Some("already-set")
        );
    }

    /// A non-visual route key (no `visual:` prefix) must never be touched.
    #[test]
    fn backfill_visual_route_metadata_ignores_non_visual_routes() {
        let router = EmbeddingRouter::from_toml_str("").unwrap();
        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response
            .routes
            .insert("code".into(), crate::embed::queue::RouteStatus::default());

        backfill_visual_route_metadata(&router, &mut response);

        assert!(response.routes["code"].provider.is_none());
    }

    /// Backfill: an image doc routes through `enqueue_visual_project_file`,
    /// not the Code/Docs bucket path. No queue is installed in this test
    /// process, so the enqueue itself is a no-op (mirrors
    /// `reembed_index_enqueue_counts_only_queue_accepted_items`); what this
    /// proves is that the visual branch is reached and does not panic or
    /// fall through to the bucket dispatch.
    #[test]
    fn enqueue_reembed_index_doc_routes_image_chunks_through_the_visual_lane() {
        assert!(!enqueue_reembed_index_doc(
            &[Bucket::Docs],
            &image_embedding_source_doc()
        ));
    }

    // ---- residue sweeper -------------------------------------------------

    fn route_status(
        available: bool,
        source: u64,
        indexed: u64,
        dropped: u64,
        queue_depth: u64,
    ) -> queue::RouteStatus {
        queue::RouteStatus {
            available,
            source_count: Some(source),
            indexed_count: indexed,
            dropped_count: dropped,
            queue_depth,
            ..Default::default()
        }
    }

    /// The transcript corpus is guarded: the sweeper must never list it among
    /// sweepable routes, so a sweep can never auto-enqueue transcripts.
    #[test]
    fn sweepable_buckets_exclude_the_guarded_transcript_corpus() {
        let buckets = sweepable_buckets();
        assert!(!buckets.contains(&Bucket::Transcripts));
        assert_eq!(buckets.len(), Bucket::ALL.len() - 1);
        for bucket in Bucket::ALL {
            if bucket != Bucket::Transcripts {
                assert!(buckets.contains(&bucket), "missing {bucket:?}");
            }
        }
    }

    /// Enqueueable residue excludes poison (dropped) and outages
    /// (unavailable) and mid-drain routes — the cost-safety guard that keeps
    /// the sweeper from re-hammering provider-rejected payloads or a
    /// credential-missing route.
    #[test]
    fn route_enqueueable_residue_excludes_poison_outages_and_in_flight() {
        // 100 source, 60 embedded, 0 dropped, idle, available -> 40 residue.
        assert_eq!(
            route_enqueueable_residue(&route_status(true, 100, 60, 0, 0)),
            40
        );
        // The 40-item shortfall is entirely poison -> nothing enqueueable.
        assert_eq!(
            route_enqueueable_residue(&route_status(true, 100, 60, 40, 0)),
            0
        );
        // Unavailable (e.g. credential missing) -> excluded regardless.
        assert_eq!(
            route_enqueueable_residue(&route_status(false, 100, 0, 0, 0)),
            0
        );
        // Mid-drain (queue_depth > 0) -> wait for it, don't re-enqueue.
        assert_eq!(
            route_enqueueable_residue(&route_status(true, 100, 0, 0, 5)),
            0
        );
    }

    #[test]
    fn sweep_snapshot_skips_transcripts_and_flags_busy() {
        let mut response = EmbedStatusResponse {
            routes: Default::default(),
        };
        response
            .routes
            .insert("code".into(), route_status(true, 100, 90, 0, 0)); // 10 residue
        response
            .routes
            .insert("notes".into(), route_status(true, 50, 50, 0, 3)); // busy
        // A transcript route with residue AND a draining queue must be
        // ignored on both axes (guarded corpus).
        response
            .routes
            .insert("transcripts".into(), route_status(true, 999, 0, 0, 7));

        let snap = sweep_snapshot(&response);
        assert_eq!(
            snap.residue, 10,
            "only code's residue; transcripts excluded"
        );
        assert!(snap.busy, "notes is draining");
    }

    /// Termination / no-hot-loop: the wake decision. A stuck pass (residue
    /// remains but nothing admitted and no cap hit) must drop to timer-only
    /// so it cannot spin on drain nudges; every other state may fast-refill.
    #[test]
    fn sweep_wake_decision_backs_off_only_on_unenqueueable_residue() {
        let base = SweepReport::default();
        // Converged: idle, listen for a manual reembed kick.
        assert!(sweep_should_listen_for_drain(&SweepReport {
            enqueueable_residue: 0,
            ..base
        }));
        // Busy: wait for the in-flight wave's drain nudge.
        assert!(sweep_should_listen_for_drain(&SweepReport {
            enqueueable_residue: 5,
            busy: true,
            ..base
        }));
        // Progress: a wave was admitted -> fast refill on drain.
        assert!(sweep_should_listen_for_drain(&SweepReport {
            enqueueable_residue: 5,
            enqueued: 3,
            ran_pass: true,
            ..base
        }));
        // Capped: more waves to come -> fast refill on drain.
        assert!(sweep_should_listen_for_drain(&SweepReport {
            enqueueable_residue: 5,
            capped: true,
            ran_pass: true,
            ..base
        }));
        // Stuck: residue remains but nothing admitted and not capped ->
        // back off to the timer (no drain-nudge spin).
        assert!(!sweep_should_listen_for_drain(&SweepReport {
            enqueueable_residue: 5,
            enqueued: 0,
            capped: false,
            ran_pass: true,
            ..base
        }));
    }
}
