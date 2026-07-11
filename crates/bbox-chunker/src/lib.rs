pub mod av_transcript;
pub mod code;
pub mod config;
pub mod html;
pub mod ipynb;
pub mod markdown;
pub mod office;
pub mod pdf;
pub mod text;
pub mod xlsx;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use bbox_corpus_core::entity_ref::EntityRef;

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
    pub symbol: Option<String>,
    pub symbol_exact: Option<String>,
    /// Raw tree-sitter node kind for the symbol-producing node. Populated
    /// by code chunks built from a `SymbolSpec`; `None` for chunks
    /// without a tree-sitter kind (non-code, structure-pack fallback
    /// before tree-sitter parse, etc.). See CN-D1 in
    /// `design/archive/code-nav-symbolic-exploration-impl.md`.
    pub symbol_kind: Option<String>,
    /// Kind of the nearest enclosing symbol-producing ancestor, or
    /// `None` at file top level. Required for deterministic indexed
    /// `refactor_kind_for` derivation; see CN-T2.
    pub parent_kind: Option<String>,
    /// 1-based line of `byte_start` in the source. `None` for non-line
    /// oriented sources or chunks without source-line metadata.
    pub line_start: Option<u32>,
    /// 1-based line of `byte_end` in the source. `None` for non-line
    /// oriented sources or chunks without source-line metadata.
    pub line_end: Option<u32>,
    pub content: String,
    pub byte_start: u64,
    pub byte_end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeProvenance {
    Explicit,
    Derived,
    Implicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
        // HtmlChunker MUST claim before CodeChunker: `code::language_for_path`
        // maps .html/.htm to the "html" tree-sitter grammar (via
        // tree-sitter-language-pack), so CodeChunker's `claims()` already
        // matches those extensions and would otherwise win the `find()` in
        // `bbox-corpus-index`'s registry scan (first match wins), routing
        // markup through code-symbol extraction instead of prose sectioning.
        Box::new(html::HtmlChunker),
        Box::new(code::CodeChunker),
        Box::new(markdown::MarkdownChunker),
        Box::new(config::JsonChunker),
        Box::new(config::TomlChunker),
        Box::new(config::YamlChunker),
        Box::new(pdf::PdfChunker),
        Box::new(office::DocxChunker),
        Box::new(office::PptxChunker),
        Box::new(ipynb::IpynbChunker),
        Box::new(xlsx::XlsxChunker),
        Box::new(av_transcript::AvTranscriptChunker),
        Box::new(text::PlainTextChunker),
    ]
}

pub fn placeholder_chunk(
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
        symbol: None,
        symbol_exact: None,
        symbol_kind: None,
        parent_kind: None,
        line_start: None,
        line_end: None,
        content: content.into(),
        byte_start,
        byte_end,
    }
}
