#![allow(dead_code)] // E1 lands provider/routing surface; E2/E3 wire live consumers.

pub mod ollama;
pub mod queue;
pub mod voyage;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_corpus_core::entity_ref::EntityRef;

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

    pub fn queue_and_vector_route(
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
pub struct ClusterId {
    pub id: String,
    pub members: Vec<EntityRef>,
}

pub fn embed_iterate_internal(
    bucket: &str,
    project_id: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<impl Iterator<Item = (EntityRef, Vec<f32>)>> {
    let route = internal_bucket_route(bucket, project_id)?;
    let rows = bbox_vectors::iter_active(&route, since)?
        .filter_map(|entry| {
            vector_entity_ref(&entry.entity_id).map(|entity| (entity, entry.vector))
        })
        .collect::<Vec<_>>();
    Ok(rows.into_iter())
}

pub fn cluster_neighbors_within(
    bucket: &str,
    project_id: &str,
    similarity_threshold: f32,
) -> Result<Vec<ClusterId>> {
    let route = internal_bucket_route(bucket, project_id)?;
    let clusters = bbox_vectors::cluster_neighbors_within_route(&route, similarity_threshold)?
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
    if let Some((name, version, _component)) =
        crate::embed_queue::parse_agent_component_entity_id_parts(raw)
    {
        return Some(EntityRef::Agent { name, version });
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
    fn embed_iterate_internal_respects_since_and_entity_refs() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = bbox_vectors::install_test_global(store.clone());
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
    fn cluster_neighbors_within_returns_bounded_entity_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = bbox_vectors::install_test_global(store.clone());
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
