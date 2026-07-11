//! Daemon-side embedding runtime — the upward-coupled half of the embed
//! surface. The contract (Bucket, EmbeddingRouter, the queue handle and
//! enqueue helpers) lives in `crate::embed` / `crate::embed_queue`; this
//! module owns everything that needs `SharedState`, orchestration agent
//! types, or routing-verdict dispatch: reembed orchestration, embedding
//! route coverage, agent-manifest embeddings, and the knowledge
//! contradiction detector (registered into the queue worker's hook at
//! SharedState construction).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

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
    tokio::spawn(async move {
        match enqueue_reembed_routes(&state, &buckets, max_entities) {
            Ok(enqueued) => {
                tracing::info!(route = %route, ?max_entities, enqueued, "embedding rebuild queue refill completed");
            }
            Err(err) => {
                tracing::warn!(route = %route, error = %err, "embedding rebuild queue refill failed");
            }
        }
    });
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "route": p.route,
        "max_entities": p.max_entities,
        "message": "rebuild queue refill started; final enqueue count will be logged",
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
}

/// Partition lifecycle (`bbox_embed_partitions`). Deliberately separate
/// from `bbox_reembed`, which never prunes: re-embedding and reclaiming
/// orphaned vector spaces are different operator decisions
/// (design/corpus/agentic-corpus/multimodal-embedding-routing.md Layer 5).
pub fn embed_partitions(p: &EmbedPartitionsParams) -> Result<String> {
    let router = EmbeddingRouter::load_default().unwrap_or_default();
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
    let mut mapped: BTreeMap<String, (Vec<String>, crate::embed::Route)> = BTreeMap::new();
    for route in router.configured_routes() {
        let label = match &route.project_id {
            Some(project) => format!("{}@{project}", route.bucket.as_str()),
            None => route.bucket.as_str().to_string(),
        };
        mapped
            .entry(route.vector_route_id())
            .or_insert_with(|| (Vec::new(), route.clone()))
            .0
            .push(label);
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
                "mapped_buckets": mapping.map(|(buckets, _)| buckets.clone()).unwrap_or_default(),
                "provider": mapping.map(|(_, r)| r.provider_id.clone()),
                "endpoint_kind": mapping.map(|(_, r)| r.endpoint_kind),
                "document_model": mapping.map(|(_, r)| r.document_model.clone()),
                "query_model": mapping.map(|(_, r)| r.query_model.clone()),
                "output_dtype": mapping.map(|(_, r)| r.output_dtype.as_str()),
                "compatibility_family": mapping.map(|(_, r)| r.compatibility_family.clone()),
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
            record_coverage(
                router,
                coverage,
                active_by_route,
                bucket,
                Some(&chunk.project_id),
                &crate::embed_queue::project_file_entity_id(&chunk),
                &chunk.chunk_hash,
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
        Bucket::Knowledge | Bucket::Notes | Bucket::Threads | Bucket::AgentManifest => Ok(()),
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
        for entry in state.kb.read().all_entries() {
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
    let doc_types = reembed_index_doc_types(buckets);
    if !doc_types.is_empty() {
        let remaining = max_entities.map(|max| max.saturating_sub(enqueued));
        let mut index_enqueued = 0usize;
        state
            .idx
            .read()
            .for_each_embedding_source_doc_for_doc_types(&doc_types, None, |doc| {
                if limit_reached(remaining, index_enqueued) {
                    return Ok(());
                }
                if enqueue_reembed_index_doc(buckets, &doc) {
                    index_enqueued += 1;
                }
                Ok(())
            })?;
        enqueued += index_enqueued;
    }
    Ok(enqueued)
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
            let entity_id = crate::embed_queue::project_file_entity_id(&chunk);
            crate::embed_queue::enqueue_project_file_as(&chunk, &entity_id, bucket)
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
        Bucket::Knowledge | Bucket::Notes | Bucket::Threads | Bucket::AgentManifest => false,
    }
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
            let path = Path::new(&doc.file_path);
            if crate::chunker::code::language_for_path(path).is_some() {
                Some(Bucket::Code)
            } else if is_docs_path(path) {
                Some(Bucket::Docs)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_docs_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("md" | "mdx" | "rst" | "txt")
    )
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
    Some(Chunk {
        project_id,
        file_path: PathBuf::from(&doc.file_path),
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
    queue::normalize_route_statuses(&mut response);
    apply_stall_health(&mut response);
    Ok(response)
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
                    "coverage {ratio:.3} with idle queue — unembedded residue is not being enqueued; run bbox_reembed(route=\"{route}\") or wait for the nightly backfill"
                )
            });
        }
    }
}

pub(crate) fn status_json_for_state(state: &SharedState) -> Result<String> {
    const STATUS_COVERAGE_BUCKETS: &[Bucket] = &[
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::GitMessage,
        Bucket::Notes,
        Bucket::Threads,
        Bucket::AgentManifest,
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
    Ok(serde_json::to_string_pretty(&response)?)
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
    fn embed_partitions_prune_requires_age_threshold() {
        let router = EmbeddingRouter::default();
        let params = EmbedPartitionsParams {
            action: Some("prune".into()),
            older_than_days: None,
            apply: false,
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
        assert!(
            git.health_reason
                .as_deref()
                .unwrap()
                .contains("bbox_reembed"),
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
}
