use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, Query as QueryTrait, TermQuery};
use tantivy::schema::*;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

pub const INDEX_SCHEMA_VERSION: &str = "agentic-corpus-g5-symbol-tokenized";
const SCHEMA_VERSION_FILE: &str = "schema_version.txt";

/// Metadata about an indexed file, for incremental updates.
#[derive(Serialize, Deserialize)]
pub(super) struct FileMeta {
    pub(super) mtime: u64,
    pub(super) size: u64,
}

/// Field handles extracted for sharing with the background reindex thread.
/// All fields are `Copy` — they're just integer indices into the schema.
#[derive(Clone, Copy)]
pub struct FieldHandles {
    pub content: Field,
    pub session_id: Field,
    pub account: Field,
    pub project: Field,
    pub role: Field,
    pub timestamp: Field,
    pub file_path: Field,
    /// Tokenized variant of `file_path` indexed with the code tokenizer so
    /// path components like `src/embed/voyage.rs` split into `src`, `embed`,
    /// `voyage`, `rs`. Lets BM25 boost results whose file/symbol path matches
    /// the query — without this, a query for "voyage queue cap" can't prefer
    /// files literally named voyage.rs / queue.rs over arbitrary content matches.
    pub path_tokens: Field,
    pub byte_offset: Field,
    pub git_branch: Field,
    pub is_subagent: Field,
    pub agent_slug: Field,
    pub doc_type: Field,
    #[allow(dead_code)]
    pub chunk_kind: Field,
    #[allow(dead_code)]
    pub language: Field,
    #[allow(dead_code)]
    pub symbol: Field,
    #[allow(dead_code)]
    pub symbol_exact: Field,
    #[allow(dead_code)]
    pub code_content: Field,
    #[allow(dead_code)]
    pub chunk_hash: Field,
    #[allow(dead_code)]
    pub entity_id: Field,
    pub parser_version: Field,
    #[allow(dead_code)]
    pub commit_sha: Field,
    #[allow(dead_code)]
    pub repo_id: Field,
    #[allow(dead_code)]
    pub commit_author_name: Field,
    #[allow(dead_code)]
    pub commit_author_email: Field,
}

/// Config needed by the background reindex thread.
#[derive(Clone)]
pub struct ReindexConfig {
    pub roots: Vec<(String, PathBuf)>,
    pub codex_root: Option<PathBuf>,
    pub meta_path: PathBuf,
    pub projects_path: PathBuf,
    pub knowledge_path: PathBuf,
    pub threads_path: PathBuf,
    pub roadmap_path: PathBuf,
}

pub struct TranscriptIndex {
    index: Index,
    reader: IndexReader,
    #[allow(dead_code)]
    schema: Schema,
    fields: FieldHandles,
    config: ReindexConfig,
    /// TTL cache for `stats()` output. The expensive part of stats is
    /// walking every account's `projects/` tree — dominates the call
    /// time for a corpus of any size. Wrapped in an inner Mutex so
    /// stats() can mutate it through a shared `&TranscriptIndex`
    /// (the whole struct is already behind RwLock in SharedState).
    pub(super) stats_cache: Mutex<Option<(Instant, String)>>,
}

#[derive(Debug, Clone)]
pub(crate) struct EdgeProjectionDoc {
    pub doc_type: String,
    pub account: String,
    pub session_id: String,
    pub byte_offset: u64,
    pub file_path: String,
    pub entity_id: Option<String>,
}

impl EdgeProjectionDoc {
    pub fn project_file_occurrence_idx(&self) -> Option<u32> {
        let entity = self.entity_id.as_deref()?;
        match crate::entity_ref::EntityRef::parse(entity).ok()? {
            crate::entity_ref::EntityRef::ProjectFile { occurrence_idx, .. } => {
                Some(occurrence_idx)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EmbeddingSourceDoc {
    pub doc_type: String,
    pub account: String,
    pub session_id: String,
    pub project: String,
    pub file_path: String,
    pub byte_offset: u64,
    pub chunk_kind: String,
    pub language: Option<String>,
    pub symbol: Option<String>,
    pub symbol_exact: Option<String>,
    pub chunk_hash: Option<String>,
    pub entity_id: Option<String>,
    pub content: String,
}

impl TranscriptIndex {
    pub fn open_or_create(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
        knowledge_path: PathBuf,
        threads_path: PathBuf,
        roadmap_path: PathBuf,
    ) -> Result<Self> {
        reset_index_on_schema_mismatch(index_path)?;
        let meta_path = index_path.join("_meta.json");

        let (schema, fields) = build_schema();

        fs::create_dir_all(index_path)?;

        // Try opening existing index, fall back to creating new
        let index = match Index::open_in_dir(index_path) {
            Ok(idx) => {
                tracing::info!("Opened existing index at {}", index_path.display());
                idx
            }
            Err(_) => {
                tracing::info!("Creating new index at {}", index_path.display());
                Index::create_in_dir(index_path, schema.clone())?
            }
        };
        register_code_tokenizer(&index);
        write_schema_version_marker(index_path)?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path,
            projects_path,
            knowledge_path,
            threads_path,
            roadmap_path,
        };

        Ok(Self {
            index,
            reader,
            schema,
            fields,
            config,
            stats_cache: Mutex::new(None),
        })
    }

    /// Get a clone of the Index handle for the background thread.
    pub fn index_handle(&self) -> Index {
        self.index.clone()
    }

    /// Get the field handles for the background thread.
    pub fn field_handles(&self) -> FieldHandles {
        self.fields
    }

    /// Get the reindex config for the background thread.
    pub fn reindex_config(&self) -> ReindexConfig {
        self.config.clone()
    }

    pub(crate) fn index_knowledge_entry(
        &mut self,
        entry: &crate::knowledge::KnowledgeEntry,
    ) -> Result<()> {
        knowledge_docs::upsert_knowledge_entry(
            &self.index,
            self.fields,
            &self.config.knowledge_path,
            entry,
        )?;
        self.reader.reload()?;
        *self.stats_cache.lock() = None;
        Ok(())
    }

    pub(crate) fn delete_knowledge_entry(&mut self, entry_id: &str) -> Result<()> {
        knowledge_docs::delete_knowledge_entry(&self.index, self.fields, entry_id)?;
        self.reader.reload()?;
        *self.stats_cache.lock() = None;
        Ok(())
    }

    pub(crate) fn index_roadmap_item(&mut self, item: &crate::roadmap::RoadmapItem) -> Result<()> {
        roadmap_docs::upsert_roadmap_item(
            &self.index,
            self.fields,
            &self.config.roadmap_path,
            item,
        )?;
        self.reader.reload()?;
        *self.stats_cache.lock() = None;
        Ok(())
    }

    pub(crate) fn delete_roadmap_item(&mut self, item_id: &str) -> Result<()> {
        roadmap_docs::delete_roadmap_item(&self.index, self.fields, item_id)?;
        self.reader.reload()?;
        *self.stats_cache.lock() = None;
        Ok(())
    }

    pub(crate) fn index_threads_store(&mut self, threads: &crate::threads::Threads) -> Result<()> {
        thread_docs::upsert_threads_store(
            &self.index,
            self.fields,
            &self.config.threads_path,
            threads,
        )?;
        self.reader.reload()?;
        *self.stats_cache.lock() = None;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        let searcher = self.reader.searcher();
        searcher.num_docs() == 0
    }

    /// Current document count from a fresh searcher snapshot. Used by the
    /// edge-index rebuild watcher to detect when the corpus has grown.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub(crate) fn edge_projection_docs(&self) -> Result<Vec<EdgeProjectionDoc>> {
        let searcher = self.reader.searcher();
        let limit = searcher.num_docs() as usize;
        if limit > 100_000 {
            tracing::warn!(
                doc_count = limit,
                "EdgeIndex projection is materializing all index docs at startup"
            );
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        // NOTE: AllQuery + TopDocs::with_limit materializes every doc into memory at once.
        // Acceptable at <50k doc scale (current corpus); refactor to streaming/segment
        // iteration before this crosses ~100k.
        let top_docs = searcher.search(
            &tantivy::query::AllQuery,
            &tantivy::collector::TopDocs::with_limit(limit),
        )?;
        let mut docs = Vec::with_capacity(top_docs.len());
        for (_score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            docs.push(EdgeProjectionDoc {
                doc_type: first_text(&doc, self.fields.doc_type),
                account: first_text(&doc, self.fields.account),
                session_id: first_text(&doc, self.fields.session_id),
                byte_offset: first_u64(&doc, self.fields.byte_offset),
                file_path: first_text(&doc, self.fields.file_path),
                entity_id: optional_text(&doc, self.fields.entity_id),
            });
        }
        Ok(docs)
    }

    pub(crate) fn embedding_source_docs_for_doc_types(
        &self,
        doc_types: &[&str],
        max_docs: Option<usize>,
    ) -> Result<Vec<EmbeddingSourceDoc>> {
        let mut docs = Vec::new();
        self.for_each_embedding_source_doc_for_doc_types(doc_types, max_docs, |doc| {
            docs.push(doc);
            Ok(())
        })?;
        Ok(docs)
    }

    pub(crate) fn for_each_embedding_source_doc_for_doc_types<F>(
        &self,
        doc_types: &[&str],
        max_docs: Option<usize>,
        mut f: F,
    ) -> Result<usize>
    where
        F: FnMut(EmbeddingSourceDoc) -> Result<()>,
    {
        if doc_types.is_empty() {
            return Ok(0);
        }
        let searcher = self.reader.searcher();
        let query: Box<dyn QueryTrait> = if doc_types.len() == 1 {
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.doc_type, doc_types[0]),
                IndexRecordOption::Basic,
            ))
        } else {
            Box::new(BooleanQuery::new(
                doc_types
                    .iter()
                    .map(|doc_type| {
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.doc_type, doc_type),
                                IndexRecordOption::Basic,
                            )) as Box<dyn QueryTrait>,
                        )
                    })
                    .collect(),
            ))
        };
        let limit = match max_docs {
            Some(n) => n,
            None => searcher.search(&*query, &Count)?,
        };
        if limit == 0 {
            return Ok(0);
        }
        // TopDocs materializes (f32, DocAddress) pairs — ~12 bytes each.
        // That's small compared to loading full stored fields (~200+ bytes),
        // so we can afford to collect all addresses for large corpuses then
        // stream stored-field loads one at a time through the callback.
        let top_docs = searcher.search(&*query, &TopDocs::with_limit(limit))?;
        let mut emitted = 0usize;
        for (_score, addr) in top_docs {
            if max_docs.is_some_and(|max| emitted >= max) {
                break;
            }
            let doc: TantivyDocument = searcher.doc(addr)?;
            let doc_type = first_text(&doc, self.fields.doc_type);
            // Belt-and-suspenders: doc_type field should match the query,
            // but skip if it doesn't (stale segment, merge artifact, etc.).
            if !doc_types.iter().any(|wanted| *wanted == doc_type) {
                continue;
            }
            f(EmbeddingSourceDoc {
                doc_type,
                account: first_text(&doc, self.fields.account),
                session_id: first_text(&doc, self.fields.session_id),
                project: first_text(&doc, self.fields.project),
                file_path: first_text(&doc, self.fields.file_path),
                byte_offset: first_u64(&doc, self.fields.byte_offset),
                chunk_kind: first_text(&doc, self.fields.chunk_kind),
                language: optional_text(&doc, self.fields.language),
                symbol: optional_text(&doc, self.fields.symbol),
                symbol_exact: optional_text(&doc, self.fields.symbol_exact),
                chunk_hash: optional_text(&doc, self.fields.chunk_hash),
                entity_id: optional_text(&doc, self.fields.entity_id),
                content: first_text(&doc, self.fields.content),
            })?;
            emitted += 1;
        }
        Ok(emitted)
    }

    pub(crate) fn entity_properties(
        &self,
        entity_id: &str,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.entity_id, entity_id),
            IndexRecordOption::Basic,
        );
        let top_docs = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(1))?;
        let Some((_score, addr)) = top_docs.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(addr)?;
        Ok(Some(self.properties_from_doc(&doc)))
    }

    pub(crate) fn session_properties(
        &self,
        provider: &str,
        session_id: &str,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.session_id, session_id),
            IndexRecordOption::Basic,
        );
        for (_score, addr) in
            searcher.search(&query, &tantivy::collector::TopDocs::with_limit(100))?
        {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if optional_text(&doc, self.fields.account).as_deref() != Some(provider) {
                continue;
            }
            let mut properties = self.properties_from_doc(&doc);
            properties.insert("provider".into(), provider.to_string());
            properties.insert("session_id".into(), session_id.to_string());
            if let Some(content) = optional_text(&doc, self.fields.content) {
                properties.insert(
                    "first_user_prompt".into(),
                    content.chars().take(300).collect(),
                );
            }
            return Ok(Some(properties));
        }
        Ok(None)
    }

    pub(crate) fn transcript_properties(
        &self,
        provider: &str,
        session_id: &str,
        byte_offset: u64,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.session_id, session_id),
            IndexRecordOption::Basic,
        );
        for (_score, addr) in
            searcher.search(&query, &tantivy::collector::TopDocs::with_limit(500))?
        {
            let doc: TantivyDocument = searcher.doc(addr)?;
            if optional_text(&doc, self.fields.account).as_deref() != Some(provider) {
                continue;
            }
            if optional_u64(&doc, self.fields.byte_offset) != Some(byte_offset) {
                continue;
            }
            let mut properties = self.properties_from_doc(&doc);
            properties.insert("provider".into(), provider.to_string());
            properties.insert("session_id".into(), session_id.to_string());
            properties.insert("line_offset".into(), byte_offset.to_string());
            return Ok(Some(properties));
        }
        Ok(None)
    }

    fn properties_from_doc(&self, doc: &TantivyDocument) -> BTreeMap<String, String> {
        let mut properties = BTreeMap::new();
        for (name, field) in [
            ("doc_type", self.fields.doc_type),
            ("chunk_kind", self.fields.chunk_kind),
            ("language", self.fields.language),
            ("symbol", self.fields.symbol),
            ("symbol_exact", self.fields.symbol_exact),
            ("file_path", self.fields.file_path),
            ("repo_id", self.fields.repo_id),
            ("commit_sha", self.fields.commit_sha),
            ("commit_author_name", self.fields.commit_author_name),
            ("commit_author_email", self.fields.commit_author_email),
            ("role", self.fields.role),
        ] {
            if let Some(value) = optional_text(doc, field).filter(|value| !value.is_empty()) {
                properties.insert(name.to_string(), value);
            }
        }
        if let Some(content) = optional_text(doc, self.fields.content) {
            let preview = content.chars().take(300).collect::<String>();
            properties.insert("content_preview".into(), preview);
        }
        if let Some(offset) = optional_u64(doc, self.fields.byte_offset) {
            properties.insert("byte_offset".into(), offset.to_string());
        }
        properties
    }
}

fn first_text(doc: &TantivyDocument, field: Field) -> String {
    optional_text(doc, field).unwrap_or_default()
}

fn optional_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_all(field).next().and_then(|value| match value {
        tantivy::schema::OwnedValue::Str(text) => Some(text.clone()),
        _ => None,
    })
}

fn optional_u64(doc: &TantivyDocument, field: Field) -> Option<u64> {
    doc.get_first(field).and_then(|v| v.as_value().as_u64())
}

fn first_u64(doc: &TantivyDocument, field: Field) -> u64 {
    doc.get_all(field)
        .next()
        .and_then(|value| match value {
            tantivy::schema::OwnedValue::U64(n) => Some(*n),
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) fn build_schema() -> (Schema, FieldHandles) {
    let mut builder = Schema::builder();
    let code_options = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(code_tokenizer::CODE_TOKENIZER_NAME)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    let fields = FieldHandles {
        content: builder.add_text_field("content", TEXT | STORED),
        session_id: builder.add_text_field("session_id", STRING | STORED),
        account: builder.add_text_field("account", STRING | STORED),
        project: builder.add_text_field("project", TEXT | STORED),
        role: builder.add_text_field("role", STRING | STORED),
        timestamp: builder.add_text_field("timestamp", STRING | STORED),
        file_path: builder.add_text_field("file_path", STRING | STORED),
        path_tokens: builder.add_text_field("path_tokens", code_options.clone()),
        byte_offset: builder.add_u64_field("byte_offset", STORED),
        git_branch: builder.add_text_field("git_branch", STRING | STORED),
        is_subagent: builder.add_u64_field("is_subagent", INDEXED | STORED),
        agent_slug: builder.add_text_field("agent_slug", STRING | STORED),
        doc_type: builder.add_text_field("doc_type", STRING | STORED),
        chunk_kind: builder.add_text_field("chunk_kind", STRING | STORED),
        language: builder.add_text_field("language", STRING | STORED),
        // Use code_tokenizer on `symbol` so qualified names like
        // `Substrate.TriadClosure::handle_call` index as the union of their
        // camel-case and snake-case parts. Without this, a query for
        // `triad_closure` couldn't match a chunk whose elixir defmodule is
        // `Substrate.TriadClosure` (the default tokenizer keeps CamelCase
        // as one token, so the snake_case query never aligns).
        symbol: builder.add_text_field("symbol", code_options.clone()),
        symbol_exact: builder.add_text_field("symbol_exact", STRING | STORED),
        code_content: builder.add_text_field("code_content", code_options),
        chunk_hash: builder.add_text_field("chunk_hash", STRING | STORED),
        entity_id: builder.add_text_field("entity_id", STRING | STORED),
        parser_version: builder.add_text_field("parser_version", STRING | STORED),
        commit_sha: builder.add_text_field("commit_sha", STRING | STORED),
        repo_id: builder.add_text_field("repo_id", STRING | STORED),
        commit_author_name: builder.add_text_field("commit_author_name", TEXT | STORED),
        commit_author_email: builder.add_text_field("commit_author_email", STRING | STORED),
    };
    (builder.build(), fields)
}

pub(crate) fn register_code_tokenizer(index: &Index) {
    index.tokenizers().register(
        code_tokenizer::CODE_TOKENIZER_NAME,
        TextAnalyzer::from(code_tokenizer::CodeTokenizer::default()),
    );
}

fn reset_index_on_schema_mismatch(index_path: &Path) -> Result<()> {
    if !index_path.exists() {
        return Ok(());
    }
    let marker_path = index_path.join(SCHEMA_VERSION_FILE);
    let should_reset = match fs::read_to_string(&marker_path) {
        Ok(raw) => raw.trim() != INDEX_SCHEMA_VERSION,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            index_path.read_dir()?.next().is_some()
        }
        Err(err) => return Err(err.into()),
    };
    if should_reset {
        tracing::info!(
            path = %index_path.display(),
            schema_version = INDEX_SCHEMA_VERSION,
            "dropping transcript index for schema migration"
        );
        fs::remove_dir_all(index_path)?;
    }
    Ok(())
}

fn write_schema_version_marker(index_path: &Path) -> Result<()> {
    fs::write(
        index_path.join(SCHEMA_VERSION_FILE),
        format!("{INDEX_SCHEMA_VERSION}\n"),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_open_writes_schema_version_marker() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");

        let _index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();

        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_mismatch_drops_existing_index_directory() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();

        let _index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();

        assert!(!index_path.join("stale-file").exists());
        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn code_content_tokenizer_finds_identifier_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();
        let project = crate::projects::ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
        };
        let chunk = crate::chunker::Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("rust".into()),
            symbol: Some("KnowledgeStore".into()),
            symbol_exact: Some("KnowledgeStore".into()),
            content: "pub struct KnowledgeStore;".into(),
            byte_start: 0,
            byte_end: 26,
        };
        let doc = project_files::build_project_file_doc(
            &chunk,
            &project,
            Path::new("/tmp/repo/src/lib.rs"),
            Some("a".repeat(40).as_str()),
            index.field_handles(),
        );
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        index.reader.reload().unwrap();

        let result = index
            .search(&SearchParams {
                query: "Knowledge".into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                exclude_self: None,
            })
            .unwrap();
        assert!(result.contains("/tmp/repo/src/lib.rs"), "{result}");
    }

    #[test]
    fn delete_knowledge_entry_removes_tantivy_doc() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let knowledge_path = dir.path().join("knowledge.json");
        let mut index = TranscriptIndex::open_or_create(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            knowledge_path.clone(),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();
        let entry = crate::knowledge::KnowledgeEntry {
            id: "abc12345".into(),
            title: "Delete fixture".into(),
            content: "tombstone searchable knowledge phrase".into(),
            cluster: None,
            variants: Default::default(),
            category: crate::knowledge::Category::Memory,
            scope: crate::knowledge::Scope::Global,
            project: None,
            providers: Vec::new(),
            priority: crate::knowledge::Priority::Standard,
            weight: 100,
            status: crate::knowledge::Status::Active,
            approval: crate::knowledge::Approval::UserConfirmed,
            render: true,
            decay: true,
            review_at: None,
            supersedes: None,
            links: Vec::new(),
            rationale: None,
            expires_at: None,
            source: "test".into(),
            created_at: "2026-05-05T17:30:00Z".into(),
            updated_at: "2026-05-05T17:30:00Z".into(),
            recall_count: 0,
            last_recalled: None,
        };

        index.index_knowledge_entry(&entry).unwrap();
        let hits = index
            .search(&SearchParams {
                query: "tombstone searchable".into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                exclude_self: None,
            })
            .unwrap();
        assert!(hits.contains("tombstone"), "{hits}");
        assert!(hits.contains("searchable"), "{hits}");

        index.delete_knowledge_entry("abc12345").unwrap();
        let hits = index
            .search(&SearchParams {
                query: "tombstone searchable".into(),
                mode: None,
                account: None,
                project: None,
                role: None,
                include_subagents: None,
                limit: Some(5),
                exclude_self: None,
            })
            .unwrap();
        assert!(
            hits == "No results found." || hits == "Index is empty. Run blackbox_reindex first.",
            "{hits}"
        );
    }
}

mod code_tokenizer;
mod git_history;
mod helpers;
mod knowledge_docs;
mod project_files;
mod reindex;
mod roadmap_docs;
mod search;
mod thread_docs;
mod tool_edges;

pub use helpers::find_session_file;
pub(crate) use knowledge_docs::{knowledge_chunk_hash, knowledge_entity_id};
pub use reindex::spawn_reindex_thread;
pub(crate) use roadmap_docs::{roadmap_chunk_hash, roadmap_entity_id};
pub use search::{
    CiteParams, ContextParams, HybridBm25Hit, MessagesParams, ReindexParams, SearchParams,
    SessionParams, SessionsListParams, TopicsParams,
};

pub(crate) fn resolve_current_project_chunk_entity(
    project: &crate::projects::ProjectRecord,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<crate::entity_ref::EntityRef>> {
    project_files::resolve_current_chunk_entity(project, root, absolute_path, byte_range)
}
