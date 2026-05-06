#![allow(dead_code)] // E1 lands provider/routing surface; E2/E3 wire live consumers.

pub mod ollama;
pub mod queue;
pub mod voyage;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const VOYAGE_PROVIDER_ID: &str = "voyage";
pub const OLLAMA_PROVIDER_ID: &str = "ollama";

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
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
}

impl Bucket {
    pub const ALL: [Bucket; 6] = [
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::Transcripts,
        Bucket::GitMessage,
        Bucket::Notes,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Code => "code",
            Self::Docs => "docs",
            Self::Transcripts => "transcripts",
            Self::GitMessage => "git_message",
            Self::Notes => "notes",
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
            other => bail!("unknown embedding provider `{other}` for bucket {}", bucket.as_str()),
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
            other => bail!("unknown embedding provider `{other}` for bucket {}", bucket.as_str()),
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
}

pub fn route_for(bucket: Bucket, project_id: Option<&str>) -> Result<Box<dyn EmbeddingProvider>> {
    EmbeddingRouter::load_default()?.route_for(bucket, project_id)
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
}

pub fn reembed_stub(p: &ReembedParams) -> Result<String> {
    if p.route.trim().is_empty() {
        bail!("route is required");
    }
    tracing::info!(
        route = %p.route,
        "rebuild requested for embedding route; not yet implemented (lands in E3)"
    );
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "route": p.route,
        "message": format!("rebuild requested for route {}; not yet implemented (lands in E3)", p.route),
    }))?)
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
code = "voyage"

[embed.routes.per_project."proj1234"]
code = "ollama"
"#,
        )
        .unwrap();
        assert_eq!(
            router.route(Bucket::Code, Some("proj1234")).unwrap().provider_id,
            OLLAMA_PROVIDER_ID
        );
        assert_eq!(
            router.route(Bucket::Code, None).unwrap().provider_id,
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
    fn reembed_stub_returns_ok() {
        let rendered = reembed_stub(&ReembedParams {
            route: "knowledge".into(),
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["route"], "knowledge");
    }
}
