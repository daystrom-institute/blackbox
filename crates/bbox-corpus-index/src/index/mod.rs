use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use bbox_corpus_core::project_record::{ProjectRecordsProvider, ProjectRecordsSnapshot};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query as QueryTrait, TermQuery};
use tantivy::schema::*;
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

pub const INDEX_SCHEMA_VERSION: &str = "agentic-corpus-g11-path-free-project-files";
const SCHEMA_VERSION_FILE: &str = "schema_version.txt";

/// Metadata about an indexed file, for incremental updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: u64,
    pub size: u64,
    /// Materialization version under which this file's derived edges were last
    /// produced (project files only; `None` for transcripts/store docs and for
    /// entries written before this field existed). When it differs from
    /// `snapshot::current_materialization_version()` the project indexer must
    /// re-chunk even if mtime/size are unchanged, so a chunker/indexer/parser
    /// version bump never leaves stale edges in a freshly-keyed snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mat_version: Option<String>,
    #[serde(default)]
    pub source: FileMetaSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileMetaSource {
    #[default]
    LegacyFilesystem,
    LocalProjectFile {
        project_id: String,
        selector: String,
        relative_path: String,
        entry_key: String,
    },
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
    /// Normalized, slash-separated project-relative path (P3-E, governing
    /// section 10.2). On a project-file document this is the same value
    /// `file_path` carries; it exists as its own field so the response
    /// boundary can return the structured triple without re-deriving
    /// anything from a display string, and so a future field-level cut of
    /// `file_path` needs no reader change.
    pub relative_path: Field,
    /// Stable machine identifier `bbox://project/<project_id>/<encoded>`
    /// (`bbox_code_source::encode_source_uri`). Never changes when aliases
    /// or attachments change.
    pub source_uri: Field,
    /// `local` / `collected`, derived from the selector
    /// (`bbox_code_source::source_kind_for_selector`).
    pub source_kind: Field,
    pub code_source_selector: Field,
    pub code_source_generation: Field,
    pub code_source_entry_key: Field,
    /// Tokenized variant of `file_path` indexed with the code tokenizer so
    /// path components like `src/embed/voyage.rs` split into `src`, `embed`,
    /// `voyage`, `rs`. Lets BM25 boost results whose file/symbol path matches
    /// the query — without this, a query for "voyage queue cap" can't prefer
    /// files literally named voyage.rs / queue.rs over arbitrary content matches.
    pub path_tokens: Field,
    pub byte_offset: Field,
    /// End byte of the chunk in the source file (CN-D3). Stored so the
    /// indexed code_symbols lane can return a `byte_range: (start,
    /// end)` tuple matching the live lane without re-reading the
    /// source file.
    #[allow(dead_code)]
    pub byte_end: Field,
    /// 1-based start line in the source file (CN-D3). Sourced from
    /// `Chunk.line_start`.
    #[allow(dead_code)]
    pub line_start: Field,
    /// 1-based end line in the source file (CN-D3). Sourced from
    /// `Chunk.line_end`.
    #[allow(dead_code)]
    pub line_end: Field,
    pub git_branch: Field,
    pub is_subagent: Field,
    pub agent_slug: Field,
    pub doc_type: Field,
    /// Project ID as a queryable STRING term (CN-D3). Previously a
    /// project_file doc carried the canonical path + entity_id but no
    /// fast `project_id` filter; the indexed code_symbols lane needs
    /// one. Populated whenever the chunk has a non-empty
    /// `chunk.project_id`.
    #[allow(dead_code)]
    pub project_id: Field,
    /// Resolved base-project id stamped on transcript and tool_call docs at
    /// ingest (gap-72fd5932): the registered project owning the session's
    /// cwd, including any worktree of it. Lets a project filter match work
    /// from every checkout while `project` keeps the literal session cwd.
    pub base_project_id: Field,
    #[allow(dead_code)]
    pub chunk_kind: Field,
    #[allow(dead_code)]
    pub language: Field,
    #[allow(dead_code)]
    pub symbol: Field,
    #[allow(dead_code)]
    pub symbol_exact: Field,
    /// Raw tree-sitter node kind for the chunk's symbol (CN-D3). See
    /// CN-D1 / CN-D2 for how the value is sourced. STRING-tokenized
    /// so kind filters are exact-match.
    #[allow(dead_code)]
    pub symbol_kind: Field,
    /// Kind of the nearest enclosing symbol-producing ancestor (CN-D3).
    /// Required so the indexed code_symbols lane can derive
    /// `refactor_kind_for(language, symbol_kind, parent_kind)` without
    /// re-parsing.
    #[allow(dead_code)]
    pub parent_kind: Field,
    #[allow(dead_code)]
    pub code_content: Field,
    #[allow(dead_code)]
    pub chunk_hash: Field,
    #[allow(dead_code)]
    pub entity_id: Field,
    pub logical_ref: Field,
    pub knowledge_visibility: Field,
    pub knowledge_scope_hash: Field,
    pub knowledge_checkout_id: Field,
    pub knowledge_snapshot_id: Field,
    pub parser_version: Field,
    #[allow(dead_code)]
    pub commit_sha: Field,
    #[allow(dead_code)]
    pub repo_id: Field,
    #[allow(dead_code)]
    pub commit_author_name: Field,
    #[allow(dead_code)]
    pub commit_author_email: Field,
    pub tool_server: Field,
    pub tool_name: Field,
    pub tool_kind: Field,
    pub tool_target: Field,
    pub tool_outcome: Field,
    pub task_id: Field,
    pub tool_use_id: Field,
}

/// Config needed by the background reindex thread.
#[derive(Clone)]
pub struct ReindexConfig {
    pub roots: Vec<(String, PathBuf)>,
    pub codex_root: Option<PathBuf>,
    pub meta_path: PathBuf,
    pub projects_path: PathBuf,
    pub code_source_store_path: PathBuf,
    pub knowledge_path: PathBuf,
    pub threads_path: PathBuf,
    pub roadmap_path: PathBuf,
    /// Standalone harness process sessions dir (`$BRO_HOME/harness-sessions`) whose
    /// sidecar event logs are indexed via the transcript adapter registry.
    /// `None` (the default, and what hermetic tests get) disables the
    /// harness-sessions adapter entirely — reindex must never silently scan
    /// the operator's real harness state. Set by the daemon at startup via
    /// [`TranscriptIndex::set_harness_sessions_dir`].
    pub harness_sessions_dir: Option<PathBuf>,
    /// Gemini CLI tmp root (`~/.gemini/tmp` or `GEMINI_TMP_ROOT`) whose chat
    /// JSON files are indexed via the gemini interactive adapter. Same
    /// hermetic-by-default contract as `harness_sessions_dir`: `None`
    /// disables the adapter. Set via [`TranscriptIndex::set_gemini_tmp_root`].
    pub gemini_tmp_root: Option<PathBuf>,
}

pub struct TranscriptIndex {
    index_path: PathBuf,
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
    /// (the whole struct is already behind RwLock in SharedState), and
    /// in an Arc so the IndexWriterActor can invalidate it post-commit.
    pub stats_cache: StatsCache,
    active_code_selectors: std::sync::Arc<RwLock<BTreeMap<String, String>>>,
    /// Injected project authority. Every selector derivation reads a fresh
    /// snapshot from it, so this index never reads `projects.json` off disk.
    records_provider: std::sync::Arc<dyn ProjectRecordsProvider>,
    /// What the open did at the replacement boundary, and whether it withheld
    /// the schema marker. Both facts are read by the shared replacement driver
    /// to classify what it must do; neither is recoverable afterwards, because
    /// the drive itself changes the on-disk state they describe.
    replacement: schema_replacement::IndexReplacementOutcomeV1,
}

/// Fixed-snapshot [`ProjectRecordsProvider`] for offline and test callers that
/// have no live authority to derive from.
pub struct StaticProjectRecordsProvider {
    snapshot: ProjectRecordsSnapshot,
}

impl StaticProjectRecordsProvider {
    pub fn new(snapshot: ProjectRecordsSnapshot) -> Self {
        Self { snapshot }
    }

    /// Frozen provider over an EMPTY authority, for the offline and test lanes
    /// that never resolve a project identity.
    ///
    /// The `projects.json`-reading constructor this replaces was the last
    /// direct `load_project_records` consumer (Phase 6 plan section 5.2). A
    /// caller that does need records supplies them explicitly through
    /// [`StaticProjectRecordsProvider::from_bridge_records`]; project identity
    /// is never re-derived from a host path.
    pub fn empty() -> Self {
        Self::new(ProjectRecordsSnapshot::empty())
    }

    /// Frozen provider over explicit catalog-bridge records (FD-8 retains the
    /// bridge decode path). Callers hold the records already; nothing here
    /// reads a host path.
    pub fn from_bridge_records(
        records: Vec<bbox_corpus_core::project_record::ProjectRecord>,
        authority_epoch: u64,
    ) -> Self {
        Self::new(ProjectRecordsSnapshot::from_bridge_records(
            records,
            authority_epoch,
        ))
    }
}

impl ProjectRecordsProvider for StaticProjectRecordsProvider {
    fn records_snapshot(&self) -> ProjectRecordsSnapshot {
        self.snapshot.clone()
    }
}

/// Shared stats TTL cache; the writer actor clears it after every commit.
pub type StatsCache = std::sync::Arc<Mutex<Option<(Instant, String)>>>;

#[derive(Debug, Clone)]
pub struct EdgeProjectionDoc {
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
        match bbox_corpus_core::entity_ref::EntityRef::parse(entity).ok()? {
            bbox_corpus_core::entity_ref::EntityRef::ProjectFile { occurrence_idx, .. }
            | bbox_corpus_core::entity_ref::EntityRef::ProjectFileV2 { occurrence_idx, .. } => {
                Some(occurrence_idx)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingSourceDoc {
    pub doc_type: String,
    pub account: String,
    pub session_id: String,
    /// The stored `project` field. After the P3-E cut this is the identity's
    /// DISPLAY NAME on a project-file document, never a checkout path, which
    /// is what makes it usable as the backfill lane's prepend value.
    pub project: String,
    pub file_path: String,
    /// The stored `relative_path` field. Empty on document kinds that do not
    /// carry one (transcripts, store docs).
    pub relative_path: String,
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
    /// Test and offline construction path only: opens with an EXPLICIT project
    /// authority. The daemon opens the index through
    /// [`TranscriptIndex::open_or_create_with_code_source_store_path`] with a
    /// live [`ProjectRecordsProvider`].
    ///
    /// The `projects.json`-derived predecessor of this constructor was deleted
    /// in Phase 6 (plan section 5.2/5.3): no lane may reconstruct a project
    /// authority by reading a host path. `projects_path` survives as a state
    /// LOCATION (it still anchors the code-source store and the edge sidecar),
    /// never as an identity source.
    ///
    /// No guard is injected here, so this path REFUSES a destructive schema
    /// replacement (P3-E fail-closed contract). A fresh index directory never
    /// triggers one; a test or offline caller that intends a replacement must
    /// use [`TranscriptIndex::open_or_create_guarded`].
    #[allow(clippy::too_many_arguments)]
    pub fn open_or_create_with_records(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
        knowledge_path: PathBuf,
        threads_path: PathBuf,
        roadmap_path: PathBuf,
        records_provider: std::sync::Arc<dyn ProjectRecordsProvider>,
    ) -> Result<Self> {
        Self::open_or_create_guarded(
            index_path,
            roots,
            codex_root,
            projects_path,
            knowledge_path,
            threads_path,
            roadmap_path,
            records_provider,
            None,
        )
    }

    /// [`TranscriptIndex::open_or_create_with_records`] with an explicit
    /// pre-replacement guard, for the test and offline lanes that exercise the
    /// replacement boundary itself.
    #[allow(clippy::too_many_arguments)]
    pub fn open_or_create_guarded(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
        knowledge_path: PathBuf,
        threads_path: PathBuf,
        roadmap_path: PathBuf,
        records_provider: std::sync::Arc<dyn ProjectRecordsProvider>,
        schema_replacement_guard: Option<schema_replacement::SchemaReplacementGuard>,
    ) -> Result<Self> {
        let code_source_store_path = projects_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("code-sources");
        Self::open_or_create_with_code_source_store_path(
            index_path,
            roots,
            codex_root,
            projects_path,
            code_source_store_path,
            knowledge_path,
            threads_path,
            roadmap_path,
            records_provider,
            schema_replacement_guard,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_or_create_with_code_source_store_path(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
        code_source_store_path: PathBuf,
        knowledge_path: PathBuf,
        threads_path: PathBuf,
        roadmap_path: PathBuf,
        records_provider: std::sync::Arc<dyn ProjectRecordsProvider>,
        schema_replacement_guard: Option<schema_replacement::SchemaReplacementGuard>,
    ) -> Result<Self> {
        Self::open_or_create_at_replacement_boundary(
            index_path,
            roots,
            codex_root,
            projects_path,
            code_source_store_path,
            knowledge_path,
            threads_path,
            roadmap_path,
            records_provider,
            schema_replacement_guard,
            schema_replacement::CatalogReplacementIntentV1::MismatchOnly,
        )
    }

    /// The open that carries an explicit replacement intent (Q-F).
    ///
    /// Separate from the constructor above rather than an eleventh positional
    /// argument on it: the callers that have classified rebuild recovery, or
    /// that hold an operator authorization, are a small and deliberately
    /// named set, and every other caller should keep getting
    /// `MismatchOnly` without having to know the vocabulary exists.
    #[allow(clippy::too_many_arguments)]
    pub fn open_or_create_at_replacement_boundary(
        index_path: &Path,
        roots: Vec<(String, PathBuf)>,
        codex_root: Option<PathBuf>,
        projects_path: PathBuf,
        code_source_store_path: PathBuf,
        knowledge_path: PathBuf,
        threads_path: PathBuf,
        roadmap_path: PathBuf,
        records_provider: std::sync::Arc<dyn ProjectRecordsProvider>,
        schema_replacement_guard: Option<schema_replacement::SchemaReplacementGuard>,
        replacement_intent: schema_replacement::CatalogReplacementIntentV1,
    ) -> Result<Self> {
        let replacement = reset_index_on_schema_mismatch(
            index_path,
            &projects_path,
            &code_source_store_path,
            schema_replacement_guard.as_ref(),
            replacement_intent,
        )?;
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
        // THE MARKER IS PUBLISHED LAST, or not at all here. A replacement in
        // flight - freshly performed above, or evidenced by a manifest that
        // survived the drop - withholds it until the re-emission pass and the
        // manifest commit have both landed. Publishing it here would declare a
        // replacement complete before a single document was re-emitted, and
        // would erase the only signal a later recovery reads.
        if !replacement.marker_withheld {
            write_schema_version_marker(index_path)?;
        }

        // Bridge commit carryover, consumed at EVERY open when present and
        // BEFORE the reader below binds (plan section 9 item 2). Not gated on
        // `schema_was_reset`: the mismatch trigger fires once, so a crash after
        // the drop leaves no mismatch on the next open and a gated consumer
        // would never run. The re-add is delete-term-then-add and the spill file
        // is removed only after the commit, so replaying is safe and losing the
        // population is not possible.
        let carried_commits =
            schema_replacement::consume_commit_spill_if_present(index_path, &index, fields)?;
        if carried_commits > 0 {
            tracing::info!(
                commit_documents = carried_commits,
                "carried commit documents across the index replacement"
            );
        }

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&projects_path);
        project_files::recover_pending_local_snapshot_activations(
            &reader.searcher(),
            fields,
            &edges_dir,
        )?;

        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path,
            projects_path,
            code_source_store_path,
            knowledge_path,
            threads_path,
            roadmap_path,
            harness_sessions_dir: None,
            gemini_tmp_root: None,
        };
        let active_code_selectors = load_active_code_selectors(
            &records_provider.records_snapshot().corpus_project_ids,
            &config.projects_path,
        )?;

        Ok(Self {
            index_path: index_path.to_path_buf(),
            index,
            reader,
            schema,
            fields,
            config,
            stats_cache: std::sync::Arc::new(Mutex::new(None)),
            active_code_selectors: std::sync::Arc::new(RwLock::new(active_code_selectors)),
            records_provider,
            replacement,
        })
    }

    /// Clone the shared `IndexReader` handle (writer-actor post-commit
    /// reloads go through this).
    pub fn reader_handle(&self) -> IndexReader {
        self.reader.clone()
    }

    /// Clone the stats TTL-cache handle (writer-actor post-commit
    /// invalidation goes through this).
    pub fn stats_cache_handle(&self) -> StatsCache {
        self.stats_cache.clone()
    }

    /// Get a clone of the Index handle for the background thread.
    pub fn index_handle(&self) -> Index {
        self.index.clone()
    }

    /// Canonical store path this handle actually opened. Runtime
    /// orchestrators must derive sibling durable assets from this value, not
    /// re-read configuration that test or embedded callers may override.
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// Snapshot a searcher off the shared `IndexReader`. Cheap — the
    /// reader is `OnCommit`-driven and segment-arc-cloned. Use this
    /// from per-call tool handlers instead of
    /// `reader_builder().try_into()` (which builds a fresh reader and
    /// forces per-call segment loads).
    pub fn searcher(&self) -> tantivy::Searcher {
        self.reader.searcher()
    }

    /// Force the shared reader to pick up newly committed segments.
    /// Production paths rely on `ReloadPolicy::OnCommit` so reload
    /// happens automatically; tests that commit + immediately query
    /// in the same thread can hit a race where the reader hasn't
    /// observed the commit yet. Call this in tests after `commit()`.
    /// Un-gated (no `cfg(test)`) so consumer-crate tests can use it —
    /// `cfg(test)` does not cross crate boundaries.
    pub fn reader_reload_for_test(&self) {
        let _ = self.reader.reload();
    }

    /// Get the field handles for the background thread.
    pub fn field_handles(&self) -> FieldHandles {
        self.fields
    }

    pub fn active_code_selectors(&self) -> BTreeMap<String, String> {
        self.active_code_selectors.read().clone()
    }

    pub fn schema_was_reset(&self) -> bool {
        self.replacement.performed.is_some()
    }

    /// WHY this open replaced the index, or `None` if it did not (Q-F).
    pub fn replacement_cause(&self) -> Option<schema_replacement::CatalogIndexReplacementCause> {
        self.replacement.performed
    }

    /// Whether the open withheld the schema marker.
    ///
    /// True on a fresh replacement, and also on an open that preserved an
    /// index whose marker a PREVIOUS process withheld. The second case is how
    /// "manifest committed, marker never published" is told apart from an
    /// ordinary steady-state boot, and it is why the marker is never published
    /// speculatively at open.
    pub fn schema_marker_withheld(&self) -> bool {
        self.replacement.marker_withheld
    }

    pub fn complete_schema_migration(&self) -> Result<()> {
        write_schema_version_marker(&self.index_path)
    }

    pub fn refresh_active_code_selectors(&self) -> Result<BTreeMap<String, String>> {
        let selectors = load_active_code_selectors(
            &self.records_provider.records_snapshot().corpus_project_ids,
            &self.config.projects_path,
        )?;
        self.replace_active_code_selectors(selectors.clone());
        Ok(selectors)
    }

    pub fn replace_active_code_selectors(&self, selectors: BTreeMap<String, String>) {
        *self.active_code_selectors.write() = selectors;
    }

    pub fn active_code_selector(&self, project_id: &str) -> Option<String> {
        self.active_code_selectors.read().get(project_id).cloned()
    }

    pub fn active_code_source_query(&self) -> Option<Box<dyn QueryTrait>> {
        let selectors = self.active_code_selectors.read().clone();
        self.active_code_source_query_for(&selectors)
    }

    pub fn active_code_source_query_for(
        &self,
        selectors: &BTreeMap<String, String>,
    ) -> Option<Box<dyn QueryTrait>> {
        let mut lanes: Vec<(Occur, Box<dyn QueryTrait>)> = vec![(
            Occur::Should,
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery)),
                (
                    Occur::MustNot,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.doc_type, "project_file"),
                        IndexRecordOption::Basic,
                    )),
                ),
            ])),
        )];
        lanes.extend(selectors.values().map(|selector| {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.code_source_selector, selector),
                    IndexRecordOption::Basic,
                )) as Box<dyn QueryTrait>,
            )
        }));
        Some(Box::new(BooleanQuery::new(lanes)))
    }

    /// Per-hit code-activity probe. Both the selector map and the searcher
    /// are caller-supplied ON PURPOSE (Phase 3 plan section 4.5): the daemon
    /// passes its pinned `CodeReadView`, so a vector hit is filtered against
    /// exactly the index generation and selector snapshot the rest of the
    /// request saw. The former live-state variants (`is_active_code_entity`,
    /// `is_active_code_entity_for`) read `self.active_code_selectors` and
    /// minted a fresh searcher per call; they are removed rather than
    /// deprecated, because their only failure mode was silent (a hit
    /// filtered against a newer generation than the one that produced it).
    pub fn is_active_code_entity_for_with_searcher(
        &self,
        entity_id: &str,
        selectors: &BTreeMap<String, String>,
        searcher: &tantivy::Searcher,
    ) -> bool {
        use bbox_corpus_core::entity_ref::EntityRef;

        match EntityRef::parse(entity_id) {
            Ok(EntityRef::ProjectFile { project_id, .. })
            | Ok(EntityRef::Symbol { project_id, .. })
            | Ok(EntityRef::ProjectFileV2 { project_id, .. })
            | Ok(EntityRef::SymbolV2 { project_id, .. }) => {
                let Some(active) = selectors.get(&project_id) else {
                    return false;
                };
                let query = BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.entity_id, entity_id),
                            IndexRecordOption::Basic,
                        )),
                    ),
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.code_source_selector, active),
                            IndexRecordOption::Basic,
                        )),
                    ),
                ]);
                searcher
                    .search(&query, &tantivy::collector::Count)
                    .is_ok_and(|count| count > 0)
            }
            _ => true,
        }
    }

    /// Resolve a legacy provenance `(relative_path, byte_range)` against one
    /// caller-pinned code generation. No checkout path is consulted: both
    /// the selector map and searcher come from the daemon's coherent
    /// `CodeReadView`.
    pub fn resolve_project_chunk_for_selector_with_searcher(
        &self,
        project_id: &str,
        selector: &str,
        relative_path: &str,
        byte_range: Option<(u64, u64)>,
        searcher: &tantivy::Searcher,
    ) -> Result<Option<bbox_corpus_core::entity_ref::EntityRef>> {
        use bbox_corpus_core::entity_ref::EntityRef;

        bbox_code_source::validate_relative_path(relative_path).map_err(anyhow::Error::new)?;
        if byte_range.is_some_and(|(start, end)| start > end) {
            anyhow::bail!("invalid provenance byte range");
        }
        let query = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.code_source_selector, selector),
                    IndexRecordOption::Basic,
                )),
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.relative_path, relative_path),
                    IndexRecordOption::Basic,
                )),
            ),
        ]);
        let count = searcher.search(&query, &Count)?;
        if count == 0 {
            return Ok(None);
        }
        if count > 100_000 {
            anyhow::bail!("one active project file exceeds the provenance resolution limit");
        }
        let mut candidates = Vec::with_capacity(count);
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<TantivyDocument>(address)?;
            let entity = document
                .get_first(self.fields.entity_id)
                .and_then(|value| match value {
                    OwnedValue::Str(value) => Some(value.as_str()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("active project-file document has no entity id"))
                .and_then(|value| EntityRef::parse(value).map_err(anyhow::Error::new))?;
            let occurrence_idx = match &entity {
                EntityRef::ProjectFileV2 {
                    project_id: target_project,
                    occurrence_idx,
                    ..
                } if target_project == project_id => *occurrence_idx,
                EntityRef::ProjectFileV2 { .. } => {
                    anyhow::bail!("active selector contains a foreign project-file identity")
                }
                _ => anyhow::bail!("active selector contains a non-V2 project-file identity"),
            };
            let byte_start = document
                .get_first(self.fields.byte_offset)
                .and_then(|value| value.as_u64())
                .ok_or_else(|| anyhow::anyhow!("active project-file document has no byte start"))?;
            let byte_end = document
                .get_first(self.fields.byte_end)
                .and_then(|value| value.as_u64())
                .ok_or_else(|| anyhow::anyhow!("active project-file document has no byte end"))?;
            candidates.push((occurrence_idx, byte_start, byte_end, entity));
        }
        candidates.sort_by_key(|candidate| candidate.0);
        let selected = byte_range
            .and_then(|(start, _)| {
                candidates
                    .iter()
                    .find(|(_, byte_start, byte_end, _)| *byte_start <= start && start <= *byte_end)
            })
            .or_else(|| candidates.first());
        Ok(selected.map(|(_, _, _, entity)| entity.clone()))
    }

    /// Get the reindex config for the background thread.
    pub fn reindex_config(&self) -> ReindexConfig {
        self.config.clone()
    }

    /// Enable harness-session event-log indexing from `dir`
    /// (`$BRO_HOME/harness-sessions`). Called by daemon startup; left unset
    /// (and therefore disabled) in hermetic test indexes.
    pub fn set_harness_sessions_dir(&mut self, dir: PathBuf) {
        self.config.harness_sessions_dir = Some(dir);
    }

    /// Enable interactive Gemini chat indexing from `tmp_root`. Called by
    /// daemon startup; left unset (disabled) in hermetic test indexes.
    pub fn set_gemini_tmp_root(&mut self, tmp_root: PathBuf) {
        self.config.gemini_tmp_root = Some(tmp_root);
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

    /// Cheap count of docs matching a single `doc_type` term, via a
    /// `TermQuery` + `Count` collector -- no stored-doc streaming. Used by
    /// `bbox_describe_schema`'s transcript vertex count: transcript entities
    /// are deliberately excluded from `EdgeIndex::entity_type_counts_active`
    /// (they're an observed history lane, not part of the active knowledge
    /// graph), so describe_schema needs a tantivy-backed count instead of an
    /// edge-index one.
    pub fn doc_type_count(&self, doc_type: &str) -> Result<usize> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.doc_type, doc_type),
            IndexRecordOption::Basic,
        );
        Ok(searcher.search(&query, &Count)?)
    }

    /// Stream every doc's edge-projection fields through `f`, walking each
    /// segment's doc store in storage order (sequential block decompression,
    /// deleted docs skipped via the alive bitset). One decompressed block and
    /// one projected doc are live at a time, so memory stays flat regardless
    /// of corpus size — unlike the previous AllQuery + TopDocs::with_limit
    /// implementation, which built an O(N) score heap and materialized every
    /// doc into a single Vec.
    pub fn for_each_edge_projection_doc<F>(&self, mut f: F) -> Result<usize>
    where
        F: FnMut(EdgeProjectionDoc) -> Result<()>,
    {
        let searcher = self.reader.searcher();
        let mut emitted = 0usize;
        for segment_reader in searcher.segment_readers() {
            // Sequential scan never revisits a block; the minimum cache works.
            let store_reader = segment_reader.get_store_reader(1)?;
            for doc in store_reader.iter::<TantivyDocument>(segment_reader.alive_bitset()) {
                let doc = doc?;
                f(EdgeProjectionDoc {
                    doc_type: first_text(&doc, self.fields.doc_type),
                    account: first_text(&doc, self.fields.account),
                    session_id: first_text(&doc, self.fields.session_id),
                    byte_offset: first_u64(&doc, self.fields.byte_offset),
                    file_path: first_text(&doc, self.fields.file_path),
                    entity_id: optional_text(&doc, self.fields.entity_id),
                })?;
                emitted += 1;
            }
        }
        Ok(emitted)
    }

    pub fn embedding_source_docs_for_doc_types(
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

    pub fn for_each_embedding_source_doc_for_doc_types<F>(
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
        let mut query: Box<dyn QueryTrait> = if doc_types.len() == 1 {
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
        if let Some(active) = self.active_code_source_query() {
            query = Box::new(BooleanQuery::new(vec![
                (Occur::Must, query),
                (Occur::Must, active),
            ]));
        }
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
                ..self.embedding_source_doc_from_doc(&doc)
            })?;
            emitted += 1;
        }
        Ok(emitted)
    }

    pub fn embedding_source_doc_for_entity_id(
        &self,
        entity_id: &str,
    ) -> Result<Option<EmbeddingSourceDoc>> {
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
        Ok(Some(self.embedding_source_doc_from_doc(&doc)))
    }

    fn embedding_source_doc_from_doc(&self, doc: &TantivyDocument) -> EmbeddingSourceDoc {
        EmbeddingSourceDoc {
            doc_type: first_text(doc, self.fields.doc_type),
            account: first_text(doc, self.fields.account),
            session_id: first_text(doc, self.fields.session_id),
            project: first_text(doc, self.fields.project),
            file_path: first_text(doc, self.fields.file_path),
            relative_path: first_text(doc, self.fields.relative_path),
            byte_offset: first_u64(doc, self.fields.byte_offset),
            chunk_kind: first_text(doc, self.fields.chunk_kind),
            language: optional_text(doc, self.fields.language),
            symbol: optional_text(doc, self.fields.symbol),
            symbol_exact: optional_text(doc, self.fields.symbol_exact),
            chunk_hash: optional_text(doc, self.fields.chunk_hash),
            entity_id: optional_text(doc, self.fields.entity_id),
            content: first_text(doc, self.fields.content),
        }
    }

    pub fn entity_properties(&self, entity_id: &str) -> Result<Option<BTreeMap<String, String>>> {
        let searcher = self.reader.searcher();
        self.entity_properties_with_searcher(entity_id, &searcher)
    }

    pub fn entity_properties_with_searcher(
        &self,
        entity_id: &str,
        searcher: &tantivy::Searcher,
    ) -> Result<Option<BTreeMap<String, String>>> {
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

    pub fn session_properties(
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

    pub fn transcript_properties(
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
        // byte_offset is STORED-only (not INDEXED -- see build_schema), so
        // there's no query term to match it exactly; every doc in the
        // session has to be checked in memory. DocSetCollector (unscored,
        // unbounded) is load-bearing here: a scored TopDocs::with_limit(N)
        // caps the candidate set at N docs in an arbitrary tie-broken order
        // (every doc in a STRING-field TermQuery scores identically), so any
        // session with more than N chunks silently drops matches beyond the
        // cap. That's why bbox_inspect_entity 404'd real transcript refs
        // hybrid_search had just returned (gap-edc84378): the target doc
        // simply wasn't among the first 500 by tie-broken score.
        let matches = searcher.search(&query, &DocSetCollector)?;
        for addr in matches {
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
            ("relative_path", self.fields.relative_path),
            ("source_uri", self.fields.source_uri),
            ("source_kind", self.fields.source_kind),
            ("project", self.fields.project),
            ("project_id", self.fields.project_id),
            ("code_source_selector", self.fields.code_source_selector),
            ("code_source_generation", self.fields.code_source_generation),
            ("repo_id", self.fields.repo_id),
            ("commit_sha", self.fields.commit_sha),
            ("commit_author_name", self.fields.commit_author_name),
            ("commit_author_email", self.fields.commit_author_email),
            ("tool_server", self.fields.tool_server),
            ("tool_name", self.fields.tool_name),
            ("tool_kind", self.fields.tool_kind),
            ("tool_target", self.fields.tool_target),
            ("tool_outcome", self.fields.tool_outcome),
            ("task_id", self.fields.task_id),
            ("tool_use_id", self.fields.tool_use_id),
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
        if let Some(display_path) = render_display_path(&properties) {
            properties.insert("display_path".into(), display_path);
        }
        properties
    }
}

/// Render `display_path` in the fixed fallback order of governing
/// section 10.2:
///
/// 1. session workspace mapping - does not exist yet, returns `None` this
///    phase, deliberately not faked from anything else;
/// 2. explicitly selected operator attachment for local UI output - joined
///    ABOVE this function by the daemon surface that holds the resolved
///    attachment and RENDERED, never opened (no lease, no stat, no read);
/// 3. accepted project alias/display name plus relative path.
///
/// Tier 3 is what this function can compute from stored fields alone, so it is
/// the only tier implemented here. `source_uri` stays the machine identity and
/// is unaffected by which tier renders.
pub fn render_display_path(properties: &BTreeMap<String, String>) -> Option<String> {
    let relative_path = properties
        .get("relative_path")
        .filter(|value| !value.is_empty())?;
    let display_name = properties
        .get("project")
        .filter(|value| !value.is_empty())?;
    Some(format!("{display_name}/{relative_path}"))
}

/// Tier 2 of the `display_path` order: the operator-selected attachment root
/// joined onto the stored relative path. Rendered for local UI output and
/// NEVER opened - this returns a string, takes no lease, and touches no
/// filesystem, which is why a detached or unavailable checkout cannot make it
/// fail.
pub fn render_selected_attachment_display_path(
    selected_checkout_root: &Path,
    relative_path: &str,
) -> String {
    selected_checkout_root
        .join(relative_path)
        .to_string_lossy()
        .into_owned()
}

pub fn first_text(doc: &TantivyDocument, field: Field) -> String {
    optional_text(doc, field).unwrap_or_default()
}

pub fn optional_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_all(field).next().and_then(|value| match value {
        tantivy::schema::OwnedValue::Str(text) => Some(text.clone()),
        _ => None,
    })
}

pub fn optional_u64(doc: &TantivyDocument, field: Field) -> Option<u64> {
    doc.get_first(field).and_then(|v| v.as_value().as_u64())
}

pub fn first_u64(doc: &TantivyDocument, field: Field) -> u64 {
    doc.get_all(field)
        .next()
        .and_then(|value| match value {
            tantivy::schema::OwnedValue::U64(n) => Some(*n),
            _ => None,
        })
        .unwrap_or_default()
}

/// Seed one local selector per corpus project, then apply the edge-manifest
/// override for projects the corpus knows about. `projects_path` is still the
/// key to the sidecar edges directory; the project identity set is injected.
fn load_active_code_selectors(
    corpus_project_ids: &BTreeSet<String>,
    projects_path: &Path,
) -> Result<BTreeMap<String, String>> {
    let mut selectors = BTreeMap::new();
    for project_id in corpus_project_ids {
        selectors.insert(
            project_id.clone(),
            bbox_code_source::local_selector(project_id),
        );
    }
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(projects_path);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    for (project_id, entry) in manifest.workspaces {
        if corpus_project_ids.contains(&project_id)
            && let Some(selector) = entry.code_source_selector
        {
            selectors.insert(project_id, selector);
        }
    }
    Ok(selectors)
}

pub fn build_schema() -> (Schema, FieldHandles) {
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
        relative_path: builder.add_text_field("relative_path", STRING | STORED),
        source_uri: builder.add_text_field("source_uri", STRING | STORED),
        source_kind: builder.add_text_field("source_kind", STRING | STORED),
        code_source_selector: builder.add_text_field("code_source_selector", STRING | STORED),
        code_source_generation: builder.add_text_field("code_source_generation", STRING | STORED),
        code_source_entry_key: builder.add_text_field("code_source_entry_key", STRING | STORED),
        path_tokens: builder.add_text_field("path_tokens", code_options.clone()),
        byte_offset: builder.add_u64_field("byte_offset", STORED),
        byte_end: builder.add_u64_field("byte_end", STORED),
        line_start: builder.add_u64_field("line_start", STORED),
        line_end: builder.add_u64_field("line_end", STORED),
        git_branch: builder.add_text_field("git_branch", STRING | STORED),
        is_subagent: builder.add_u64_field("is_subagent", INDEXED | STORED),
        agent_slug: builder.add_text_field("agent_slug", STRING | STORED),
        doc_type: builder.add_text_field("doc_type", STRING | STORED),
        project_id: builder.add_text_field("project_id", STRING | STORED),
        base_project_id: builder.add_text_field("base_project_id", STRING | STORED),
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
        symbol_kind: builder.add_text_field("symbol_kind", STRING | STORED),
        parent_kind: builder.add_text_field("parent_kind", STRING | STORED),
        code_content: builder.add_text_field("code_content", code_options),
        chunk_hash: builder.add_text_field("chunk_hash", STRING | STORED),
        entity_id: builder.add_text_field("entity_id", STRING | STORED),
        logical_ref: builder.add_text_field("logical_ref", STRING | STORED),
        knowledge_visibility: builder.add_text_field("knowledge_visibility", STRING | STORED),
        knowledge_scope_hash: builder.add_text_field("knowledge_scope_hash", STRING | STORED),
        knowledge_checkout_id: builder.add_text_field("knowledge_checkout_id", STRING | STORED),
        knowledge_snapshot_id: builder.add_text_field("knowledge_snapshot_id", STRING | STORED),
        parser_version: builder.add_text_field("parser_version", STRING | STORED),
        commit_sha: builder.add_text_field("commit_sha", STRING | STORED),
        repo_id: builder.add_text_field("repo_id", STRING | STORED),
        commit_author_name: builder.add_text_field("commit_author_name", TEXT | STORED),
        commit_author_email: builder.add_text_field("commit_author_email", STRING | STORED),
        tool_server: builder.add_text_field("tool_server", STRING | STORED),
        tool_name: builder.add_text_field("tool_name", STRING | STORED),
        tool_kind: builder.add_text_field("tool_kind", STRING | STORED),
        tool_target: builder.add_text_field("tool_target", STRING | STORED),
        tool_outcome: builder.add_text_field("tool_outcome", STRING | STORED),
        task_id: builder.add_text_field("task_id", STRING | STORED),
        tool_use_id: builder.add_text_field("tool_use_id", STRING | STORED),
    };
    (builder.build(), fields)
}

pub fn register_code_tokenizer(index: &Index) {
    index.tokenizers().register(
        code_tokenizer::CODE_TOKENIZER_NAME,
        TextAnalyzer::from(code_tokenizer::CodeTokenizer::default()),
    );
}

/// The replacement boundary: decide whether the index is dropped, and under
/// which cause, before anything opens it.
///
/// **Three triggers, not one (adjudication Q-F).** The marker mismatch is the
/// daemon-upgrade trigger and stays exactly as it was. The operator force is
/// the Phase 6 same-schema trigger, and it goes THROUGH this function rather
/// than around it precisely so it cannot skip the guard, the fail-closed
/// refusal, or the marker withholding. `PreserveInterrupted` is not a trigger
/// at all: it is the caller's pre-open recovery classification being honored
/// here, suppressing the pre-marker arm for an index whose marker is withheld
/// because a replacement is already in flight.
// one-time boot path before the runtime serves traffic.
#[allow(clippy::disallowed_methods)]
fn reset_index_on_schema_mismatch(
    index_path: &Path,
    projects_path: &Path,
    code_source_store_path: &Path,
    guard: Option<&schema_replacement::SchemaReplacementGuard>,
    intent: schema_replacement::CatalogReplacementIntentV1,
) -> Result<schema_replacement::IndexReplacementOutcomeV1> {
    use schema_replacement::{
        CatalogIndexReplacementCause, CatalogReplacementIntentV1, IndexReplacementOutcomeV1,
    };

    let not_replaced = |marker_withheld| IndexReplacementOutcomeV1 {
        performed: None,
        marker_withheld,
    };
    if !index_path.exists() {
        if intent == CatalogReplacementIntentV1::ForceSameSchema {
            // The operator authorized a replacement of a specific predecessor.
            // There is no index here to be that predecessor, so the
            // authorization does not describe this state.
            anyhow::bail!(
                "error.schema_replacement_stale_predecessor: refusing the operator-triggered \
                 replacement at {}: no index exists to replace",
                index_path.display()
            );
        }
        return Ok(not_replaced(
            intent == CatalogReplacementIntentV1::PreserveInterrupted,
        ));
    }
    let marker_path = index_path.join(SCHEMA_VERSION_FILE);
    let observed = match fs::read_to_string(&marker_path) {
        Ok(raw) => Some(raw.trim().to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let mismatch_reset = match observed.as_deref() {
        Some(marker) => marker != INDEX_SCHEMA_VERSION,
        // A non-empty index directory with no marker at all predates the
        // marker and must be replaced; an empty one is a fresh create. Under
        // `PreserveInterrupted` the absent marker means the opposite: it was
        // WITHHELD by a replacement that is still in flight, and the surviving
        // manifest is the evidence. Dropping there would destroy the exact
        // population that manifest pins.
        None => {
            intent != CatalogReplacementIntentV1::PreserveInterrupted
                && index_path.read_dir()?.next().is_some()
        }
    };
    let cause = if intent == CatalogReplacementIntentV1::ForceSameSchema {
        // The Q-F precondition, checked here rather than by the caller so no
        // future caller can reach the drop without it. A marker that is
        // missing or does not match the running version means the index is not
        // the predecessor the authorization named.
        match observed.as_deref() {
            Some(marker) if marker == INDEX_SCHEMA_VERSION => {}
            other => anyhow::bail!(
                "error.schema_replacement_stale_predecessor: refusing the operator-triggered \
                 replacement at {} (observed {}, required {}): the outgoing marker must equal \
                 the running schema version",
                index_path.display(),
                other.unwrap_or("<no marker>"),
                INDEX_SCHEMA_VERSION
            ),
        }
        Some(CatalogIndexReplacementCause::OperatorPathFreeRebuild)
    } else if mismatch_reset {
        Some(CatalogIndexReplacementCause::SchemaMismatch)
    } else {
        None
    };
    let Some(cause) = cause else {
        // Nothing is replaced. The marker is still withheld when a manifest
        // survives past the drop and the index carries no marker: that is
        // crash state (3) or (4), and publishing the marker here would erase
        // the signal the drive state is classified from.
        return Ok(not_replaced(
            intent == CatalogReplacementIntentV1::PreserveInterrupted && observed.is_none(),
        ));
    };
    {
        let edges_dir =
            bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(projects_path);
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
        verify_collected_schema_migration_sources(&manifest, code_source_store_path)?;
        // P3-E: fail closed. Before this milestone the drop below was
        // unconditional for every caller. An absent guard now refuses it
        // outright rather than silently dropping a history population no
        // inventory has proved, and a guard that refuses aborts the reset with
        // the last-good lexical and vector views still selected because
        // nothing has been replaced yet.
        let Some(guard) = guard else {
            anyhow::bail!(
                "error.schema_replacement_unguarded: refusing to replace the index at {} \
                 (observed {}, target {}) with no pre-replacement guard injected",
                index_path.display(),
                observed.as_deref().unwrap_or("<no marker>"),
                INDEX_SCHEMA_VERSION
            );
        };
        let authorization = guard(&schema_replacement::SchemaReplacementRequest {
            index_path,
            projects_path,
            code_source_store_path,
            observed_schema_version: observed.clone(),
            target_schema_version: INDEX_SCHEMA_VERSION,
            cause,
        })
        .context("pre-replacement guard refused the index schema replacement")?;
        tracing::info!(
            path = %index_path.display(),
            schema_version = INDEX_SCHEMA_VERSION,
            observed_schema_version = observed.as_deref().unwrap_or("<no marker>"),
            ?cause,
            authorized_by = %authorization.authorized_by,
            "dropping transcript index for replacement"
        );
        // The guard has durably published the Prepared manifest by now (that
        // is what it returns authorization for), so the deletion below is
        // recoverable from pinned generations rather than a point of no
        // return.
        fs::remove_dir_all(index_path)?;
    }
    Ok(IndexReplacementOutcomeV1 {
        performed: Some(cause),
        // The marker commits LAST, after the re-emission pass and the
        // manifest commit. Publishing it here would mark a replacement
        // complete before a single document had been re-emitted.
        marker_withheld: true,
    })
}

fn verify_collected_schema_migration_sources(
    manifest: &bbox_edge_sidecar::manifest::ManifestIndex,
    code_source_store_path: &Path,
) -> Result<()> {
    let collected = manifest
        .workspaces
        .iter()
        .filter_map(|(project_id, entry)| {
            let selector = entry.code_source_selector.as_deref()?;
            selector.starts_with("collected:").then_some((
                project_id,
                selector,
                entry.code_source_generation.as_deref(),
            ))
        })
        .collect::<Vec<_>>();
    if collected.is_empty() {
        return Ok(());
    }
    let store = bbox_code_source_store::CodeSourceStore::open(
        code_source_store_path,
        bbox_code_source_store::StoreLimits::default(),
    )?;
    for (project_id, selector, generation) in collected {
        let activation = store.load_activation(project_id)?.ok_or_else(|| {
            anyhow::anyhow!("active collected source has no migration activation record")
        })?;
        if activation.selector != selector || generation != Some(activation.generation_id.as_str())
        {
            anyhow::bail!("active collected source migration metadata is inconsistent");
        }
        let stored = store.find_generation(&activation.generation_id)?;
        let entries =
            store.load_generation_entries(&stored.descriptor.scope, &activation.generation_id)?;
        for entry in entries {
            store
                .verified_blob_file(&entry.content_sha256, entry.size)
                .with_context(|| {
                    format!(
                        "active collected source {} cannot migrate schemas because a source blob is unavailable",
                        activation.generation_id
                    )
                })?;
        }
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
    fn interrupted_replacement_with_absent_index_keeps_marker_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = reset_index_on_schema_mismatch(
            &dir.path().join("absent-index"),
            &dir.path().join("projects.json"),
            &dir.path().join("code-sources"),
            None,
            schema_replacement::CatalogReplacementIntentV1::PreserveInterrupted,
        )
        .unwrap();

        assert!(outcome.performed.is_none());
        assert!(outcome.marker_withheld);
    }

    #[test]
    fn index_open_writes_schema_version_marker() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");

        let _index = TranscriptIndex::open_or_create_with_records(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();

        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn explicit_code_source_store_path_is_independent_of_projects_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let projects_path = root.join("registry").join("projects.json");
        let code_source_store_path = root.join("state").join("code-sources");
        let index = TranscriptIndex::open_or_create_with_code_source_store_path(
            &root.join("index"),
            Vec::new(),
            None,
            projects_path.clone(),
            code_source_store_path.clone(),
            root.join("knowledge.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::new(
                ProjectRecordsSnapshot::empty(),
            )),
            None,
        )
        .unwrap();

        let config = index.reindex_config();
        assert_eq!(config.projects_path, projects_path);
        assert_eq!(config.code_source_store_path, code_source_store_path);
    }

    #[test]
    fn pinned_searcher_keeps_entity_properties_on_one_index_generation() {
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let entity_id = "project_file:project-a:path-a:chunk-a:0";
        let add_entity = |writer: &mut tantivy::IndexWriter, file_path: &str| {
            let mut doc = TantivyDocument::new();
            doc.add_text(fields.doc_type, "project_file");
            doc.add_text(fields.entity_id, entity_id);
            doc.add_text(fields.file_path, file_path);
            writer.add_document(doc).unwrap();
        };

        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        add_entity(&mut writer, "src/old.rs");
        writer.commit().unwrap();
        index.reader_reload_for_test();
        let pinned = index.searcher();

        writer.delete_term(Term::from_field_text(fields.entity_id, entity_id));
        add_entity(&mut writer, "src/new.rs");
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let pinned_properties = index
            .entity_properties_with_searcher(entity_id, &pinned)
            .unwrap()
            .unwrap();
        let current_properties = index.entity_properties(entity_id).unwrap().unwrap();
        assert_eq!(
            pinned_properties.get("file_path").map(String::as_str),
            Some("src/old.rs")
        );
        assert_eq!(
            current_properties.get("file_path").map(String::as_str),
            Some("src/new.rs")
        );
    }

    #[test]
    fn schema_version_mismatch_waits_for_rebuild_before_writing_marker() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();

        let index = TranscriptIndex::open_or_create_guarded(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
            Some(test_authorizing_guard()),
        )
        .unwrap();

        assert!(!index_path.join("stale-file").exists());
        assert!(!index_path.join(SCHEMA_VERSION_FILE).exists());
        index.complete_schema_migration().unwrap();
        let marker = fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE)).unwrap();
        assert_eq!(marker.trim(), INDEX_SCHEMA_VERSION);
    }

    /// A guard that authorizes unconditionally, for the tests that care about
    /// the mechanics AFTER authorization rather than about the guard itself.
    fn test_authorizing_guard() -> schema_replacement::SchemaReplacementGuard {
        std::sync::Arc::new(|_request| {
            Ok(schema_replacement::SchemaReplacementAuthorization::new(
                "test-guard",
            ))
        })
    }

    /// P3-E fail-closed contract: with no guard injected, a detected schema
    /// mismatch REFUSES the replacement instead of dropping the index. The
    /// outgoing index, its marker, and its files all survive, which is what
    /// keeps the last-good lexical and vector views readable.
    #[test]
    fn schema_mismatch_with_no_guard_refuses_and_keeps_the_old_index() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();

        let error = TranscriptIndex::open_or_create_with_records(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .err()
        .expect("an unguarded replacement must be refused");
        assert!(
            format!("{error:#}").contains("error.schema_replacement_unguarded"),
            "{error:#}"
        );
        assert!(index_path.join("stale-file").exists());
        assert_eq!(
            fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE))
                .unwrap()
                .trim(),
            "old-schema"
        );
    }

    /// A guard that REFUSES aborts the reset with everything intact. This is
    /// the shape every refusal-matrix row shares (missing generation, corrupt
    /// manifest, commitment mismatch): the guard errors, nothing is dropped.
    #[test]
    fn a_refusing_guard_aborts_the_reset_and_keeps_the_old_index() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();

        let guard: schema_replacement::SchemaReplacementGuard = std::sync::Arc::new(|_request| {
            Err(anyhow::anyhow!(
                "error.history_commitment_mismatch: namespace proof failed"
            ))
        });
        let error = TranscriptIndex::open_or_create_guarded(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
            Some(guard),
        )
        .err()
        .expect("a refusing guard must abort the replacement");
        assert!(
            format!("{error:#}").contains("error.history_commitment_mismatch"),
            "{error:#}"
        );
        assert!(index_path.join("stale-file").exists());
        assert_eq!(
            fs::read_to_string(index_path.join(SCHEMA_VERSION_FILE))
                .unwrap()
                .trim(),
            "old-schema"
        );
    }

    /// The guard observes the OUTGOING marker and the incoming target, so a
    /// materializer can prove the population it is about to carry against the
    /// schema that produced it.
    #[test]
    fn the_guard_observes_both_schema_versions() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        fs::create_dir_all(&index_path).unwrap();
        fs::write(index_path.join(SCHEMA_VERSION_FILE), "old-schema\n").unwrap();
        fs::write(index_path.join("stale-file"), "stale").unwrap();
        let observed = std::sync::Arc::new(Mutex::new(None));
        let captured = observed.clone();
        let guard: schema_replacement::SchemaReplacementGuard =
            std::sync::Arc::new(move |request| {
                *captured.lock() = Some((
                    request.observed_schema_version.clone(),
                    request.target_schema_version.to_string(),
                ));
                Ok(schema_replacement::SchemaReplacementAuthorization::new(
                    "test-guard",
                ))
            });
        TranscriptIndex::open_or_create_guarded(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
            Some(guard),
        )
        .unwrap();
        assert_eq!(
            observed.lock().clone(),
            Some((
                Some("old-schema".to_string()),
                INDEX_SCHEMA_VERSION.to_string()
            ))
        );
    }

    /// A fresh index directory is NOT a mismatch, so the guard never runs and
    /// the no-guard path stays usable for every ordinary open.
    #[test]
    fn a_fresh_index_never_invokes_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = invoked.clone();
        let guard: schema_replacement::SchemaReplacementGuard =
            std::sync::Arc::new(move |_request| {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(schema_replacement::SchemaReplacementAuthorization::new(
                    "test-guard",
                ))
            });
        TranscriptIndex::open_or_create_guarded(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
            Some(guard),
        )
        .unwrap();
        assert!(!invoked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn for_each_edge_projection_doc_streams_all_segments_and_skips_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();

        // Segment 1: a transcript doc.
        let mut transcript = TantivyDocument::new();
        transcript.add_text(fields.doc_type, "transcript");
        transcript.add_text(fields.account, "claude");
        transcript.add_text(fields.session_id, "sess-1");
        transcript.add_u64(fields.byte_offset, 42);
        writer.add_document(transcript).unwrap();
        writer.commit().unwrap();

        // Segment 2: a project_file chunk plus a doc that gets deleted, so the
        // iterator must both cross segment boundaries and honor the alive bitset.
        let mut chunk = TantivyDocument::new();
        chunk.add_text(fields.doc_type, "project_file");
        chunk.add_text(fields.file_path, "src/lib.rs");
        chunk.add_text(fields.entity_id, "pfile:proj1234:src/lib.rs:0");
        writer.add_document(chunk).unwrap();
        let mut deleted = TantivyDocument::new();
        deleted.add_text(fields.doc_type, "transcript");
        deleted.add_text(fields.session_id, "sess-deleted");
        writer.add_document(deleted).unwrap();
        writer.commit().unwrap();
        writer.delete_term(Term::from_field_text(fields.session_id, "sess-deleted"));
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let mut docs = Vec::new();
        let emitted = index
            .for_each_edge_projection_doc(|doc| {
                docs.push(doc);
                Ok(())
            })
            .unwrap();

        assert_eq!(emitted, 2);
        assert_eq!(docs.len(), 2);
        docs.sort_by(|a, b| a.doc_type.cmp(&b.doc_type));
        assert_eq!(docs[0].doc_type, "project_file");
        assert_eq!(docs[0].file_path, "src/lib.rs");
        assert_eq!(
            docs[0].entity_id.as_deref(),
            Some("pfile:proj1234:src/lib.rs:0")
        );
        assert_eq!(docs[1].doc_type, "transcript");
        assert_eq!(docs[1].account, "claude");
        assert_eq!(docs[1].session_id, "sess-1");
        assert_eq!(docs[1].byte_offset, 42);
        assert!(
            !docs.iter().any(|d| d.session_id == "sess-deleted"),
            "deleted doc must be skipped via the alive bitset"
        );
    }

    #[test]
    fn hybrid_bm25_canonicalizes_legacy_transcript_entity_ids() {
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();

        // Transcript doc carrying the legacy UNPREFIXED entity_id shape
        // (`<provider>:<session>:<offset>:<idx>`): the hit must come back
        // with the canonical parseable `transcript:` ref synthesized from
        // the doc fields, not the stored string.
        let mut transcript = TantivyDocument::new();
        transcript.add_text(fields.doc_type, "transcript");
        transcript.add_text(fields.account, "claude");
        transcript.add_text(fields.session_id, "sess-legacy");
        transcript.add_u64(fields.byte_offset, 1234);
        transcript.add_text(fields.content, "quantum flux capacitor alignment");
        transcript.add_text(fields.entity_id, "claude:sess-legacy:1234:0");
        writer.add_document(transcript).unwrap();

        // Non-transcript doc: explicit entity_id passes through verbatim.
        let mut chunk = TantivyDocument::new();
        chunk.add_text(fields.doc_type, "project_file");
        chunk.add_text(fields.file_path, "src/flux.rs");
        chunk.add_text(fields.content, "quantum flux capacitor alignment");
        chunk.add_text(fields.entity_id, "project_file:proj1234:aa:bb:0");
        chunk.add_text(
            fields.code_source_selector,
            bbox_code_source::local_selector("proj1234"),
        );
        writer.add_document(chunk).unwrap();
        writer.commit().unwrap();
        index.reader_reload_for_test();
        index.replace_active_code_selectors(BTreeMap::from([(
            "proj1234".to_string(),
            bbox_code_source::local_selector("proj1234"),
        )]));

        let hits = index
            .hybrid_bm25_hits("quantum flux capacitor", 10, None)
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|hit| hit.entity_id.as_str()).collect();
        assert!(
            ids.contains(&"transcript:claude:sess-legacy:1234:0"),
            "legacy transcript id must canonicalize: {ids:?}"
        );
        assert!(
            ids.contains(&"project_file:proj1234:aa:bb:0"),
            "explicit non-transcript id must pass through: {ids:?}"
        );
        assert!(
            !ids.contains(&"claude:sess-legacy:1234:0"),
            "unprefixed transcript id must not leak through: {ids:?}"
        );
    }

    #[test]
    fn doc_type_count_counts_only_the_requested_doc_type() {
        // gap-edc84378: bbox_describe_schema's transcript count comes from
        // this cheap TermQuery + Count collector, not from EdgeIndex (which
        // deliberately excludes transcript from its active counts).
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();

        for (idx, session) in ["sess-1", "sess-2"].into_iter().enumerate() {
            let mut transcript = TantivyDocument::new();
            transcript.add_text(fields.doc_type, "transcript");
            transcript.add_text(fields.account, "claude");
            transcript.add_text(fields.session_id, session);
            transcript.add_u64(fields.byte_offset, idx as u64);
            writer.add_document(transcript).unwrap();
        }
        let mut chunk = TantivyDocument::new();
        chunk.add_text(fields.doc_type, "project_file");
        chunk.add_text(fields.file_path, "src/lib.rs");
        chunk.add_text(fields.entity_id, "pfile:proj1234:src/lib.rs:0");
        writer.add_document(chunk).unwrap();
        writer.commit().unwrap();
        index.reader_reload_for_test();

        assert_eq!(index.doc_type_count("transcript").unwrap(), 2);
        assert_eq!(index.doc_type_count("project_file").unwrap(), 1);
        assert_eq!(index.doc_type_count("commit").unwrap(), 0);
    }

    #[test]
    fn transcript_properties_finds_docs_beyond_the_old_top_docs_cap() {
        // gap-edc84378 fold: transcript_properties used to scan a session's
        // docs via TopDocs::with_limit(500), a scored collector over a
        // STRING TermQuery where every doc scores identically -- so any
        // session with more than 500 chunks could silently drop matches
        // beyond the cap. Verified against the old TopDocs(500) code: this
        // 600-doc session's *first*-inserted doc (offset 0) was exactly the
        // one it dropped (tantivy's tie-break favors later doc ids), and
        // that's the doc this test resolves.
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();

        const DOC_COUNT: u64 = 600;
        for offset in 0..DOC_COUNT {
            let mut transcript = TantivyDocument::new();
            transcript.add_text(fields.doc_type, "transcript");
            transcript.add_text(fields.account, "claude");
            transcript.add_text(fields.session_id, "sess-big");
            transcript.add_u64(fields.byte_offset, offset);
            writer.add_document(transcript).unwrap();
        }
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let first = index
            .transcript_properties("claude", "sess-big", 0)
            .unwrap();
        assert!(
            first.is_some(),
            "expected to resolve the first-inserted doc in a 600-doc session"
        );

        // A byte_offset that genuinely has no matching doc must still 404 --
        // the fix must not fabricate matches for arbitrary offsets.
        let missing = index
            .transcript_properties("claude", "sess-big", DOC_COUNT)
            .unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn for_each_edge_projection_doc_callback_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        let mut doc = TantivyDocument::new();
        doc.add_text(fields.doc_type, "transcript");
        doc.add_text(fields.session_id, "sess-1");
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let result = index.for_each_edge_projection_doc(|_| anyhow::bail!("stop"));
        assert!(result.is_err());
    }

    #[test]
    fn code_content_tokenizer_finds_identifier_fragments() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let index = TranscriptIndex::open_or_create_with_records(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let project = bbox_corpus_core::project_record::ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = bbox_chunker::Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("rust".into()),
            symbol: Some("KnowledgeStore".into()),
            symbol_exact: Some("KnowledgeStore".into()),
            symbol_kind: Some("struct_item".into()),
            parent_kind: None,
            line_start: Some(1),
            line_end: Some(1),
            content: "pub struct KnowledgeStore;".into(),
            byte_start: 0,
            byte_end: 26,
            visual_payload: None,
        };
        let doc = project_files::build_project_file_doc(
            &chunk,
            &project,
            "test-project-display",
            Some("a".repeat(40).as_str()),
            None,
            index.field_handles(),
        );
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        index.reader.reload().unwrap();
        index.replace_active_code_selectors(BTreeMap::from([(
            project.project_id.clone(),
            bbox_code_source::local_selector(&project.project_id),
        )]));

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
        // P3-E: the rendered result carries the RELATIVE path and the display
        // name; no host root appears anywhere in it.
        assert!(result.contains("src/lib.rs"), "{result}");
        assert!(
            !result.contains("/tmp/repo"),
            "a rendered project-file result must carry no host root: {result}"
        );
    }

    /// gap-72fd5932 plus the phase-2 B1 retirement: the two filter lanes
    /// are driven entirely by the caller-supplied `ProjectFilterInput`.
    /// Docs stamped with base_project_id are reachable by resolved id even
    /// when their literal cwd is an out-of-tree worktree path the substring
    /// lane cannot match, the literal lane still stands on its own, and a
    /// registered record on disk never resolves an id inside this crate.
    #[test]
    fn project_filter_lanes_follow_the_supplied_filter_input() {
        let dir = tempfile::tempdir().unwrap();
        let projects_path = dir.path().join("projects.json");
        std::fs::write(
            &projects_path,
            serde_json::json!({
                "projects": [{
                    "project_id": "feedbeef",
                    "repo_id": null,
                    "canonical_path": "/tmp/registered-base",
                    "registered_at": "2026-01-01T00:00:00Z",
                    "is_git_repo": true,
                    "aliases": ["blackbox"],
                }]
            })
            .to_string(),
        )
        .unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &dir.path().join("index"),
            Vec::new(),
            None,
            projects_path,
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            // The SAME record the file above carries. Injected explicitly
            // rather than read back off `projects_path`: the assertion below
            // is that a registered record never resolves an id inside this
            // crate, and an empty authority would make that vacuous.
            std::sync::Arc::new(StaticProjectRecordsProvider::from_bridge_records(
                vec![bbox_corpus_core::project_record::ProjectRecord {
                    project_id: "feedbeef".into(),
                    repo_id: None,
                    canonical_path: "/tmp/registered-base".into(),
                    registered_at: "2026-01-01T00:00:00Z".into(),
                    is_git_repo: true,
                    languages: Default::default(),
                    aliases: ["blackbox".to_string()].into_iter().collect(),
                }],
                0,
            )),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        let add_doc = |session: &str, cwd: &str, base: Option<&str>| {
            let mut doc = TantivyDocument::new();
            doc.add_text(fields.doc_type, "transcript");
            doc.add_text(fields.content, "worktree stamping probe");
            doc.add_text(fields.session_id, session);
            doc.add_text(fields.project, cwd);
            doc.add_u64(fields.is_subagent, 0);
            if let Some(base) = base {
                doc.add_text(fields.base_project_id, base);
            }
            writer.add_document(doc).unwrap();
        };
        // Out-of-tree worktree session: literal cwd shares no substring
        // with the base path, only the stamp links it.
        add_doc(
            "wt-session",
            "/state/fleet/worktrees/task-9",
            Some("feedbeef"),
        );
        add_doc("base-session", "/tmp/registered-base", Some("feedbeef"));
        add_doc("other-session", "/somewhere/else", None);
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let probe = |filter: ProjectFilterInput| {
            index
                .search_with_project_filter(
                    &SearchParams {
                        query: "stamping probe".into(),
                        mode: None,
                        account: None,
                        project: Some(filter.literal.clone()),
                        role: None,
                        include_subagents: None,
                        limit: Some(10),
                        exclude_self: None,
                    },
                    Some(&filter),
                    &index.active_code_selectors(),
                    &index.searcher(),
                )
                .unwrap()
        };

        // Resolved id: both checkouts of the base project, nothing else.
        for selector in ["feedbeef", "blackbox"] {
            let result = probe(ProjectFilterInput {
                project_id: Some("feedbeef".into()),
                literal: selector.into(),
            });
            assert!(result.contains("wt-session"), "{selector}: {result}");
            assert!(result.contains("base-session"), "{selector}: {result}");
            assert!(!result.contains("other-session"), "{selector}: {result}");
        }

        // Literal only: the substring lane matches the base checkout's cwd
        // and nothing the stamp would have added.
        let literal_only = probe(ProjectFilterInput::unresolved("/tmp/registered-base"));
        assert!(literal_only.contains("base-session"), "{literal_only}");
        assert!(!literal_only.contains("wt-session"), "{literal_only}");
        assert!(!literal_only.contains("other-session"), "{literal_only}");

        // The record on disk is registered, but resolution is the caller's
        // job: an unresolved selector never manufactures an id here, so the
        // stamped worktree session stays out of reach.
        let unresolved_id = probe(ProjectFilterInput::unresolved("feedbeef"));
        assert!(
            unresolved_id.contains("No results found"),
            "{unresolved_id}"
        );
        let unregistered = probe(ProjectFilterInput::unresolved("somewhere"));
        assert!(unregistered.contains("other-session"), "{unregistered}");
        assert!(!unregistered.contains("wt-session"), "{unregistered}");
    }

    /// CN-D3 contract: project_file docs carry symbol_kind, parent_kind,
    /// byte_end, line_start, line_end, and a queryable project_id term.
    /// Without these stored fields the indexed code_symbols lane in
    /// CN-T2 cannot operate. Verify each round-trips through tantivy.
    #[test]
    fn project_file_doc_round_trips_symbol_kind_parent_kind_line_ranges_and_project_id() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let index = TranscriptIndex::open_or_create_with_records(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let project = bbox_corpus_core::project_record::ProjectRecord {
            project_id: "proj-cn-d3".into(),
            repo_id: Some("repo-cn-d3".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = bbox_chunker::Chunk {
            project_id: "proj-cn-d3".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("rust".into()),
            symbol: Some("S::run".into()),
            symbol_exact: Some("run".into()),
            symbol_kind: Some("function_item".into()),
            parent_kind: Some("impl_item".into()),
            line_start: Some(3),
            line_end: Some(3),
            content: "fn run(&self) {}".into(),
            byte_start: 19,
            byte_end: 35,
            visual_payload: None,
        };
        let doc = project_files::build_project_file_doc(
            &chunk,
            &project,
            "test-project-display",
            None,
            None,
            index.field_handles(),
        );
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        index.reader.reload().unwrap();

        let searcher = index.reader.searcher();
        let fields = index.field_handles();
        let term_query = TermQuery::new(
            Term::from_field_text(fields.project_id, "proj-cn-d3"),
            IndexRecordOption::Basic,
        );
        let hits = searcher
            .search(&term_query, &tantivy::collector::TopDocs::with_limit(2))
            .unwrap();
        assert_eq!(hits.len(), 1, "project_id term must locate the doc");

        let stored: TantivyDocument = searcher.doc(hits[0].1).unwrap();
        assert_eq!(
            optional_text(&stored, fields.symbol_kind).as_deref(),
            Some("function_item")
        );
        assert_eq!(
            optional_text(&stored, fields.parent_kind).as_deref(),
            Some("impl_item")
        );
        assert_eq!(optional_u64(&stored, fields.byte_offset), Some(19));
        assert_eq!(optional_u64(&stored, fields.byte_end), Some(35));
        assert_eq!(optional_u64(&stored, fields.line_start), Some(3));
        assert_eq!(optional_u64(&stored, fields.line_end), Some(3));
        assert_eq!(
            optional_text(&stored, fields.project_id).as_deref(),
            Some("proj-cn-d3")
        );
    }

    #[test]
    fn embedding_source_doc_for_entity_id_returns_full_content() {
        let dir = tempfile::tempdir().unwrap();
        let index_path = dir.path().join("index");
        let index = TranscriptIndex::open_or_create_with_records(
            &index_path,
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let project = bbox_corpus_core::project_record::ProjectRecord {
            project_id: "proj-size".into(),
            repo_id: Some("repo-size".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let content = "pub fn measured() { println!(\"full content\"); }";
        let chunk = bbox_chunker::Chunk {
            project_id: "proj-size".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "e".repeat(64),
            occurrence_idx: 0,
            language: Some("rust".into()),
            symbol: Some("measured".into()),
            symbol_exact: Some("measured".into()),
            symbol_kind: Some("function_item".into()),
            parent_kind: None,
            line_start: Some(1),
            line_end: Some(1),
            content: content.into(),
            byte_start: 0,
            byte_end: content.len() as u64,
            visual_payload: None,
        };
        let doc = project_files::build_project_file_doc(
            &chunk,
            &project,
            "test-project-display",
            None,
            None,
            index.field_handles(),
        );
        let entity_id = embed_hook::project_file_entity_id_for_snapshot(&chunk, None);
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();
        index.reader.reload().unwrap();

        let resolved = index
            .embedding_source_doc_for_entity_id(&entity_id)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.content, content);
        assert_eq!(resolved.entity_id.as_deref(), Some(entity_id.as_str()));
        assert_eq!(
            resolved.chunk_hash.as_deref(),
            Some(chunk.chunk_hash.as_str())
        );
    }

    #[test]
    fn provenance_v1_resolver_uses_exact_selector_and_byte_range() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let index = TranscriptIndex::open_or_create_with_records(
            &root.join("index"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("knowledge.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = index.field_handles();
        let selector = "collected:project-one:generation-one";
        let mut writer = index.index_handle().writer(50_000_000).unwrap();
        for (occurrence, start, end, hash) in [
            (0_u32, 0_u64, 9_u64, "a".repeat(64)),
            (1_u32, 10_u64, 20_u64, "b".repeat(64)),
        ] {
            let mut document = TantivyDocument::default();
            document.add_text(fields.code_source_selector, selector);
            document.add_text(fields.relative_path, "src/lib.rs");
            document.add_u64(fields.byte_offset, start);
            document.add_u64(fields.byte_end, end);
            document.add_text(
                fields.entity_id,
                format!("project_file_v2:project-one:snapshot:path:{hash}:{occurrence}"),
            );
            writer.add_document(document).unwrap();
        }
        writer.commit().unwrap();
        index.reader_reload_for_test();
        let searcher = index.searcher();
        let selected = index
            .resolve_project_chunk_for_selector_with_searcher(
                "project-one",
                selector,
                "src/lib.rs",
                Some((12, 13)),
                &searcher,
            )
            .unwrap()
            .unwrap();
        assert!(selected.to_string().ends_with(":1"));
        assert!(
            index
                .resolve_project_chunk_for_selector_with_searcher(
                    "project-one",
                    "collected:project-one:other",
                    "src/lib.rs",
                    Some((12, 13)),
                    &searcher,
                )
                .unwrap()
                .is_none()
        );
    }
}

pub mod code_tokenizer;
pub mod embed_hook;
pub mod git_history;
pub mod helpers;
pub mod history_generations;
pub mod migration_inventory;
pub mod passes;
pub mod project_files;
pub mod schema_replacement;
pub mod search;
pub mod tool_edges;

pub use helpers::find_session_file;
pub use search::{
    CiteParams, ContextParams, HybridBm25Hit, MessagesParams, ProjectFilterInput, ReindexParams,
    SearchParams, SessionParams, SessionsListParams, TopicsParams,
};

pub fn resolve_current_project_chunk_entity(
    project_id: &str,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<bbox_corpus_core::entity_ref::EntityRef>> {
    project_files::resolve_current_chunk_entity(project_id, root, absolute_path, byte_range)
}
