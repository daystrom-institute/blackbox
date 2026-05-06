pub mod config;
pub mod markdown;
pub mod text;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::entity_ref::EntityRef;

pub const MAX_CHUNK_BYTES: usize = 12 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub project_id: String,
    pub file_path: PathBuf,
    pub rel_path_hash: String,
    pub chunk_kind: String,
    pub chunk_hash: String,
    pub occurrence_idx: u32,
    pub language: Option<String>,
    pub content: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeProvenance {
    Explicit,
    Derived,
    Implicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeConfidence {
    Exact,
    Heuristic,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub provenance: EdgeProvenance,
    pub confidence: EdgeConfidence,
}

pub trait SourceFormatChunker: Send + Sync {
    fn format_id(&self) -> &str;
    fn claims(&self, path: &Path, sniff: &[u8]) -> bool;
    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)>;
}

pub fn default_registry() -> Vec<Box<dyn SourceFormatChunker>> {
    vec![
        Box::new(markdown::MarkdownChunker),
        Box::new(config::JsonChunker),
        Box::new(config::TomlChunker),
        Box::new(config::YamlChunker),
        Box::new(text::PlainTextChunker),
    ]
}

pub(crate) fn placeholder_chunk(
    path: &Path,
    chunk_kind: &str,
    language: Option<&str>,
    content: impl Into<String>,
    byte_start: u64,
    byte_end: u64,
    occurrence_idx: u32,
) -> Chunk {
    Chunk {
        project_id: String::new(),
        file_path: path.to_path_buf(),
        rel_path_hash: String::new(),
        chunk_kind: chunk_kind.to_string(),
        chunk_hash: String::new(),
        occurrence_idx,
        language: language.map(str::to_string),
        content: content.into(),
        byte_start,
        byte_end,
    }
}
