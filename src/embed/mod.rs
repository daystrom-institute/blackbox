#![allow(dead_code)] // E1 lands provider/routing surface; E2/E3 wire live consumers.

pub mod ollama;
pub mod queue;
pub mod voyage;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::chunker::Chunk;
use crate::entity_ref::EntityRef;
use crate::index::EmbeddingSourceDoc;
use crate::server::state::SharedState;

pub const VOYAGE_PROVIDER_ID: &str = "voyage";
pub const OLLAMA_PROVIDER_ID: &str = "ollama";

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch once. Transient errors propagate to the caller;
    /// retry policy and exponential backoff are owned by the E2 queue.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
    fn model_name(&self) -> &str;
    fn id(&self) -> &str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Knowledge,
    Code,
    Docs,
    Transcripts,
    GitMessage,
    Notes,
    Threads,
    AgentManifest,
}

impl Bucket {
    pub const ALL: [Bucket; 8] = [
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::Transcripts,
        Bucket::GitMessage,
        Bucket::Notes,
        Bucket::Threads,
        Bucket::AgentManifest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Code => "code",
            Self::Docs => "docs",
            Self::Transcripts => "transcripts",
            Self::GitMessage => "git_message",
            Self::Notes => "notes",
            Self::Threads => "threads",
            Self::AgentManifest => "agent_manifest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub bucket: Bucket,
    pub project_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub dimensions: usize,
}

impl Route {
    pub fn vector_route_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.provider_id.as_bytes());
        hasher.update([0]);
        hasher.update(self.model.as_bytes());
        hasher.update([0]);
        hasher.update(self.dimensions.to_string().as_bytes());
        let digest = hasher.finalize();
        format!(
            "{}-{}-{}-{:02x}{:02x}{:02x}{:02x}",
            sanitize_route_component(&self.provider_id),
            sanitize_route_component(&self.model),
            self.dimensions,
            digest[0],
            digest[1],
            digest[2],
            digest[3]
        )
    }
}

fn sanitize_route_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedConfigFile {
    #[serde(default)]
    pub embed: EmbedConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmbedConfig {
    #[serde(default)]
    pub providers: ProviderConfigs,
    #[serde(default)]
    pub routes: RoutesConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderConfigs {
    #[serde(default)]
    pub voyage: voyage::VoyageConfig,
    #[serde(default)]
    pub ollama: ollama::OllamaConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutesConfig {
    pub knowledge: Option<String>,
    pub code: Option<String>,
    pub docs: Option<String>,
    pub transcripts: Option<String>,
    pub git_message: Option<String>,
    pub notes: Option<String>,
    pub threads: Option<String>,
    pub agent_manifest: Option<String>,
    #[serde(default)]
    pub per_project: BTreeMap<String, BucketRoutes>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BucketRoutes {
    pub knowledge: Option<String>,
    pub code: Option<String>,
    pub docs: Option<String>,
    pub transcripts: Option<String>,
    pub git_message: Option<String>,
    pub notes: Option<String>,
    pub threads: Option<String>,
    pub agent_manifest: Option<String>,
}

impl BucketRoutes {
    fn get(&self, bucket: Bucket) -> Option<&str> {
        match bucket {
            Bucket::Knowledge => self.knowledge.as_deref(),
            Bucket::Code => self.code.as_deref(),
            Bucket::Docs => self.docs.as_deref(),
            Bucket::Transcripts => self.transcripts.as_deref(),
            Bucket::GitMessage => self.git_message.as_deref(),
            Bucket::Notes => self.notes.as_deref(),
            Bucket::Threads => self.threads.as_deref(),
            Bucket::AgentManifest => self.agent_manifest.as_deref(),
        }
    }
}

impl RoutesConfig {
    fn global(&self, bucket: Bucket) -> Option<&str> {
        match bucket {
            Bucket::Knowledge => self.knowledge.as_deref(),
            Bucket::Code => self.code.as_deref(),
            Bucket::Docs => self.docs.as_deref(),
            Bucket::Transcripts => self.transcripts.as_deref(),
            Bucket::GitMessage => self.git_message.as_deref(),
            Bucket::Notes => self.notes.as_deref(),
            Bucket::Threads => self.threads.as_deref(),
            Bucket::AgentManifest => self.agent_manifest.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EmbeddingRouter {
    config: EmbedConfig,
}

impl EmbeddingRouter {
    pub fn load_default() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading embed config {}", path.display()))?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let file: EmbedConfigFile = toml::from_str(text).context("parsing embed config")?;
        Ok(Self { config: file.embed })
    }

    pub fn route(&self, bucket: Bucket, project_id: Option<&str>) -> Result<Route> {
        let provider_id = self.provider_id(bucket, project_id);
        let (model, dimensions) = match provider_id {
            VOYAGE_PROVIDER_ID => (
                self.config.providers.voyage.model.clone(),
                voyage::VOYAGE_DIMENSIONS,
            ),
            OLLAMA_PROVIDER_ID => (
                self.config.providers.ollama.model.clone(),
                ollama::OLLAMA_DIMENSIONS,
            ),
            other => bail!(
                "unknown embedding provider `{other}` for bucket {}",
                bucket.as_str()
            ),
        };
        Ok(Route {
            bucket,
            project_id: project_id.map(str::to_string),
            provider_id: provider_id.to_string(),
            model,
            dimensions,
        })
    }

    pub fn route_for(
        &self,
        bucket: Bucket,
        project_id: Option<&str>,
    ) -> Result<Box<dyn EmbeddingProvider>> {
        let provider_id = self.provider_id(bucket, project_id);
        match provider_id {
            VOYAGE_PROVIDER_ID => Ok(Box::new(voyage::VoyageProvider::from_config(
                self.config.providers.voyage.clone(),
            )?)),
            OLLAMA_PROVIDER_ID => Ok(Box::new(ollama::OllamaProvider::from_config(
                self.config.providers.ollama.clone(),
            )?)),
            other => bail!(
                "unknown embedding provider `{other}` for bucket {}",
                bucket.as_str()
            ),
        }
    }

    fn provider_id(&self, bucket: Bucket, project_id: Option<&str>) -> &str {
        project_id
            .and_then(|id| self.config.routes.per_project.get(id))
            .and_then(|routes| routes.get(bucket))
            .or_else(|| self.config.routes.global(bucket))
            .unwrap_or(VOYAGE_PROVIDER_ID)
    }

    pub fn rate_limit_per_min(&self, provider_id: &str) -> Option<u32> {
        match provider_id {
            VOYAGE_PROVIDER_ID => Some(self.config.providers.voyage.rate_limit_per_min),
            OLLAMA_PROVIDER_ID => None,
            _ => None,
        }
    }

    pub(crate) fn queue_and_vector_route(
        &self,
        bucket: Bucket,
        project_id: Option<&str>,
    ) -> Result<(String, String)> {
        let route = self.route(bucket, project_id)?;
        let default = self.route(bucket, None)?;
        let queue_route = if project_id.is_some()
            && (route.provider_id != default.provider_id
                || route.model != default.model
                || route.dimensions != default.dimensions)
        {
            format!("{}:{}", bucket.as_str(), project_id.unwrap_or_default())
        } else {
            bucket.as_str().to_string()
        };
        Ok((queue_route, route.vector_route_id()))
    }
}

pub fn route_for(bucket: Bucket, project_id: Option<&str>) -> Result<Box<dyn EmbeddingProvider>> {
    EmbeddingRouter::load_default()?.route_for(bucket, project_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClusterId {
    pub id: String,
    pub members: Vec<EntityRef>,
}

pub(crate) fn embed_iterate_internal(
    bucket: &str,
    project_id: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<impl Iterator<Item = (EntityRef, Vec<f32>)>> {
    let route = internal_bucket_route(bucket, project_id)?;
    let rows = crate::vectors::iter_active(&route, since)?
        .filter_map(|entry| {
            vector_entity_ref(&entry.entity_id).map(|entity| (entity, entry.vector))
        })
        .collect::<Vec<_>>();
    Ok(rows.into_iter())
}

pub(crate) fn cluster_neighbors_within(
    bucket: &str,
    project_id: &str,
    similarity_threshold: f32,
) -> Result<Vec<ClusterId>> {
    let route = internal_bucket_route(bucket, project_id)?;
    let clusters = crate::vectors::cluster_neighbors_within_route(&route, similarity_threshold)?
        .into_iter()
        .filter_map(|cluster| {
            let members = cluster
                .members
                .iter()
                .filter_map(|raw| vector_entity_ref(raw))
                .collect::<Vec<_>>();
            if members.len() < 2 {
                None
            } else {
                Some(ClusterId {
                    id: cluster.id,
                    members,
                })
            }
        })
        .collect();
    Ok(clusters)
}

fn internal_bucket_route(bucket: &str, project_id: &str) -> Result<String> {
    let bucket = bucket_from_str(bucket)?;
    let project = if project_id.trim().is_empty() {
        None
    } else {
        Some(project_id.trim())
    };
    Ok(EmbeddingRouter::load_default()?
        .route(bucket, project)?
        .vector_route_id())
}

fn bucket_from_str(bucket: &str) -> Result<Bucket> {
    let bucket = bucket.trim();
    Bucket::ALL
        .iter()
        .copied()
        .find(|candidate| candidate.as_str() == bucket)
        .with_context(|| {
            format!(
                "unknown embedding bucket `{bucket}`; expected one of: {}",
                Bucket::ALL
                    .iter()
                    .map(|candidate| candidate.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn vector_entity_ref(raw: &str) -> Option<EntityRef> {
    if let Some((agent, _component)) = crate::embed_queue::parse_agent_component_entity_id(raw) {
        return Some(EntityRef::Agent {
            name: agent.name,
            version: agent.version,
        });
    }
    EntityRef::parse(raw).ok()
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("blackbox")
        .join("embed.toml")
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RouteDimensionTracker {
    dimensions: BTreeMap<String, usize>,
}

impl RouteDimensionTracker {
    pub fn record(&mut self, route: impl Into<String>, dimensions: usize) -> Result<()> {
        let route = route.into();
        if let Some(existing) = self.dimensions.get(&route) {
            if *existing != dimensions {
                bail!(
                    "embedding route `{route}` dimension mismatch: existing={existing}, incoming={dimensions}; run bbox_reembed for the route before mixing vectors"
                );
            }
        } else {
            self.dimensions.insert(route, dimensions);
        }
        Ok(())
    }
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
    Bucket::ALL
        .iter()
        .copied()
        .find(|bucket| bucket.as_str() == route)
        .map(|bucket| vec![bucket])
        .with_context(|| {
            format!(
                "unknown embedding route `{route}`; expected one of: all, {}",
                Bucket::ALL
                    .iter()
                    .map(|bucket| bucket.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn count_reembed_entities(state: &Arc<SharedState>, buckets: &[Bucket]) -> Result<usize> {
    let knowledge_count = if buckets.contains(&Bucket::Knowledge) {
        state.kb.read().all_entries().len()
    } else {
        0
    };
    let note_count = if buckets.contains(&Bucket::Notes) {
        state.notes.read().all().len()
    } else {
        0
    };
    let agent_count = if buckets.contains(&Bucket::AgentManifest) {
        count_agent_manifest_components(state)?
    } else {
        0
    };
    let doc_types = reembed_index_doc_types(buckets);
    let mut index_count = 0usize;
    if !doc_types.is_empty() {
        state
            .idx
            .read()
            .for_each_embedding_source_doc_for_doc_types(&doc_types, None, |doc| {
                if reembed_index_doc_bucket(&doc).is_some_and(|bucket| buckets.contains(&bucket)) {
                    index_count += 1;
                }
                Ok(())
            })?;
    }
    Ok(knowledge_count + note_count + agent_count + index_count)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RouteCoverage {
    pub source_count: u64,
    pub indexed_count: u64,
}

pub(crate) fn route_coverage(
    state: &SharedState,
    buckets: &[Bucket],
) -> Result<BTreeMap<String, RouteCoverage>> {
    let router = EmbeddingRouter::load_default()?;
    let mut coverage = BTreeMap::new();
    let mut active_by_route = BTreeMap::new();
    if buckets.contains(&Bucket::Knowledge) {
        for entry in state.kb.read().all_entries() {
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
        for item in state.roadmap.read().all_items() {
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
        for note in state.notes.read().all() {
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
        for thread in state.threads.read().all() {
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
        record_agent_manifest_coverage(state, &router, &mut coverage, &mut active_by_route)?;
    }
    let doc_types = reembed_index_doc_types(buckets);
    if !doc_types.is_empty() {
        state
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
    state: &SharedState,
    router: &EmbeddingRouter,
    coverage: &mut BTreeMap<String, RouteCoverage>,
    active_by_route: &mut BTreeMap<String, HashSet<(String, String)>>,
) -> Result<()> {
    let catalog = state.artifacts.read();
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
            crate::embed_queue::AgentManifestComponent::Primary,
            crate::embed_queue::AgentManifestComponent::WhenToUse,
            crate::embed_queue::AgentManifestComponent::AntiPatterns,
        ] {
            let Some(chunk_hash) = crate::embed_queue::agent_component_hash(&manifest, component)
            else {
                continue;
            };
            record_coverage(
                router,
                coverage,
                active_by_route,
                Bucket::AgentManifest,
                None,
                &crate::embed_queue::agent_component_entity_id(&agent, component),
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
            crate::embed_queue::enqueue_knowledge(entry, &entity_id, &chunk_hash);
            enqueued += 1;
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
            crate::embed_queue::enqueue_roadmap(item, &entity_id, &chunk_hash);
            enqueued += 1;
        }
    }
    if buckets.contains(&Bucket::Notes) {
        for note in state.notes.read().all() {
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            crate::embed_queue::enqueue_note(note);
            enqueued += 1;
        }
    }
    if buckets.contains(&Bucket::Threads) {
        for thread in state.threads.read().all() {
            if limit_reached(max_entities, enqueued) {
                return Ok(enqueued);
            }
            crate::embed_queue::enqueue_thread(thread);
            enqueued += 1;
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
            .for_each_embedding_source_doc_for_doc_types(&doc_types, remaining, |doc| {
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

fn count_agent_manifest_components(state: &Arc<SharedState>) -> Result<usize> {
    let catalog = state.artifacts.read();
    let entries = catalog.list(&crate::artifacts::ArtifactListParams {
        kind: Some(crate::artifacts::ArtifactKind::Agent),
        name: None,
        include_superseded: true,
    })?;
    let mut count = 0usize;
    for entry in entries {
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
        count += crate::embed_queue::agent_manifest_component_count(&manifest);
    }
    Ok(count)
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
        enqueued += crate::embed_queue::agent_manifest_component_count(&manifest);
        crate::embed_queue::enqueue_agent_manifest(&agent, &manifest);
    }
    Ok(enqueued)
}

fn count_reembed_index_docs(buckets: &[Bucket], docs: &[EmbeddingSourceDoc]) -> usize {
    docs.iter()
        .filter(|doc| reembed_index_doc_bucket(doc).is_some_and(|bucket| buckets.contains(&bucket)))
        .count()
}

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
            crate::embed_queue::enqueue_project_file(&chunk, &entity_id);
            true
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
            );
            true
        }
        Bucket::GitMessage => {
            let (Some(entity_id), Some(chunk_hash)) = (&doc.entity_id, &doc.chunk_hash) else {
                return false;
            };
            crate::embed_queue::enqueue_git_message(entity_id, chunk_hash, &doc.content);
            true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_is_voyage() {
        let router = EmbeddingRouter::default();
        let route = router.route(Bucket::Knowledge, None).unwrap();
        assert_eq!(route.provider_id, VOYAGE_PROVIDER_ID);
        assert_eq!(route.dimensions, voyage::VOYAGE_DIMENSIONS);
    }

    #[test]
    fn per_project_override_beats_global_route() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
threads = "voyage"

[embed.routes.per_project."proj1234"]
threads = "ollama"
"#,
        )
        .unwrap();
        assert_eq!(
            router
                .route(Bucket::Threads, Some("proj1234"))
                .unwrap()
                .provider_id,
            OLLAMA_PROVIDER_ID
        );
        assert_eq!(
            router.route(Bucket::Threads, None).unwrap().provider_id,
            VOYAGE_PROVIDER_ID
        );
    }

    #[test]
    fn dimension_tracker_rejects_mismatch() {
        let mut tracker = RouteDimensionTracker::default();
        tracker.record("knowledge", 1024).unwrap();
        let err = tracker.record("knowledge", 768).unwrap_err();
        assert!(err.to_string().contains("dimension mismatch"));
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

    #[test]
    fn reembed_empty_index_doc_enumeration_counts_zero() {
        assert_eq!(count_reembed_index_docs(&[Bucket::Code], &[]), 0);
        assert_eq!(count_reembed_index_docs(&Bucket::ALL, &[]), 0);
    }

    #[test]
    fn reembed_index_enqueue_honors_max_entities() {
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
            1
        );
    }

    #[test]
    fn embed_iterate_internal_respects_since_and_entity_refs() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = crate::vectors::install_test_global(store.clone());
        let route = EmbeddingRouter::default()
            .route(Bucket::Transcripts, None)
            .unwrap()
            .vector_route_id();
        store
            .upsert(
                &route,
                "transcript:claude:old-session:1:0",
                "old",
                vec![1.0, 0.0],
            )
            .unwrap();
        let cutoff = chrono::Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .upsert(
                &route,
                "transcript:claude:new-session:2:0",
                "new",
                vec![0.0, 1.0],
            )
            .unwrap();

        let rows = embed_iterate_internal("transcripts", "", Some(cutoff))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0,
            EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "new-session".into(),
                line_offset: 2,
                event_idx: 0,
            }
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
    fn cluster_neighbors_within_returns_bounded_entity_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = crate::vectors::install_test_global(store.clone());
        let route = EmbeddingRouter::default()
            .route(Bucket::AgentManifest, None)
            .unwrap()
            .vector_route_id();
        store
            .upsert(
                &route,
                "agent_embed:reviewer:v1:primary",
                "a",
                vec![1.0, 0.0],
            )
            .unwrap();
        store
            .upsert(
                &route,
                "agent_embed:copywriter:v1:primary",
                "b",
                vec![1.0, 0.0],
            )
            .unwrap();
        store
            .upsert(&route, "agent_embed:writer:v1:primary", "c", vec![0.0, 1.0])
            .unwrap();

        let clusters = cluster_neighbors_within("agent_manifest", "", 0.99).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].members,
            vec![
                EntityRef::Agent {
                    name: "copywriter".into(),
                    version: 1,
                },
                EntityRef::Agent {
                    name: "reviewer".into(),
                    version: 1,
                }
            ]
        );
    }
}
