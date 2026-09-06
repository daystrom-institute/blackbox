//! Native transcript snapshot wire. Source identity is an enrolled connector
//! scope plus an opaque stream id, never a producer or daemon filesystem path.
use anyhow::{Result, ensure};
use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 1;
pub const CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_STREAM_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_CHUNKS: usize = 1024;

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn validate_hash(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid sha256 identifier"
    );
    Ok(())
}
fn label(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "invalid transcript label"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeSource {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkRef {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSnapshot {
    pub schema_version: u32,
    pub scope: ConnectorScope,
    /// Hash of the producer's source/account/relative stream identity.
    pub stream_id: String,
    pub source: NativeSource,
    pub account: String,
    pub session_id: String,
    #[serde(default)]
    pub is_subagent: bool,
    pub content_sha256: String,
    pub byte_length: u64,
    /// Complete raw JSONL prefix, split into fixed-size chunks. Only the last
    /// chunk may be shorter. A torn final source line is never published.
    pub chunks: Vec<ChunkRef>,
}
impl StreamSnapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unsupported transcript schema"
        );
        validate_hash(&self.stream_id)?;
        validate_hash(&self.content_sha256)?;
        label(&self.account, 128)?;
        label(&self.session_id, 256)?;
        ensure!(
            self.byte_length <= MAX_STREAM_BYTES && self.chunks.len() <= MAX_CHUNKS,
            "transcript stream exceeds limits"
        );
        let mut bytes = 0u64;
        for (i, chunk) in self.chunks.iter().enumerate() {
            validate_hash(&chunk.sha256)?;
            ensure!(
                chunk.size > 0 && chunk.size <= CHUNK_BYTES as u64,
                "invalid transcript chunk size"
            );
            ensure!(
                i + 1 == self.chunks.len() || chunk.size == CHUNK_BYTES as u64,
                "non-final transcript chunk is short"
            );
            bytes += chunk.size;
        }
        ensure!(
            bytes == self.byte_length,
            "transcript snapshot length mismatch"
        );
        Ok(())
    }
    pub fn generation(&self) -> Result<String> {
        self.validate()?;
        Ok(sha256(&serde_json::to_vec(self)?))
    }
    pub fn locator(&self, generation: &str) -> String {
        format!(
            "native:{}/{}/{}",
            self.scope.connector_source_id(),
            self.stream_id,
            generation
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub snapshot: StreamSnapshot,
    /// Compare-and-swap authority. None means no published generation exists.
    pub expected_generation: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishedSnapshot {
    pub snapshot: StreamSnapshot,
    pub generation: String,
    pub published_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStatus {
    pub stream_id: String,
    pub generation: Option<String>,
    pub byte_length: u64,
    pub published_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishReceipt {
    pub generation: String,
    pub locator: String,
    pub byte_length: u64,
    pub durable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamQuery {
    pub scope: ConnectorScope,
    pub stream_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnboardRequest {
    pub scope: ConnectorScope,
    /// Stable producer installation identity, matched to the operator grant.
    pub remote_authority: String,
    pub display_name: String,
}
