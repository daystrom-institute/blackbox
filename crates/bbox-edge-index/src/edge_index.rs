use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_index::index::{EdgeProjectionDoc, TranscriptIndex};
pub use bbox_edge_sidecar::edge_sidecar::*;
use bbox_knowledge::knowledge::{Knowledge, KnowledgeEdgeKind};
use bbox_stores::roadmap::Roadmap;
use bbox_threads::notes::Notes;
use bbox_threads::threads::{EdgeKind, EdgeTarget, Threads};

#[derive(Default)]
pub struct EdgeIndex {
    edges: Vec<Edge>,
    forward: HashMap<EntityRef, Vec<usize>>,
    reverse: HashMap<EntityRef, Vec<usize>>,
    commit_anchor_index: HashMap<String, Vec<usize>>,
    session_tool_calls: HashMap<(String, String), Vec<usize>>,
}

pub struct EdgeStoreRefs<'a> {
    pub index: &'a TranscriptIndex,
    pub knowledge: &'a Knowledge,
    pub threads: &'a Threads,
    pub notes: &'a Notes,
    /// (provider, session_id, bro_label) rows extracted from the task
    /// store by the caller — dependency inversion keeping this store
    /// below orchestration in the crate DAG.
    pub session_brofile_rows: Vec<(String, String, String)>,
    pub roadmap: &'a Roadmap,
    pub edges_dir: PathBuf,
    pub registered_project_ids: Option<HashSet<String>>,
    pub include_tantivy_projection: bool,
    /// When false, observed lane edges (EDITED_FILE/READ_FILE/RAN_BASH) are
    /// excluded from the rebuilt index. Default graph queries (describe_schema,
    /// hybrid search) use Active mode; provenance/blame callers use Historical.
    pub include_observed: bool,
}

impl EdgeIndex {
    pub fn rebuild(stores: &EdgeStoreRefs<'_>) -> Self {
        let started = Instant::now();
        let (mut index, mut seen) = Self::project_store_edges(stores);

        index.load_sidecar_edges(
            &stores.edges_dir,
            stores.registered_project_ids.as_ref(),
            &mut seen,
            stores.include_observed,
        );

        index.log_rebuilt(stores.include_tantivy_projection, started);
        index
    }

    /// Store-projection half of `rebuild`: walks the in-memory stores only.
    /// Callers holding store read guards (knowledge/threads/notes/tasks/
    /// roadmap/idx) run just this half under them, then drop the guards
    /// before `load_sidecar_edges` — the sidecar load is a multi-GB disk
    /// parse that needs no store access, and parking_lot fairness means one
    /// writer queued behind a guard held that long blocks all new readers.
    /// Store projections must run before the sidecar load so `seen` dedup
    /// attribution matches `rebuild`.
    pub fn project_store_edges(stores: &EdgeStoreRefs<'_>) -> (Self, HashSet<EdgeKey>) {
        let mut index = Self::default();
        let mut seen = HashSet::new();

        index.project_knowledge_edges(stores.knowledge, &mut seen);
        index.project_thread_edges(stores.threads, &mut seen);
        index.project_note_edges(stores.notes, &mut seen);
        index.project_task_edges(&stores.session_brofile_rows, &mut seen);
        index.project_roadmap_edges(stores.roadmap, &mut seen);
        if stores.include_tantivy_projection {
            index.project_tantivy_edges(stores.index, &mut seen);
        } else {
            tracing::debug!("rebuilt EdgeIndex without Tantivy stored-doc projection");
        }
        (index, seen)
    }

    pub fn log_rebuilt(&self, include_tantivy_projection: bool, started: Instant) {
        tracing::info!(
            edges = self.edge_count(),
            sources = self.forward.len(),
            targets = self.reverse.len(),
            include_tantivy_projection,
            elapsed_ms = started.elapsed().as_millis(),
            "rebuilt EdgeIndex"
        );
    }

    pub fn load_sidecar_edges(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<EdgeKey>,
        include_observed: bool,
    ) {
        match bbox_edge_sidecar::manifest::try_load_manifest_index(edges_dir) {
            Ok(manifest_index) => {
                let loadable = manifest_index.active_paths_for_loader(edges_dir);
                let total_materialized_files = count_materialized_jsonl_files(edges_dir);
                let skipped_inactive = total_materialized_files.saturating_sub(loadable.len());
                self.load_manifest_active_paths(&loadable, seen);
                self.load_legacy_explicit_edges(
                    edges_dir,
                    registered_project_ids,
                    seen,
                    include_observed,
                );
                tracing::info!(
                    active_paths = loadable.len(),
                    skipped_inactive_refs = skipped_inactive,
                    total_materialized_files,
                    "loaded edges via manifest-index"
                );
            }
            Err(reason) => {
                if !matches!(
                    reason,
                    bbox_edge_sidecar::manifest::ManifestFallbackReason::MissingNotMigrated
                ) {
                    tracing::warn!(?reason, "manifest-index fallback to legacy sidecar loading");
                }
                self.project_sidecar_edges(
                    edges_dir,
                    registered_project_ids,
                    seen,
                    include_observed,
                );
            }
        }
    }

    #[allow(dead_code)]
    pub fn forward_edges(&self, source: &EntityRef) -> Vec<&Edge> {
        let Some(indices) = self.forward.get(source) else {
            return Vec::new();
        };
        let mut edges = Vec::with_capacity(indices.len());
        for edge_id in indices {
            edges.push(&self.edges[*edge_id]);
        }
        edges
    }

    #[allow(dead_code)]
    pub fn reverse_edges(&self, target: &EntityRef) -> Vec<&Edge> {
        let Some(indices) = self.reverse.get(target) else {
            return Vec::new();
        };
        let mut edges = Vec::with_capacity(indices.len());
        for edge_id in indices {
            edges.push(&self.edges[*edge_id]);
        }
        edges
    }

    /// Forward edges for `source`, plus a synthesized `IN_SESSION` edge when
    /// `source` is a transcript ref and no materialized `IN_SESSION` edge
    /// already covers it.
    ///
    /// transcript -> session `IN_SESSION` is a pure function of the ref
    /// (provider/session_id are parsed straight out of `EntityRef::Transcript`),
    /// so it doesn't need `project_tantivy_edges`, the bulk Tantivy stored-doc
    /// projection that used to materialize this edge for every transcript doc.
    /// That projection is opt-in via `include_tantivy_projection`, and every
    /// caller (boot rebuild, store-mutation rebuilds) now passes `false` --
    /// deliberately, to avoid materializing a multi-GB edge set for a corpus
    /// with millions of transcript turns (see commit ffd9027e and the doc
    /// comment on `EdgeStoreRefs::include_tantivy_projection`). This method is
    /// the query-time replacement: it costs nothing until an agent actually
    /// asks about a specific transcript ref.
    ///
    /// This is FORWARD ONLY. The reverse direction -- given a `session:` ref,
    /// enumerate every transcript chunk that is IN_SESSION it -- is NOT a
    /// pure function of the session ref alone; it requires scanning every
    /// transcript doc in that session, which is exactly the bulk projection
    /// this method exists to avoid. `reverse_edges` for a session ref
    /// therefore does not gain a synthesized counterpart here, and that
    /// enumeration is not currently supported by the graph tools.
    pub fn forward_edges_with_synthesis(&self, source: &EntityRef) -> Vec<Edge> {
        let mut edges: Vec<Edge> = self.forward_edges(source).into_iter().cloned().collect();
        if let EntityRef::Transcript {
            provider,
            session_id,
            ..
        } = source
        {
            if !edges.iter().any(|edge| edge.kind == "IN_SESSION") {
                edges.push(exact_edge(
                    source.clone(),
                    "IN_SESSION",
                    EntityRef::Session {
                        provider: provider.clone(),
                        session_id: session_id.clone(),
                    },
                    EdgeProvenance::Derived,
                ));
            }
        }
        edges
    }

    #[allow(dead_code)]
    pub fn forward_edges_filtered(&self, source: &EntityRef, kinds: &[&str]) -> Vec<&Edge> {
        self.forward_edges(source)
            .into_iter()
            .filter(|edge| kinds.iter().any(|kind| *kind == edge.kind))
            .collect()
    }

    #[allow(dead_code)]
    pub fn reverse_edges_filtered(&self, target: &EntityRef, kinds: &[&str]) -> Vec<&Edge> {
        self.reverse_edges(target)
            .into_iter()
            .filter(|edge| kinds.iter().any(|kind| *kind == edge.kind))
            .collect()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn known_refs(&self) -> Vec<EntityRef> {
        let mut refs = HashSet::new();
        refs.extend(self.forward.keys().cloned());
        refs.extend(self.reverse.keys().cloned());
        let mut refs = refs.into_iter().collect::<Vec<_>>();
        refs.sort_by_key(|r| r.to_string());
        refs
    }

    #[cfg(test)]
    pub fn entity_type_counts(&self) -> BTreeMap<String, usize> {
        let mut seen = HashSet::new();
        let mut counts = BTreeMap::new();
        for r in self.forward.keys().chain(self.reverse.keys()) {
            if seen.insert(r) {
                *counts
                    .entry(r.entity_type().as_str().to_string())
                    .or_insert(0) += 1;
            }
        }
        counts
    }

    /// Like `entity_type_counts` but excludes entity types that only appear
    /// via observed history lanes (transcript, bash_call). Used by
    /// `bbox_describe_schema` to show the active knowledge graph without
    /// flooding counts with raw provenance observations.
    pub fn entity_type_counts_active(&self) -> BTreeMap<String, usize> {
        const OBSERVED_TYPES: &[&str] = &["transcript", "bash_call"];
        let mut seen = HashSet::new();
        let mut counts = BTreeMap::new();
        for r in self.forward.keys().chain(self.reverse.keys()) {
            if seen.insert(r) {
                let ty = r.entity_type().as_str().to_string();
                if !OBSERVED_TYPES.contains(&ty.as_str()) {
                    *counts.entry(ty).or_insert(0) += 1;
                }
            }
        }
        counts
    }

    pub fn all_edges(&self) -> impl Iterator<Item = &Edge> {
        self.edges.iter()
    }

    pub fn edges_with_anchor_commit(&self, commit_sha: &str) -> Vec<&Edge> {
        self.commit_anchor_index
            .get(commit_sha)
            .map(|indices| {
                let mut edges = Vec::with_capacity(indices.len());
                for edge_id in indices {
                    edges.push(&self.edges[*edge_id]);
                }
                edges
            })
            .unwrap_or_default()
    }

    pub fn session_tool_call_edges(&self, provider: &str, session_id: &str) -> Vec<&Edge> {
        self.session_tool_calls
            .get(&(provider.to_string(), session_id.to_string()))
            .map(|indices| {
                let mut edges = Vec::with_capacity(indices.len());
                for edge_id in indices {
                    edges.push(&self.edges[*edge_id]);
                }
                edges
            })
            .unwrap_or_default()
    }

    fn insert(&mut self, edge: Edge, seen: &mut HashSet<EdgeKey>) {
        if !seen.insert(edge.dedup_key()) {
            return;
        }
        let edge_id = self.edges.len();
        self.edges.push(edge);
        let edge = &self.edges[edge_id];
        if edge.kind == "EDITED_FILE" {
            if let Some(commit_sha) = edge.metadata.get("anchor.commit_sha_at_edit") {
                self.commit_anchor_index
                    .entry(commit_sha.clone())
                    .or_default()
                    .push(edge_id);
            }
        }
        if matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE" | "RAN_BASH") {
            if let EntityRef::Transcript {
                provider,
                session_id,
                ..
            } = &edge.source
            {
                self.session_tool_calls
                    .entry((provider.clone(), session_id.clone()))
                    .or_default()
                    .push(edge_id);
            }
        }
        self.reverse
            .entry(edge.target.clone())
            .or_default()
            .push(edge_id);
        self.forward
            .entry(edge.source.clone())
            .or_default()
            .push(edge_id);
    }

    // Not `#[cfg(test)]` gated: consumer crates (the root crate's
    // mcp_tools tests) call this from their own test modules, where this
    // crate compiles as a normal dependency and `cfg(test)` is false.
    pub fn from_edges_for_tests(edges: Vec<Edge>) -> Self {
        let mut index = Self::default();
        let mut seen = HashSet::new();
        for edge in edges {
            index.insert(edge, &mut seen);
        }
        index
    }

    fn project_knowledge_edges(&mut self, knowledge: &Knowledge, seen: &mut HashSet<EdgeKey>) {
        for entry in knowledge.all_entries() {
            if let Some(target) = &entry.supersedes {
                self.insert(
                    exact_edge(
                        EntityRef::Knowledge {
                            id: entry.id.clone(),
                        },
                        "SUPERSEDES",
                        EntityRef::Knowledge { id: target.clone() },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
            for link in &entry.links {
                let Ok(target) = EntityRef::parse(&link.target) else {
                    tracing::debug!(
                        source = %entry.id,
                        target = %link.target,
                        "skipping malformed KnowledgeEntry.links target"
                    );
                    continue;
                };
                let mut metadata = BTreeMap::new();
                if let Some(note) = &link.note {
                    metadata.insert("note".into(), note.clone());
                }
                if let Some(source_arc) = &link.source_arc {
                    metadata.insert("source_arc".into(), source_arc.clone());
                }
                self.insert(
                    Edge {
                        source: EntityRef::Knowledge {
                            id: entry.id.clone(),
                        },
                        kind: link.kind.edge_kind().into(),
                        target,
                        provenance: EdgeProvenance::Explicit,
                        confidence: link.confidence,
                        metadata,
                    },
                    seen,
                );
                if link.kind == KnowledgeEdgeKind::Supersedes {
                    // Keep authored supersession links queryable through the
                    // same edge family as the legacy `supersedes` field.
                    tracing::debug!(source = %entry.id, "projected authored SUPERSEDES knowledge link");
                }
            }
        }
    }

    fn project_thread_edges(&mut self, threads: &Threads, seen: &mut HashSet<EdgeKey>) {
        let session_providers = threads
            .all()
            .iter()
            .flat_map(|thread| thread.sessions.iter())
            .map(|session| (session.session_id.clone(), session.provider.clone()))
            .collect::<HashMap<_, _>>();
        for thread in threads.all() {
            let source = EntityRef::Thread {
                thread_id: thread.id.clone(),
            };
            for session in &thread.sessions {
                self.insert(
                    exact_edge(
                        source.clone(),
                        "THREAD_HAS_SESSION",
                        EntityRef::Session {
                            provider: session.provider.clone(),
                            session_id: session.session_id.clone(),
                        },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
            for edge in &thread.edges {
                let target = match &edge.target_type {
                    EdgeTarget::Thread => EntityRef::Thread {
                        thread_id: edge.target.clone(),
                    },
                    EdgeTarget::Session => EntityRef::Session {
                        provider: session_providers
                            .get(&edge.target)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string()),
                        session_id: edge.target.clone(),
                    },
                };
                let mut metadata = BTreeMap::new();
                if let Some(note) = &edge.note {
                    metadata.insert("note".into(), note.clone());
                }
                metadata.insert("created_at".into(), edge.created_at.clone());
                self.insert(
                    Edge {
                        source: source.clone(),
                        kind: thread_edge_kind_name(&edge.kind).to_string(),
                        target,
                        provenance: EdgeProvenance::Explicit,
                        confidence: EdgeConfidence::Exact,
                        metadata,
                    },
                    seen,
                );
            }
        }
    }

    fn project_note_edges(&mut self, notes: &Notes, seen: &mut HashSet<EdgeKey>) {
        for note in notes.all() {
            let note_ref = EntityRef::Note {
                note_id: note.id.clone(),
            };
            if let Some(task_id) = &note.task_id {
                self.insert(
                    exact_edge(
                        EntityRef::Task {
                            task_id: task_id.clone(),
                        },
                        "TASK_PRODUCED_NOTE",
                        note_ref.clone(),
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
            if let Some(session_id) = &note.session_id {
                self.insert(
                    exact_edge(
                        note_ref.clone(),
                        "NOTE_FROM_SESSION",
                        EntityRef::Session {
                            provider: note
                                .provider
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                            session_id: session_id.clone(),
                        },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
            if let Some(thread_id) = &note.thread_id {
                self.insert(
                    exact_edge(
                        note_ref.clone(),
                        "NOTE_IN_THREAD",
                        EntityRef::Thread {
                            thread_id: thread_id.clone(),
                        },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
        }
    }

    fn project_task_edges(
        &mut self,
        rows: &[(String, String, String)],
        seen: &mut HashSet<EdgeKey>,
    ) {
        for (provider, session_id, label) in rows {
            if label.contains("::") || session_id == "pending" {
                continue;
            }
            self.insert(
                exact_edge(
                    EntityRef::Session {
                        provider: provider.clone(),
                        session_id: session_id.clone(),
                    },
                    "SESSION_USED_BROFILE",
                    EntityRef::Brofile {
                        name: label.clone(),
                    },
                    EdgeProvenance::Derived,
                ),
                seen,
            );
        }
    }

    fn project_roadmap_edges(&mut self, roadmap: &Roadmap, seen: &mut HashSet<EdgeKey>) {
        for edge in roadmap.all_edges() {
            let source = match EntityRef::parse(&edge.from) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let target = match EntityRef::parse(&edge.to) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let kind = match edge.kind {
                bbox_stores::roadmap::RoadmapEdgeKind::Spawns => "ROADMAP_SPAWNS",
                bbox_stores::roadmap::RoadmapEdgeKind::DeferredFrom => "ROADMAP_DEFERRED_FROM",
                bbox_stores::roadmap::RoadmapEdgeKind::DesignedIn => "ROADMAP_DESIGNED_IN",
                bbox_stores::roadmap::RoadmapEdgeKind::DependsOn => "ROADMAP_DEPENDS_ON",
                bbox_stores::roadmap::RoadmapEdgeKind::BlockedBy => "ROADMAP_BLOCKED_BY",
                bbox_stores::roadmap::RoadmapEdgeKind::Supersedes => "ROADMAP_SUPERSEDES",
                bbox_stores::roadmap::RoadmapEdgeKind::Subsumes => "ROADMAP_SUBSUMES",
                bbox_stores::roadmap::RoadmapEdgeKind::RelatedTo => "ROADMAP_RELATED_TO",
            };
            self.insert(
                exact_edge(source, kind, target, EdgeProvenance::Derived),
                seen,
            );
        }
    }

    fn project_tantivy_edges(&mut self, index: &TranscriptIndex, seen: &mut HashSet<EdgeKey>) {
        // Docs stream off the segment doc stores one at a time
        // (for_each_edge_projection_doc): transcript docs project straight to
        // IN_SESSION edges and are dropped, so only project_file chunks are
        // buffered — the IN_FILE target (chunk[0] as file proxy) isn't known
        // until every chunk of a file has been seen.
        let mut by_file: HashMap<String, Vec<EdgeProjectionDoc>> = HashMap::new();
        let streamed = index.for_each_edge_projection_doc(|doc| {
            if doc.doc_type == "transcript" && !doc.session_id.is_empty() {
                self.insert(
                    exact_edge(
                        EntityRef::Transcript {
                            provider: doc.account.clone(),
                            session_id: doc.session_id.clone(),
                            line_offset: doc.byte_offset,
                            // TODO(F3+): event_idx is hardcoded 0 because the current schema doesn't store per-line event index; multi-event lines collide on the source ref. Fix when transcript schema gains an event_idx field (likely alongside parser_version bump).
                            event_idx: 0,
                        },
                        "IN_SESSION",
                        EntityRef::Session {
                            provider: doc.account,
                            session_id: doc.session_id,
                        },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            } else if doc.doc_type == "project_file" {
                by_file.entry(doc.file_path.clone()).or_default().push(doc);
            }
            Ok(())
        });
        if let Err(err) = streamed {
            // Don't project IN_FILE edges from a partial buffer: a file whose
            // chunk[0] was never streamed would get the wrong file proxy.
            tracing::warn!(
                error = %err,
                "tantivy edge projection failed mid-stream; skipping stored-doc file edges"
            );
            return;
        }

        for (_path, mut chunks) in by_file {
            chunks.sort_by_key(|doc| doc.project_file_occurrence_idx().unwrap_or(u32::MAX));
            let Some(file_target) = chunks
                .first()
                .and_then(|doc| doc.entity_id.as_deref())
                .and_then(|entity| EntityRef::parse(entity).ok())
            else {
                continue;
            };
            for chunk in &chunks {
                let Some(source) = chunk
                    .entity_id
                    .as_deref()
                    .and_then(|entity| EntityRef::parse(entity).ok())
                else {
                    continue;
                };
                // Skip the chunk[0] -> chunk[0] self-loop. While chunk[0]
                // serves as the file proxy in the current schema (see
                // deferred-thread #5), emitting it as both source and target
                // adds noise to inspect/notable_edges without giving the
                // agent any new information.
                if source == file_target {
                    continue;
                }
                self.insert(
                    exact_edge(
                        source,
                        "IN_FILE",
                        file_target.clone(),
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            }
            // NEXT_SECTION is already projected by the chunker's derive_edges
            // for every adjacent chunk pair. Emitting NEXT_CHUNK/PREV_CHUNK
            // here duplicates the same relationship under different kinds and
            // pollutes notable_edges (the user sees the same target twice
            // under two kinds). Skip the redundant projection.
        }
    }

    fn project_sidecar_edges(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<EdgeKey>,
        include_observed: bool,
    ) {
        let managed_derived_dir = managed_derived_edges_dir(edges_dir);
        let projects_with_managed = scan_managed_derived_project_ids(&managed_derived_dir);
        self.project_sidecar_edges_in_dir(
            edges_dir,
            registered_project_ids,
            seen,
            &projects_with_managed,
        );
        let subdirs: &[&str] = if include_observed {
            &["derived", "explicit", "observed"]
        } else {
            &["derived", "explicit"]
        };
        for sub in subdirs {
            let sub_dir = edges_dir.join(sub);
            if !sub_dir.is_dir() {
                continue;
            }
            let Ok(entries) = fs::read_dir(&sub_dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                self.project_sidecar_edges_in_dir(
                    &path,
                    registered_project_ids,
                    seen,
                    &HashSet::new(),
                );
            }
            self.project_sidecar_edges_in_dir(
                &sub_dir,
                registered_project_ids,
                seen,
                &HashSet::new(),
            );
        }
    }

    fn project_sidecar_edges_in_dir(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<EdgeKey>,
        skip_derived_for: &HashSet<String>,
    ) {
        let Ok(entries) = fs::read_dir(edges_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                tracing::debug!(path = %path.display(), "skipping non-jsonl edge sidecar file");
                continue;
            }
            if !sidecar_project_is_registered(&path, registered_project_ids) {
                tracing::info!(path = %path.display(), "skipping unregistered project edge sidecar");
                continue;
            }
            let skip_derived =
                sidecar_file_stem(&path).is_some_and(|stem| skip_derived_for.contains(stem));
            self.project_sidecar_edges_file(&path, seen, skip_derived);
        }
    }

    fn project_sidecar_edges_file(
        &mut self,
        path: &Path,
        seen: &mut HashSet<EdgeKey>,
        skip_derived: bool,
    ) {
        let Ok(file) = fs::File::open(path) else {
            return;
        };
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if skip_derived && line_provenance_is_derived(trimmed) {
                continue;
            }
            match serde_json::from_str::<Edge>(trimmed) {
                Ok(edge) => {
                    self.insert_sidecar_edge(edge, seen);
                }
                Err(err) => {
                    tracing::warn!(path = %path.display(), error = %err, "failed to parse edge sidecar line");
                }
            }
        }
    }

    fn insert_sidecar_edge(&mut self, edge: Edge, seen: &mut HashSet<EdgeKey>) {
        let derived = derived_tool_projection(&edge);
        self.insert(edge, seen);
        if let Some(edge) = derived {
            self.insert(edge, seen);
        }
    }

    fn load_manifest_active_paths(
        &mut self,
        paths: &[bbox_edge_sidecar::manifest::LoadablePath],
        seen: &mut HashSet<EdgeKey>,
    ) {
        for loadable in paths {
            match &loadable.mode {
                bbox_edge_sidecar::manifest::PathLoadMode::Full => {
                    self.project_sidecar_edges_file(&loadable.path, seen, false);
                }
                bbox_edge_sidecar::manifest::PathLoadMode::FilteredByHash { suppressed_hashes } => {
                    self.project_sidecar_edges_file_with_hash_filter(
                        &loadable.path,
                        seen,
                        suppressed_hashes,
                    );
                }
            }
        }
    }

    fn project_sidecar_edges_file_with_hash_filter(
        &mut self,
        path: &Path,
        seen: &mut HashSet<EdgeKey>,
        suppressed_hashes: &HashSet<String>,
    ) {
        let Ok(file) = fs::File::open(path) else {
            return;
        };
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Edge>(trimmed) {
                Ok(edge) => {
                    if !edge_touches_any_path_hash(&edge, suppressed_hashes) {
                        self.insert_sidecar_edge(edge, seen);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to parse edge sidecar line (hash-filtered load)"
                    );
                }
            }
        }
    }

    fn load_legacy_explicit_edges(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<EdgeKey>,
        include_observed: bool,
    ) {
        let explicit_dir = edges_dir.join("explicit");
        let observed_dir = edges_dir.join("observed");
        let explicit_lane_projects = scan_lane_project_ids(&explicit_dir);
        let observed_lane_projects = scan_lane_project_ids(&observed_dir);
        let mut migrated_projects: HashSet<String> = HashSet::new();
        for pid in &explicit_lane_projects {
            migrated_projects.insert(pid.clone());
        }
        for pid in &observed_lane_projects {
            migrated_projects.insert(pid.clone());
        }

        for project_id in &migrated_projects {
            if !sidecar_project_id_is_registered(project_id, registered_project_ids) {
                continue;
            }
            if explicit_lane_projects.contains(project_id) {
                let path = explicit_dir.join(format!("{project_id}.jsonl"));
                if path.exists() {
                    self.project_sidecar_edges_file(&path, seen, false);
                }
            }
            if include_observed && observed_lane_projects.contains(project_id) {
                let path = observed_dir.join(format!("{project_id}.jsonl"));
                if path.exists() {
                    self.project_sidecar_edges_file(&path, seen, false);
                }
            }
        }

        let managed_derived_dir = managed_derived_edges_dir(edges_dir);
        let projects_with_managed = scan_managed_derived_project_ids(&managed_derived_dir);
        let Ok(entries) = fs::read_dir(edges_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if !sidecar_project_is_registered(&path, registered_project_ids) {
                continue;
            }
            if let Some(stem) = sidecar_file_stem(&path) {
                if migrated_projects.contains(stem) {
                    continue;
                }
            }
            let skip_derived =
                sidecar_file_stem(&path).is_some_and(|stem| projects_with_managed.contains(stem));
            self.project_sidecar_edges_file(&path, seen, skip_derived);
        }
    }
}

fn thread_edge_kind_name(kind: &EdgeKind) -> &'static str {
    match kind {
        EdgeKind::SpawnedFrom => "THREAD_SPAWNED_FROM",
        EdgeKind::BlockedBy => "THREAD_BLOCKED_BY",
        EdgeKind::RelatesTo => "THREAD_RELATES_TO",
        EdgeKind::Subsumes => "THREAD_SUBSUMES",
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::*;

    #[test]
    fn tantivy_projection_streams_session_and_file_edges() {
        let dir = tempfile::tempdir().unwrap();
        let index = TranscriptIndex::open_or_create(
            &dir.path().join("index"),
            Vec::new(),
            None,
            dir.path().join("projects.json"),
            dir.path().join("knowledge.json"),
            dir.path().join("threads.json"),
            dir.path().join("roadmap.json"),
        )
        .unwrap();
        let fields = index.field_handles();
        let mut writer = index.index_handle().writer(50_000_000).unwrap();

        let mut transcript = tantivy::TantivyDocument::new();
        transcript.add_text(fields.doc_type, "transcript");
        transcript.add_text(fields.account, "claude");
        transcript.add_text(fields.session_id, "sess-1");
        transcript.add_u64(fields.byte_offset, 7);
        writer.add_document(transcript).unwrap();

        for idx in 0..3u32 {
            let mut chunk = tantivy::TantivyDocument::new();
            chunk.add_text(fields.doc_type, "project_file");
            chunk.add_text(fields.file_path, "src/lib.rs");
            chunk.add_text(
                fields.entity_id,
                format!("project_file:proj1:relhash:chunk{idx}:{idx}"),
            );
            writer.add_document(chunk).unwrap();
        }
        writer.commit().unwrap();
        index.reader_reload_for_test();

        let mut edge_index = EdgeIndex::default();
        let mut seen = HashSet::new();
        edge_index.project_tantivy_edges(&index, &mut seen);

        let session = EntityRef::Session {
            provider: "claude".into(),
            session_id: "sess-1".into(),
        };
        assert_eq!(
            edge_index
                .reverse_edges_filtered(&session, &["IN_SESSION"])
                .len(),
            1
        );

        let file_target = EntityRef::parse("project_file:proj1:relhash:chunk0:0").unwrap();
        assert_eq!(
            edge_index
                .reverse_edges_filtered(&file_target, &["IN_FILE"])
                .len(),
            2,
            "chunk[1] and chunk[2] point at the chunk[0] file proxy"
        );
        assert!(
            edge_index.forward_edges(&file_target).is_empty(),
            "chunk[0] -> chunk[0] self-loop must be skipped"
        );
    }

    #[test]
    fn forward_and_reverse_lookup_are_indexed() {
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        let source = EntityRef::Knowledge { id: "new".into() };
        let target = EntityRef::Knowledge { id: "old".into() };
        index.insert(
            exact_edge(
                source.clone(),
                "SUPERSEDES",
                target.clone(),
                EdgeProvenance::Derived,
            ),
            &mut seen,
        );

        assert_eq!(index.forward_edges(&source).len(), 1);
        assert_eq!(index.reverse_edges(&target).len(), 1);
        assert_eq!(
            index.forward_edges_filtered(&source, &["SUPERSEDES"]).len(),
            1
        );
        assert!(index.reverse_edges_filtered(&target, &["CALLS"]).is_empty());
    }

    #[test]
    fn forward_edges_with_synthesis_fills_in_missing_in_session() {
        // gap-edc84378: with the tantivy stored-doc projection opt-in and
        // off in every caller, a transcript ref with no materialized edges
        // must still resolve its IN_SESSION edge at query time.
        let index = EdgeIndex::default();
        let transcript = EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        };
        let edges = index.forward_edges_with_synthesis(&transcript);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, "IN_SESSION");
        assert_eq!(
            edges[0].target,
            EntityRef::Session {
                provider: "claude".into(),
                session_id: "sess-1".into(),
            }
        );
        assert_eq!(edges[0].provenance, EdgeProvenance::Derived);
    }

    #[test]
    fn forward_edges_with_synthesis_does_not_duplicate_materialized_in_session() {
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        let transcript = EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        };
        let session = EntityRef::Session {
            provider: "claude".into(),
            session_id: "sess-1".into(),
        };
        // A materialized edge (e.g. loaded from the tantivy projection or a
        // sidecar) is already present -- synthesis must not add a second one.
        index.insert(
            exact_edge(
                transcript.clone(),
                "IN_SESSION",
                session.clone(),
                EdgeProvenance::Derived,
            ),
            &mut seen,
        );

        let edges = index.forward_edges_with_synthesis(&transcript);
        assert_eq!(edges.len(), 1, "materialized edge must not be duplicated");
        assert_eq!(edges[0].target, session);
    }

    #[test]
    fn forward_edges_with_synthesis_leaves_other_ref_types_untouched() {
        let index = EdgeIndex::default();
        let knowledge = EntityRef::Knowledge { id: "k-1".into() };
        assert!(index.forward_edges_with_synthesis(&knowledge).is_empty());
    }

    #[test]
    fn forward_edges_with_synthesis_preserves_other_materialized_edges() {
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        let transcript = EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        };
        let file = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        index.insert(
            exact_edge(
                transcript.clone(),
                "EDITED_FILE",
                file.clone(),
                EdgeProvenance::Derived,
            ),
            &mut seen,
        );

        let edges = index.forward_edges_with_synthesis(&transcript);
        assert_eq!(
            edges.len(),
            2,
            "materialized edge plus synthesized IN_SESSION"
        );
        assert!(
            edges
                .iter()
                .any(|edge| edge.kind == "EDITED_FILE" && edge.target == file)
        );
        assert!(edges.iter().any(|edge| edge.kind == "IN_SESSION"));
    }

    #[test]
    fn knowledge_links_project_authored_edges() {
        use bbox_knowledge::knowledge::{
            Approval, Category, KnowledgeEdge, KnowledgeEdgeKind, KnowledgeEntry, Priority, Scope,
            Status,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut knowledge = Knowledge::open(&dir.path().join("knowledge.json")).unwrap();
        let now = "2026-01-01T00:00:00Z".to_string();
        knowledge
            .upsert_generated(KnowledgeEntry {
                id: "aaaabbbb".into(),
                title: "A".into(),
                content: "claim A".into(),
                cluster: None,
                variants: Default::default(),
                category: Category::Memory,
                scope: Scope::Global,
                project: None,
                providers: Vec::new(),
                priority: Priority::Standard,
                weight: 100,
                status: Status::Active,
                approval: Approval::UserConfirmed,
                render: false,
                decay: true,
                review_at: None,
                supersedes: None,
                links: vec![KnowledgeEdge {
                    target: "knowledge:ccccdddd".into(),
                    kind: KnowledgeEdgeKind::Contradicts,
                    note: Some("claims conflict".into()),
                    source_arc: Some("arc-123".into()),
                    confidence: EdgeConfidence::Heuristic,
                }],
                rationale: None,
                expires_at: None,
                source: "test".into(),
                created_at: now.clone(),
                updated_at: now,
                recall_count: 0,
                last_recalled: None,
            })
            .unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_knowledge_edges(&knowledge, &mut seen);

        let source = EntityRef::Knowledge {
            id: "aaaabbbb".into(),
        };
        let target = EntityRef::Knowledge {
            id: "ccccdddd".into(),
        };
        let edges = index.forward_edges_filtered(&source, &["Contradicts"]);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].target, target);
        assert_eq!(edges[0].provenance, EdgeProvenance::Explicit);
        assert_eq!(edges[0].confidence, EdgeConfidence::Heuristic);
        assert_eq!(
            edges[0].metadata.get("source_arc").map(String::as_str),
            Some("arc-123")
        );
    }

    #[test]
    fn thread_store_edges_project_into_agentic_graph() {
        use bbox_threads::threads::{ThreadParams, Threads};

        fn params(action: &str) -> ThreadParams {
            ThreadParams {
                action: action.into(),
                name: None,
                id: None,
                topic: None,
                project: None,
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: None,
                origin: None,
            }
        }

        fn created_id(output: &str) -> String {
            output
                .split_whitespace()
                .find(|part| part.starts_with("thread-"))
                .unwrap()
                .to_string()
        }

        let dir = tempfile::tempdir().unwrap();
        let mut threads = Threads::open(&dir.path().join("threads.json")).unwrap();
        let mut open_parent = params("open");
        open_parent.topic = Some("parent".into());
        open_parent.project = Some("/repo".into());
        let parent = created_id(&threads.thread(&open_parent).unwrap());

        let mut open_child = params("open");
        open_child.topic = Some("child".into());
        open_child.project = Some("/repo".into());
        open_child.session_id = Some("sess-1".into());
        open_child.provider = Some("claude".into());
        let child = created_id(&threads.thread(&open_child).unwrap());

        let mut link = params("link");
        link.id = Some(child.clone());
        link.edge = Some("spawned_from".into());
        link.target = Some(parent.clone());
        link.target_type = Some("thread".into());
        link.note = Some("child came from parent".into());
        threads.thread(&link).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_thread_edges(&threads, &mut seen);

        let child_ref = EntityRef::Thread {
            thread_id: child.clone(),
        };
        assert!(index.forward_edges(&child_ref).iter().any(|edge| edge.kind
            == "THREAD_HAS_SESSION"
            && edge.target
                == (EntityRef::Session {
                    provider: "claude".into(),
                    session_id: "sess-1".into(),
                })));
        let relation = index
            .forward_edges_filtered(&child_ref, &["THREAD_SPAWNED_FROM"])
            .into_iter()
            .next()
            .expect("spawned_from edge should project");
        assert_eq!(relation.target, EntityRef::Thread { thread_id: parent });
        assert_eq!(
            relation.metadata.get("note").map(String::as_str),
            Some("child came from parent")
        );
    }

    #[test]
    fn project_edge_sidecar_round_trips_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let edge = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[edge.clone(), edge]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert_eq!(index.forward_edges(&source).len(), 1);
        assert_eq!(index.reverse_edges(&target).len(), 1);
    }

    #[test]
    fn purge_managed_edges_removes_only_deleted_file_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let proj = "projpurge";

        let keep_file = EntityRef::ProjectFile {
            project_id: proj.into(),
            rel_path_hash: "keephash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let del_file = EntityRef::ProjectFile {
            project_id: proj.into(),
            rel_path_hash: "delhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        };
        let sym_a = EntityRef::SymbolV2 {
            project_id: proj.into(),
            snapshot_id: "snap".into(),
            qualified_name: "pkg.A".into(),
            defn_hash: "c".repeat(64),
        };
        let sym_b = EntityRef::SymbolV2 {
            project_id: proj.into(),
            snapshot_id: "snap".into(),
            qualified_name: "pkg.B".into(),
            defn_hash: "d".repeat(64),
        };
        let mk = |s: EntityRef, k: &str, t: EntityRef| bbox_chunker::Edge {
            source: s,
            kind: k.into(),
            target: t,
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let edges = vec![
            mk(keep_file.clone(), "NEXT_SECTION", keep_file.clone()),
            mk(del_file.clone(), "NEXT_SECTION", del_file.clone()),
            // symbol→symbol edge: carries no project-file ref, so it is retained.
            mk(sym_a.clone(), "CALLS", sym_b.clone()),
        ];
        replace_project_edges(edges_dir, "project", proj, &edges).unwrap();

        let mut stale = HashSet::new();
        stale.insert("delhash".to_string());
        let purged =
            purge_managed_edges_for_path_hashes(edges_dir, "project", proj, &stale).unwrap();
        assert_eq!(purged, 1, "only the deleted file's edge is removed");

        let remaining = read_managed_derived_edges(edges_dir, "project", proj).unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining.iter().any(|e| e.source == keep_file),
            "kept file's edge retained"
        );
        assert!(
            remaining.iter().any(|e| e.source == sym_a),
            "symbol→symbol edge retained (no file ref)"
        );
        assert!(
            !remaining.iter().any(|e| e.source == del_file),
            "deleted file's edge purged"
        );

        // Empty stale set is a no-op.
        assert_eq!(
            purge_managed_edges_for_path_hashes(edges_dir, "project", proj, &HashSet::new())
                .unwrap(),
            0
        );
    }

    #[test]
    fn entity_type_counts_dedupe_refs_without_sorting() {
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        let file = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let symbol = EntityRef::Symbol {
            project_id: "proj1234".into(),
            qualified_name: "pkg.Type".into(),
            defn_hash: "b".repeat(64),
        };
        index.insert(
            Edge {
                source: file.clone(),
                kind: "CONTAINS_SYMBOL".into(),
                target: symbol.clone(),
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
            },
            &mut seen,
        );
        index.insert(
            Edge {
                source: symbol,
                kind: "DEFINED_IN".into(),
                target: file,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
            },
            &mut seen,
        );

        let counts = index.entity_type_counts();
        assert_eq!(counts.get("project_file"), Some(&1));
        assert_eq!(counts.get("symbol"), Some(&1));
    }

    #[test]
    fn sidecar_loader_skips_unregistered_project_ids() {
        let dir = tempfile::tempdir().unwrap();
        let registered_source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let registered_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let orphan_source = EntityRef::ProjectFile {
            project_id: "orphan12".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 0,
        };
        let orphan_target = EntityRef::ProjectFile {
            project_id: "orphan12".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "d".repeat(64),
            occurrence_idx: 1,
        };
        append_project_edges(
            dir.path(),
            "proj1234",
            &[bbox_chunker::Edge {
                source: registered_source.clone(),
                kind: "NEXT_SECTION".into(),
                target: registered_target,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
            }],
        )
        .unwrap();
        append_project_edges(
            dir.path(),
            "orphan12",
            &[bbox_chunker::Edge {
                source: orphan_source.clone(),
                kind: "NEXT_SECTION".into(),
                target: orphan_target,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
            }],
        )
        .unwrap();

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen, true);

        assert_eq!(index.forward_edges(&registered_source).len(), 1);
        assert!(index.forward_edges(&orphan_source).is_empty());
    }

    #[test]
    fn sidecar_loader_keeps_global_agent_edges_with_project_filter() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::Agent {
            name: "distilled-reviewer".into(),
            version: 1,
        };
        let target = EntityRef::Session {
            provider: "claude".into(),
            session_id: "sess-1".into(),
        };
        append_edges_dedup(
            dir.path(),
            "agents",
            &[Edge {
                source: source.clone(),
                kind: "DERIVED_FROM".into(),
                target,
                provenance: EdgeProvenance::Explicit,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
            }],
        )
        .unwrap();

        let registered = HashSet::new();
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen, true);

        assert_eq!(index.forward_edges(&source).len(), 1);
    }

    #[test]
    fn sidecar_loader_filters_managed_derived_project_ids() {
        let dir = tempfile::tempdir().unwrap();
        let registered_source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let registered_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let orphan_source = EntityRef::ProjectFile {
            project_id: "orphan12".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 0,
        };
        let orphan_target = EntityRef::ProjectFile {
            project_id: "orphan12".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "d".repeat(64),
            occurrence_idx: 1,
        };
        replace_project_edges(
            dir.path(),
            "project",
            "proj1234",
            &[bbox_chunker::Edge {
                source: registered_source.clone(),
                kind: "NEXT_SECTION".into(),
                target: registered_target,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
            }],
        )
        .unwrap();
        replace_project_edges(
            dir.path(),
            "project",
            "orphan12",
            &[bbox_chunker::Edge {
                source: orphan_source.clone(),
                kind: "NEXT_SECTION".into(),
                target: orphan_target,
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
            }],
        )
        .unwrap();

        let mut registered = HashSet::new();
        registered.insert("proj1234".to_string());
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen, true);

        assert_eq!(index.forward_edges(&registered_source).len(), 1);
        assert!(index.forward_edges(&orphan_source).is_empty());
    }

    #[test]
    fn managed_project_edge_sidecar_replaces_previous_contents() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let first_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let second_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 2,
        };
        let first = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: first_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let second = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: second_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        replace_project_edges(dir.path(), "project", "proj1234", &[first]).unwrap();
        replace_project_edges(dir.path(), "project", "proj1234", &[second]).unwrap();

        let sidecar = fs::read_to_string(
            dir.path()
                .join("derived")
                .join("project")
                .join("proj1234.jsonl"),
        )
        .unwrap();
        assert_eq!(sidecar.lines().count(), 1);

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert!(
            index
                .forward_edges(&source)
                .iter()
                .any(|edge| edge.target == second_target)
        );
        assert!(
            !index
                .forward_edges(&source)
                .iter()
                .any(|edge| edge.target == first_target)
        );
    }

    #[test]
    fn managed_derived_sidecar_overrides_legacy_derived_edges() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let legacy_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let managed_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 2,
        };
        let legacy = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: legacy_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[legacy]).unwrap();
        let managed = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: managed_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        replace_project_edges(dir.path(), "project", "proj1234", &[managed]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert!(
            index
                .forward_edges(&source)
                .iter()
                .any(|edge| edge.target == managed_target),
            "managed derived edge should be loaded"
        );
        assert!(
            !index
                .forward_edges(&source)
                .iter()
                .any(|edge| edge.target == legacy_target),
            "legacy derived edge should be skipped when managed derived sidecar exists"
        );
    }

    #[test]
    fn legacy_explicit_edges_preserved_when_managed_derived_exists() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::Knowledge { id: "k-abc".into() };
        let explicit_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        };
        let derived_target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "c".repeat(64),
            occurrence_idx: 1,
        };
        let legacy_explicit = Edge {
            source: source.clone(),
            kind: "DESCRIBES".into(),
            target: explicit_target.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        };
        let legacy_derived = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: derived_target,
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_edges(dir.path(), "proj1234", &[legacy_explicit]).unwrap();
        append_project_edges(dir.path(), "proj1234", &[legacy_derived]).unwrap();

        let managed = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "other".into(),
                chunk_hash: "d".repeat(64),
                occurrence_idx: 2,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        replace_project_edges(dir.path(), "project", "proj1234", &[managed]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert!(
            index
                .forward_edges(&source)
                .iter()
                .any(|e| e.target == explicit_target),
            "explicit edge from legacy sidecar must be preserved"
        );
        assert_eq!(
            index
                .forward_edges(&EntityRef::ProjectFile {
                    project_id: "proj1234".into(),
                    rel_path_hash: "pathhash".into(),
                    chunk_hash: "a".repeat(64),
                    occurrence_idx: 0,
                })
                .len(),
            1,
            "only managed derived edge should load, not legacy derived"
        );
    }

    #[test]
    fn legacy_derived_edges_loaded_when_no_managed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 1,
        };
        let derived = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[derived]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert!(
            index
                .forward_edges(&source)
                .iter()
                .any(|e| e.target == target),
            "legacy derived edge must load when no managed sidecar exists"
        );
    }

    #[test]
    fn backup_and_temp_files_not_loaded_by_sidecar_loader() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().join("edges");
        fs::create_dir_all(&edges_dir).unwrap();

        let backup_path = edges_dir.join("proj1234.jsonl.bak-20260513");
        let temp_path = edges_dir.join("proj1234.jsonl.compact-12345");
        let mut f = fs::File::create(&backup_path).unwrap();
        writeln!(f, "not a real edge but should not be read").unwrap();
        drop(f);
        let mut f = fs::File::create(&temp_path).unwrap();
        writeln!(f, "not a real edge but should not be read").unwrap();
        drop(f);

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges_in_dir(&edges_dir, None, &mut seen, &HashSet::new());

        assert_eq!(
            index.edge_count(),
            0,
            "backup and temp files must not be loaded"
        );
    }

    #[test]
    fn line_provenance_is_derived_matches_exact_serialized_forms() {
        assert!(
            line_provenance_is_derived(
                r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"derived","confidence":"exact"}"#
            ),
            "compact JSON with derived provenance"
        );
        assert!(
            line_provenance_is_derived(
                r#"{"source":"k:abc", "kind":"DESCRIBES", "target":"k:def", "provenance": "derived", "confidence":"exact"}"#
            ),
            "JSON with spaces around colon/value for provenance"
        );
        assert!(
            !line_provenance_is_derived(
                r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"explicit","confidence":"exact"}"#
            ),
            "explicit provenance must not match"
        );
        assert!(
            !line_provenance_is_derived(
                r#"{"source":"k:abc","kind":"DESCRIBES","target":"k:def","provenance":"implicit","confidence":"exact"}"#
            ),
            "implicit provenance must not match"
        );
        assert!(
            !line_provenance_is_derived("not valid json at all"),
            "malformed line must not match"
        );
        assert!(
            !line_provenance_is_derived("{\"provenance\":\"derivedly_wrong\"}"),
            "substring that is not exact value must not match"
        );
        assert!(
            !line_provenance_is_derived(
                "{\"source\":\"k:abc\",\"kind\":\"DESCRIBES\",\"target\":\"k:def\",\"provenance\":\"explicit\",\"confidence\":\"exact\",\"metadata\":{\"nested\":\"provenance\\\":\\\"derived\\\"\"}"
            ),
            "explicit top-level with derived-like substring in metadata must not false-skip"
        );
        assert!(
            !line_provenance_is_derived("no provenance field at all"),
            "line without provenance key must not match"
        );
    }

    #[test]
    fn managed_derived_sidecar_supersedes_legacy_derived_while_preserving_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj9999".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let legacy_derived = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "proj9999".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "b".repeat(64),
                occurrence_idx: 1,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let legacy_explicit = Edge {
            source: EntityRef::Knowledge { id: "k-1".into() },
            kind: "DESCRIBES".into(),
            target: source.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        };
        append_project_edges(dir.path(), "proj9999", &[legacy_derived]).unwrap();
        append_edges(dir.path(), "proj9999", &[legacy_explicit]).unwrap();

        let managed = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "proj9999".into(),
                rel_path_hash: "other".into(),
                chunk_hash: "c".repeat(64),
                occurrence_idx: 2,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        replace_project_edges(dir.path(), "project", "proj9999", &[managed]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert_eq!(
            index.forward_edges(&source).len(),
            1,
            "only managed derived edge should appear"
        );
        assert!(
            index
                .forward_edges(&EntityRef::Knowledge { id: "k-1".into() })
                .iter()
                .any(|e| e.kind == "DESCRIBES"),
            "explicit edge from legacy sidecar must survive"
        );
    }

    #[test]
    fn compact_legacy_sidecar_removes_only_derived_edges() {
        let dir = tempfile::tempdir().unwrap();
        let source = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let derived = bbox_chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "b".repeat(64),
                occurrence_idx: 1,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[derived]).unwrap();
        let explicit = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-1".into(),
                line_offset: 42,
                event_idx: 0,
            },
            kind: "READ_FILE".into(),
            target: source.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        };
        append_edges(dir.path(), "proj1234", &[explicit]).unwrap();

        let dry_run = compact_legacy_sidecar(dir.path(), "proj1234", false).unwrap();
        assert!(!dry_run.applied);
        assert_eq!(dry_run.derived_edges_removed, 1);
        assert_eq!(dry_run.explicit_edges_retained, 1);

        let applied = compact_legacy_sidecar(dir.path(), "proj1234", true).unwrap();
        assert!(applied.applied);
        assert_eq!(applied.derived_edges_removed, 1);
        assert!(applied.backup_path.is_some());

        let compacted = fs::read_to_string(dir.path().join("proj1234.jsonl")).unwrap();
        assert_eq!(compacted.lines().count(), 1);
        assert!(compacted.contains("READ_FILE"));
        assert!(!compacted.contains("NEXT_SECTION"));
    }

    #[test]
    fn edited_file_sidecar_projects_session_edge() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-1".into(),
            line_offset: 42,
            event_idx: 0,
        };
        let file = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let edge = Edge {
            source: transcript,
            kind: "EDITED_FILE".into(),
            target: file.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        };
        append_edges(dir.path(), "proj1234", &[edge]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen, true);

        assert!(
            index
                .forward_edges(&file)
                .iter()
                .any(|edge| edge.kind == "EDITED_BY_SESSION"
                    && edge.target
                        == (EntityRef::Session {
                            provider: "claude".into(),
                            session_id: "sess-1".into(),
                        }))
        );
    }

    #[test]
    fn append_edges_dedup_skips_reimported_provenance_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edge = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-1".into(),
                line_offset: 42,
                event_idx: 0,
            },
            kind: "EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: "proj1234".into(),
                rel_path_hash: "pathhash".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::from([("anchor.commit_sha_at_edit".into(), "abc123".into())]),
        };

        assert_eq!(
            append_edges_dedup(dir.path(), "proj1234", std::slice::from_ref(&edge)).unwrap(),
            1
        );
        assert_eq!(
            append_edges_dedup(dir.path(), "proj1234", std::slice::from_ref(&edge)).unwrap(),
            0
        );
        let sidecar = fs::read_to_string(dir.path().join("proj1234.jsonl")).unwrap();
        assert_eq!(sidecar.lines().count(), 1);
    }

    // -----------------------------------------------------------------------
    // Phase 2 tests
    // -----------------------------------------------------------------------

    fn derived_chunker_edge(kind: &str) -> bbox_chunker::Edge {
        bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "h1".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            kind: kind.into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "h2".into(),
                chunk_hash: "b".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        }
    }

    fn explicit_edge(kind: &str) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: "k1".into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: "k2".into() },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        }
    }

    fn observed_tool_edge(kind: &str) -> Edge {
        Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-1".into(),
                line_offset: 10,
                event_idx: 0,
            },
            kind: kind.into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "h1".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn replace_materialized_replaces_not_appends() {
        let dir = tempfile::tempdir().unwrap();
        let first = derived_chunker_edge("CALLS");
        let second = derived_chunker_edge("USES_TYPE");

        replace_materialized_edges(dir.path(), "project", "p1", &[first]).unwrap();
        let sidecar_path = dir.path().join("derived").join("project").join("p1.jsonl");
        let content1 = fs::read_to_string(&sidecar_path).unwrap();
        assert_eq!(content1.lines().count(), 1);

        replace_materialized_edges(dir.path(), "project", "p1", &[second.clone(), second]).unwrap();
        let content2 = fs::read_to_string(&sidecar_path).unwrap();
        assert_eq!(content2.lines().count(), 2, "replacement must not append");
    }

    #[test]
    #[should_panic(expected = "rejected non-Derived edge")]
    fn replace_materialized_rejects_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = derived_chunker_edge("CALLS");
        e.provenance = EdgeProvenance::Explicit;
        let _ = replace_materialized_edges(dir.path(), "project", "p1", &[e]);
    }

    #[test]
    #[should_panic(expected = "rejected Derived edge")]
    fn append_explicit_rejects_derived() {
        let dir = tempfile::tempdir().unwrap();
        let e = Edge {
            source: EntityRef::Knowledge { id: "k1".into() },
            kind: "CALLS".into(),
            target: EntityRef::Knowledge { id: "k2".into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        };
        let _ = append_explicit_edges(dir.path(), "p1", &[e]);
    }

    #[test]
    #[should_panic(expected = "rejected Derived edge")]
    fn append_observed_rejects_derived() {
        let dir = tempfile::tempdir().unwrap();
        let e = Edge {
            source: EntityRef::Knowledge { id: "k1".into() },
            kind: "READ_FILE".into(),
            target: EntityRef::Knowledge { id: "k2".into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        };
        let _ = append_observed_edges(dir.path(), "p1", &[e]);
    }

    #[test]
    fn append_explicit_dedups_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let e = explicit_edge("DESCRIBES");
        let n = append_explicit_edges(dir.path(), "p1", std::slice::from_ref(&e)).unwrap();
        assert_eq!(n, 1);
        let n2 = append_explicit_edges(dir.path(), "p1", &[e]).unwrap();
        assert_eq!(n2, 0, "dedup must skip reimport");
    }

    #[test]
    fn append_observed_writes_tool_edges() {
        let dir = tempfile::tempdir().unwrap();
        let e = observed_tool_edge("READ_FILE");
        append_observed_edges(dir.path(), "p1", &[e]).unwrap();
        let sidecar = fs::read_to_string(dir.path().join("p1.jsonl")).unwrap();
        assert_eq!(sidecar.lines().count(), 1);
    }

    #[test]
    fn plan_legacy_extraction_classifies_lines() {
        let dir = tempfile::tempdir().unwrap();

        let derived = derived_chunker_edge("NEXT_SECTION");
        let tool = observed_tool_edge("READ_FILE");
        let explicit = explicit_edge("SUPERSEDES");

        append_project_edges(dir.path(), "p1", &[derived]).unwrap();
        append_edges(dir.path(), "p1", &[tool]).unwrap();
        append_edges(dir.path(), "p1", &[explicit]).unwrap();
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(dir.path().join("p1.jsonl"))
                .unwrap();
            f.write_all(b"\n").unwrap();
            f.write_all(b"not json\n").unwrap();
        }

        let plan = plan_legacy_edge_extraction(dir.path(), "p1").unwrap();
        assert_eq!(plan.total_lines, 5);
        assert_eq!(plan.derived_lines, 1);
        assert_eq!(plan.tool_lines, 1);
        assert_eq!(plan.explicit_lines, 1);
        assert_eq!(plan.blank_lines, 1);
        assert_eq!(plan.malformed_lines, 1);
        assert!(!plan.managed_replacement_exists);
        assert!(!plan.extractable);
    }

    #[test]
    fn plan_legacy_extraction_detects_managed_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let derived = derived_chunker_edge("NEXT_SECTION");
        append_project_edges(dir.path(), "p1", std::slice::from_ref(&derived)).unwrap();

        let plan_before = plan_legacy_edge_extraction(dir.path(), "p1").unwrap();
        assert!(!plan_before.managed_replacement_exists);

        replace_materialized_edges(dir.path(), "project", "p1", &[derived]).unwrap();

        let plan_after = plan_legacy_edge_extraction(dir.path(), "p1").unwrap();
        assert!(plan_after.managed_replacement_exists);
        assert!(plan_after.extractable);
    }

    #[test]
    fn repeated_materialized_replace_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let edge = derived_chunker_edge("CALLS");

        for _ in 0..5 {
            replace_materialized_edges(dir.path(), "project", "p1", std::slice::from_ref(&edge))
                .unwrap();
        }

        let sidecar_path = dir.path().join("derived").join("project").join("p1.jsonl");
        let content = fs::read_to_string(&sidecar_path).unwrap();
        assert_eq!(
            content.lines().count(),
            1,
            "repeated replacement must not grow line count"
        );
    }

    #[test]
    fn legacy_sidecar_still_loads_for_compatibility() {
        let dir = tempfile::tempdir().unwrap();
        let derived = derived_chunker_edge("NEXT_SECTION");
        let explicit = explicit_edge("DESCRIBES");
        append_project_edges(dir.path(), "p1", &[derived]).unwrap();
        append_edges(dir.path(), "p1", &[explicit]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        let skip = HashSet::new();
        index.project_sidecar_edges_in_dir(dir.path(), None, &mut seen, &skip);

        let source = EntityRef::ProjectFile {
            project_id: "p1".into(),
            rel_path_hash: "h1".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        assert!(
            index
                .forward_edges_filtered(&source, &["NEXT_SECTION"])
                .len()
                == 1,
            "legacy derived edge must still load"
        );
    }

    #[test]
    fn incremental_materialized_replace_preserves_unchanged_file_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let file_a = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "aaa".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "aaa".into(),
                chunk_hash: "b".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let file_b = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "bbb".into(),
                chunk_hash: "c".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "bbb".into(),
                chunk_hash: "d".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        replace_materialized_edges(
            edges_dir,
            "project",
            "p1",
            &[file_a.clone(), file_b.clone()],
        )
        .unwrap();

        let after_full = read_managed_derived_edges(edges_dir, "project", "p1").unwrap();
        assert_eq!(after_full.len(), 2);

        let file_a_updated = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "aaa".into(),
                chunk_hash: "e".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "aaa".into(),
                chunk_hash: "f".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        replace_materialized_edges_incremental(edges_dir, "project", "p1", &[file_a_updated])
            .unwrap();

        let after_incremental = read_managed_derived_edges(edges_dir, "project", "p1").unwrap();
        assert_eq!(after_incremental.len(), 2, "total edges must stay at 2");

        let b_edges: Vec<_> = after_incremental
            .iter()
            .filter(|e| match &e.source {
                EntityRef::ProjectFile { rel_path_hash, .. } => rel_path_hash == "bbb",
                _ => false,
            })
            .collect();
        assert_eq!(b_edges.len(), 1, "unchanged file-b edge must be preserved");

        let a_edges: Vec<_> = after_incremental
            .iter()
            .filter(|e| match &e.source {
                EntityRef::ProjectFile { rel_path_hash, .. } => rel_path_hash == "aaa",
                _ => false,
            })
            .collect();
        assert_eq!(a_edges.len(), 1, "updated file-a edge must be present");
        assert_eq!(a_edges[0].kind, "NEXT_SECTION");
    }

    #[test]
    fn incremental_materialized_replace_no_duplicates_on_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let edge = bbox_chunker::Edge {
            source: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "xxx".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            kind: "NEXT_SECTION".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "xxx".into(),
                chunk_hash: "b".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        replace_materialized_edges_incremental(
            dir.path(),
            "project",
            "p1",
            std::slice::from_ref(&edge),
        )
        .unwrap();
        replace_materialized_edges_incremental(dir.path(), "project", "p1", &[edge]).unwrap();

        let after = read_managed_derived_edges(dir.path(), "project", "p1").unwrap();
        assert_eq!(
            after.len(),
            1,
            "re-incremental with same edges must not duplicate"
        );
    }

    #[test]
    fn merge_materialized_git_preserves_old_commits_and_appends_new() {
        let dir = tempfile::tempdir().unwrap();

        let old_commit_edge = bbox_chunker::Edge {
            source: EntityRef::Commit {
                repo_id: "repo1".into(),
                sha: "aaaaaa".into(),
            },
            kind: "COMMIT_EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "fff".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let new_commit_edge = bbox_chunker::Edge {
            source: EntityRef::Commit {
                repo_id: "repo1".into(),
                sha: "bbbbbb".into(),
            },
            kind: "COMMIT_EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "fff".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        replace_materialized_edges(
            dir.path(),
            "git",
            "p1",
            std::slice::from_ref(&old_commit_edge),
        )
        .unwrap();
        let after_full = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
        assert_eq!(after_full.len(), 1);

        merge_materialized_edges(
            dir.path(),
            "git",
            "p1",
            std::slice::from_ref(&new_commit_edge),
        )
        .unwrap();
        let after_merge = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
        assert_eq!(after_merge.len(), 2, "old + new commit edges");

        let has_old = after_merge.iter().any(|e| match &e.source {
            EntityRef::Commit { sha, .. } => sha == "aaaaaa",
            _ => false,
        });
        let has_new = after_merge.iter().any(|e| match &e.source {
            EntityRef::Commit { sha, .. } => sha == "bbbbbb",
            _ => false,
        });
        assert!(has_old, "old commit edge must be preserved");
        assert!(has_new, "new commit edge must be appended");
    }

    #[test]
    fn merge_materialized_git_no_duplicates_on_repeated_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let edge = bbox_chunker::Edge {
            source: EntityRef::Commit {
                repo_id: "repo1".into(),
                sha: "cccccc".into(),
            },
            kind: "COMMIT_EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: "p1".into(),
                rel_path_hash: "hhh".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };

        merge_materialized_edges(dir.path(), "git", "p1", std::slice::from_ref(&edge)).unwrap();
        merge_materialized_edges(dir.path(), "git", "p1", &[edge]).unwrap();

        let after = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
        assert_eq!(after.len(), 1, "re-merge with same edge must not duplicate");
    }

    // -----------------------------------------------------------------------
    // Phase 3: EdgeIndex rebuild manifest-mode tests
    // -----------------------------------------------------------------------

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut content = lines.join("\n");
        if !content.is_empty() {
            content.push('\n');
        }
        fs::write(path, content).unwrap();
    }

    fn make_explicit_edge_line(source: &str, kind: &str, target: &str) -> String {
        serde_json::to_string(&Edge {
            source: EntityRef::Knowledge { id: source.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        })
        .unwrap()
    }

    fn make_derived_edge_line(source: &str, kind: &str, target: &str) -> String {
        serde_json::to_string(&Edge {
            source: EntityRef::Knowledge { id: source.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        })
        .unwrap()
    }

    #[test]
    fn load_sidecar_manifest_mode_active_snapshot_loads_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let active_edge = make_derived_edge_line("k_active", "DESCRIBES", "k_target");
        let snap_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-abc");
        write_jsonl(&snap_dir.join("project.jsonl"), &[&active_edge]);

        let inactive_edge = make_derived_edge_line("k_stale", "DESCRIBES", "k_target");
        let inactive_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-old");
        write_jsonl(&inactive_dir.join("project.jsonl"), &[&inactive_edge]);

        let manifest = bbox_edge_sidecar::manifest::WorkspaceManifest {
            version: 1,
            project_id: "p1".into(),
            repo_id: None,
            canonical_path: None,
            git_common_dir: None,
            git_worktree_dir: None,
            branch: Some("main".into()),
            head_sha: Some("abc".into()),
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("head-abc".into()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        bbox_edge_sidecar::manifest::WorkspaceManifest::write_to(edges_dir, &manifest).unwrap();

        let mut idx = bbox_edge_sidecar::manifest::ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/head-abc".into()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

        let active_source = EntityRef::Knowledge {
            id: "k_active".into(),
        };
        let stale_source = EntityRef::Knowledge {
            id: "k_stale".into(),
        };
        assert_eq!(
            index.forward_edges(&active_source).len(),
            1,
            "active snapshot edge must load via load_sidecar_edges"
        );
        assert_eq!(
            index.forward_edges(&stale_source).len(),
            0,
            "inactive snapshot edge must NOT load via load_sidecar_edges"
        );
    }

    #[test]
    fn load_sidecar_corrupt_index_falls_back_to_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let mat_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir);
        fs::create_dir_all(&mat_dir).unwrap();
        fs::write(
            bbox_edge_sidecar::manifest::manifest_index_path(edges_dir),
            b"not json{{{",
        )
        .unwrap();

        let legacy_edge = make_explicit_edge_line("k_legacy", "SUPERSEDES", "k_old");
        write_jsonl(&edges_dir.join("p1.jsonl"), &[&legacy_edge]);

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

        let source = EntityRef::Knowledge {
            id: "k_legacy".into(),
        };
        assert_eq!(
            index.forward_edges(&source).len(),
            1,
            "corrupt manifest must fall back to legacy loading via load_sidecar_edges"
        );
    }

    #[test]
    fn load_sidecar_missing_manifest_uses_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let explicit_line = make_explicit_edge_line("k_explicit", "DESCRIBES", "k_target");
        write_jsonl(&edges_dir.join("p1.jsonl"), &[&explicit_line]);

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

        let source = EntityRef::Knowledge {
            id: "k_explicit".into(),
        };
        assert_eq!(
            index.forward_edges(&source).len(),
            1,
            "missing manifest must fall back to legacy loading via load_sidecar_edges"
        );
    }

    #[test]
    fn load_sidecar_stale_manifest_falls_back_to_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let legacy_edge = make_explicit_edge_line("k_stale_test", "DESCRIBES", "k_target");
        write_jsonl(&edges_dir.join("p1.jsonl"), &[&legacy_edge]);

        let mut idx = bbox_edge_sidecar::manifest::ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            bbox_edge_sidecar::manifest::WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: None,
                dirty_overlay: Some("workspace/p1/dirty-overlay/does-not-exist".into()),
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

        let source = EntityRef::Knowledge {
            id: "k_stale_test".into(),
        };
        assert_eq!(
            index.forward_edges(&source).len(),
            1,
            "stale manifest (missing dirty_overlay) must fall back to legacy loading"
        );
    }

    mod cross_phase {
        use super::*;
        use crate::storage_health::{
            GcParams, GcPolicy, SnapshotRetentionPolicy, plan_gc_with_policy, scan_storage_health,
        };
        use bbox_edge_sidecar::manifest::ManifestIndex;
        use bbox_edge_sidecar::snapshot::{
            clean_snapshot_id, snapshot_dir, switch_to_clean_snapshot, switch_to_dirty_overlay,
        };

        fn derived_edge(source: &str, kind: &str, target: &str) -> Edge {
            Edge {
                source: EntityRef::Knowledge { id: source.into() },
                kind: kind.into(),
                target: EntityRef::Knowledge { id: target.into() },
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
            }
        }

        fn setup_branch_snapshot(
            edges_dir: &Path,
            project_id: &str,
            repo_id: &str,
            branch: &str,
            head_sha: &str,
            edges: Vec<Edge>,
        ) {
            let empty: Vec<Edge> = Vec::new();
            switch_to_clean_snapshot(
                edges_dir,
                project_id,
                repo_id,
                Some(branch),
                head_sha,
                edges,
                empty.clone(),
                empty,
            )
            .unwrap();
        }

        fn active_edge_sources(index: &EdgeIndex) -> Vec<String> {
            let mut sources: Vec<String> = index
                .forward
                .keys()
                .filter_map(|e| match e {
                    EntityRef::Knowledge { id } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            sources.sort();
            sources
        }

        fn load_active(edges_dir: &Path) -> EdgeIndex {
            let mut index = EdgeIndex::default();
            let mut seen = HashSet::new();
            index.load_sidecar_edges(edges_dir, None, &mut seen, true);
            index
        }

        #[test]
        fn branch_switch_active_graph_reflects_current_branch() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";

            let branch_a_sha = "aaaa1111bbbb";
            let branch_b_sha = "bbbb2222cccc";

            let edges_a = vec![derived_edge("sym_branch_a", "DESCRIBES", "target_a")];
            let edges_b = vec![derived_edge("sym_branch_b", "DESCRIBES", "target_b")];

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                branch_a_sha,
                edges_a,
            );
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "feature",
                branch_b_sha,
                edges_b,
            );

            let index = load_active(edges_dir);
            let sources = active_edge_sources(&index);
            assert!(
                sources.contains(&"sym_branch_b".to_string()),
                "active graph must have branch B edges: {sources:?}"
            );
            assert!(
                !sources.contains(&"sym_branch_a".to_string()),
                "active graph must NOT have branch A edges: {sources:?}"
            );
        }

        #[test]
        fn branch_reactivate_cached_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";

            let branch_a_sha = "aaaa1111bbbb";
            let branch_b_sha = "bbbb2222cccc";

            let edges_a = vec![derived_edge("sym_branch_a", "DESCRIBES", "target_a")];
            let edges_b = vec![derived_edge("sym_branch_b", "DESCRIBES", "target_b")];

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                branch_a_sha,
                edges_a,
            );
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "feature",
                branch_b_sha,
                edges_b,
            );

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                branch_a_sha,
                vec![derived_edge("sym_branch_a", "DESCRIBES", "target_a")],
            );

            let index = load_active(edges_dir);
            let sources = active_edge_sources(&index);
            assert!(
                sources.contains(&"sym_branch_a".to_string()),
                "switching back to A must reactivate cached A snapshot: {sources:?}"
            );
            assert!(
                !sources.contains(&"sym_branch_b".to_string()),
                "switching back to A must deactivate B snapshot: {sources:?}"
            );

            let snap_a_dir = snapshot_dir(
                edges_dir,
                project_id,
                &clean_snapshot_id(repo_id, project_id, branch_a_sha),
            );
            let snap_b_dir = snapshot_dir(
                edges_dir,
                project_id,
                &clean_snapshot_id(repo_id, project_id, branch_b_sha),
            );
            assert!(snap_a_dir.is_dir(), "branch A snapshot dir must exist");
            assert!(
                snap_b_dir.is_dir(),
                "branch B snapshot dir must be preserved"
            );
        }

        #[test]
        fn dirty_overlay_wins_over_clean_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";
            let head_sha = "aaaa1111bbbb";

            let clean_edges = vec![derived_edge("sym_clean", "DESCRIBES", "target_clean")];
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                head_sha,
                clean_edges,
            );

            let index = load_active(edges_dir);
            assert!(
                active_edge_sources(&index).contains(&"sym_clean".to_string()),
                "clean snapshot must load initially"
            );

            let dirty_edges = vec![derived_edge("sym_dirty", "DESCRIBES", "target_dirty")];
            let empty: Vec<Edge> = Vec::new();
            switch_to_dirty_overlay(
                edges_dir,
                project_id,
                repo_id,
                Some("main"),
                head_sha,
                "fingerprint1",
                dirty_edges,
                empty.clone(),
                empty,
            )
            .unwrap();

            let index = load_active(edges_dir);
            let sources = active_edge_sources(&index);
            assert!(
                sources.contains(&"sym_dirty".to_string()),
                "dirty overlay must win over clean snapshot: {sources:?}"
            );
            assert!(
                !sources.contains(&"sym_clean".to_string()),
                "clean snapshot edges must be suppressed when dirty overlay active: {sources:?}"
            );
        }

        #[test]
        fn gc_retains_active_snapshot_and_dirty_overlay() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";
            let head_sha = "aaaa1111bbbb";

            let clean_edges = vec![derived_edge("sym_clean", "DESCRIBES", "target_clean")];
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                head_sha,
                clean_edges,
            );

            let dirty_edges = vec![derived_edge("sym_dirty", "DESCRIBES", "target_dirty")];
            let empty: Vec<Edge> = Vec::new();
            switch_to_dirty_overlay(
                edges_dir,
                project_id,
                repo_id,
                Some("main"),
                head_sha,
                "fingerprint1",
                dirty_edges,
                empty.clone(),
                empty,
            )
            .unwrap();

            let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
            let policy = GcPolicy {
                materialized_snapshots: SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: 0,
                    keep_recent_per_repo: 0,
                    branch_switch_grace_minutes: 0,
                    max_age_days: None,
                    max_count_per_workspace: None,
                    max_total_bytes_per_workspace: None,
                },
                ..Default::default()
            };

            let candidates = plan_gc_with_policy(
                edges_dir,
                &registered,
                &GcParams {
                    dry_run: true,
                    project_filter: None,
                    prune_backups: true,
                    prune_orphans: false,
                    prune_temps: true,
                    prune_inactive_snapshots: true,
                    max_backup_age_days: None,
                    keep_newest_backup_per_source: 1,
                },
                &policy,
            )
            .unwrap();

            let overlay_path = format!("workspace/{}/dirty-current", project_id);
            let active_snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
            let snap_path = format!("workspace/{}/snapshots/{}", project_id, active_snap_id);

            for candidate in &candidates {
                assert!(
                    !candidate.path.contains(&overlay_path),
                    "dirty overlay must not be a GC candidate: {}",
                    candidate.path
                );
                assert!(
                    !candidate.path.contains(&snap_path),
                    "active snapshot must not be a GC candidate even when overlay wins: {}",
                    candidate.path
                );
            }

            let inactive_candidates: Vec<_> = candidates
                .iter()
                .filter(|c| c.rule == "inactive_snapshot")
                .collect();
            assert!(
                inactive_candidates.is_empty(),
                "active snapshot + dirty overlay must both be protected, no inactive_snapshot candidates"
            );
        }

        #[test]
        fn gc_prunes_inactive_snapshots_when_enabled() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";

            let sha_a = "aaaa1111bbbb";
            let sha_b = "bbbb2222cccc";

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                sha_a,
                vec![derived_edge("sym_a", "DESCRIBES", "t")],
            );
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "feature",
                sha_b,
                vec![derived_edge("sym_b", "DESCRIBES", "t")],
            );

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                sha_a,
                vec![derived_edge("sym_a", "DESCRIBES", "t")],
            );

            let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
            let policy = GcPolicy {
                materialized_snapshots: SnapshotRetentionPolicy {
                    keep_active: true,
                    keep_recent_per_workspace: 0,
                    keep_recent_per_repo: 0,
                    branch_switch_grace_minutes: 0,
                    max_age_days: None,
                    max_count_per_workspace: None,
                    max_total_bytes_per_workspace: None,
                },
                ..Default::default()
            };
            let candidates = plan_gc_with_policy(
                edges_dir,
                &registered,
                &GcParams {
                    dry_run: true,
                    project_filter: None,
                    prune_backups: true,
                    prune_orphans: false,
                    prune_temps: false,
                    prune_inactive_snapshots: true,
                    max_backup_age_days: None,
                    keep_newest_backup_per_source: 1,
                },
                &policy,
            )
            .unwrap();

            let inactive_snap_id = clean_snapshot_id(repo_id, project_id, sha_b);
            let inactive_path = format!("workspace/{}/snapshots/{}", project_id, inactive_snap_id);

            let inactive_candidate = candidates.iter().find(|c| {
                c.path.contains(&inactive_path)
                    && c.rule.starts_with("snapshot_prunable")
                    && c.deletable
            });
            assert!(
                inactive_candidate.is_some(),
                "inactive branch B snapshot must be a GC candidate: {:?}",
                candidates
                    .iter()
                    .filter(|c| c.rule.starts_with("snapshot_"))
                    .map(|c| (&c.rule, &c.path, c.deletable))
                    .collect::<Vec<_>>()
            );
        }

        #[test]
        fn storage_health_reports_active_inactive_post_snapshot() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";

            let sha_a = "aaaa1111bbbb";
            let sha_b = "bbbb2222cccc";

            let big_edge = derived_edge("sym_a", "DESCRIBES", "target_a");

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                sha_a,
                vec![big_edge.clone()],
            );
            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "feature",
                sha_b,
                vec![derived_edge("sym_b", "DESCRIBES", "target_b")],
            );

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                sha_a,
                vec![big_edge],
            );

            let registered: HashSet<String> = [project_id.to_string()].into_iter().collect();
            let report = scan_storage_health(edges_dir, &registered, None, false).unwrap();

            let ms = report.manifest_status.expect("manifest must exist");
            assert!(
                ms.active_materialized_bytes > 0,
                "active bytes must be nonzero: {}",
                ms.active_materialized_bytes
            );
            assert!(
                ms.active_materialized_files > 0,
                "active files must be nonzero: {}",
                ms.active_materialized_files
            );
            assert!(
                ms.inactive_materialized_bytes > 0,
                "inactive bytes must be nonzero (branch B snapshot): {}",
                ms.inactive_materialized_bytes
            );
            assert!(
                ms.inactive_materialized_files > 0,
                "inactive files must be nonzero (branch B snapshot): {}",
                ms.inactive_materialized_files
            );
        }

        #[test]
        fn active_loader_does_not_scan_inactive_snapshots() {
            let dir = tempfile::tempdir().unwrap();
            let edges_dir = dir.path();
            let project_id = "p1";
            let repo_id = "repo_abc";
            let head_sha = "aaaa1111bbbb";

            setup_branch_snapshot(
                edges_dir,
                project_id,
                repo_id,
                "main",
                head_sha,
                vec![Edge {
                    source: EntityRef::Knowledge {
                        id: "sym_active".into(),
                    },
                    kind: "DESCRIBES".into(),
                    target: EntityRef::Knowledge {
                        id: "target_active".into(),
                    },
                    provenance: EdgeProvenance::Derived,
                    confidence: EdgeConfidence::Exact,
                    metadata: BTreeMap::new(),
                }],
            );

            let inactive_base = bbox_edge_sidecar::manifest::materialized_dir(edges_dir)
                .join("workspace")
                .join(project_id)
                .join("snapshots");
            for i in 0..20 {
                let inactive_dir = inactive_base.join(format!("head-inactive-{i}"));
                let fat_content: Vec<String> = (0..100)
                    .map(|j| format!("{{\"fake\":\"padding_{i}_{j}\"}}"))
                    .collect();
                write_jsonl(
                    &inactive_dir.join("project.jsonl"),
                    &fat_content.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                );
            }

            let mat_dir = bbox_edge_sidecar::manifest::materialized_dir(edges_dir);
            let total_jsonl = count_materialized_jsonl_files_recursive(&mat_dir);
            assert!(
                total_jsonl > 20,
                "must have many jsonl files (active + inactive): {total_jsonl}"
            );

            let active_paths = {
                let idx = ManifestIndex::load(edges_dir).unwrap();
                idx.active_materialized_paths(edges_dir)
            };
            assert!(
                active_paths.len() <= 3,
                "active loader must see only the active snapshot files, got {}: {active_paths:?}",
                active_paths.len()
            );

            let mut index = EdgeIndex::default();
            let mut seen = HashSet::new();
            index.load_sidecar_edges(edges_dir, None, &mut seen, true);

            let source = EntityRef::Knowledge {
                id: "sym_active".into(),
            };
            assert_eq!(
                index.forward_edges(&source).len(),
                1,
                "active edge must load"
            );

            for i in 0..20 {
                let fake_source = EntityRef::Knowledge {
                    id: format!("fake_padding_{i}"),
                };
                assert_eq!(
                    index.forward_edges(&fake_source).len(),
                    0,
                    "inactive snapshot content must not be loaded"
                );
            }
        }

        fn count_materialized_jsonl_files_recursive(dir: &Path) -> usize {
            let mut count = 0;
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    if path.is_dir() {
                        count += count_materialized_jsonl_files_recursive(&path);
                    } else if path.extension().is_some_and(|e| e == "jsonl") {
                        count += 1;
                    }
                }
            }
            count
        }
    }

    // -------------------------------------------------------------------------
    // Focused tests for active/historical mode, per-file overlay, and observed
    // -------------------------------------------------------------------------

    fn project_file_edge(
        project_id: &str,
        rel_path_hash: &str,
        kind: &str,
        provenance: EdgeProvenance,
    ) -> Edge {
        let make = |occ: u32| EntityRef::ProjectFile {
            project_id: project_id.into(),
            rel_path_hash: rel_path_hash.into(),
            chunk_hash: format!("{occ:0>64}"),
            occurrence_idx: occ,
        };
        Edge {
            source: make(0),
            kind: kind.into(),
            target: make(1),
            provenance,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        }
    }

    fn observed_edge(session_id: &str, rel_path_hash: &str, project_id: &str) -> Edge {
        Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: session_id.into(),
                line_offset: 0,
                event_idx: 0,
            },
            kind: "EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: project_id.into(),
                rel_path_hash: rel_path_hash.into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn entity_type_counts_active_excludes_transcript_and_bash_call() {
        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();

        // Insert a transcript->project_file EDITED_FILE edge
        index.insert(observed_edge("sess-1", "hash1", "proj1"), &mut seen);
        // Insert a bash_call edge
        index.insert(
            Edge {
                source: EntityRef::Transcript {
                    provider: "claude".into(),
                    session_id: "sess-1".into(),
                    line_offset: 10,
                    event_idx: 0,
                },
                kind: "RAN_BASH".into(),
                target: EntityRef::BashCall {
                    session: "sess-1".into(),
                    turn: 10,
                },
                provenance: EdgeProvenance::Explicit,
                confidence: EdgeConfidence::Exact,
                metadata: BTreeMap::new(),
            },
            &mut seen,
        );
        // Insert a regular knowledge edge
        index.insert(
            exact_edge(
                EntityRef::Knowledge { id: "k1".into() },
                "DESCRIBES",
                EntityRef::Knowledge { id: "k2".into() },
                EdgeProvenance::Explicit,
            ),
            &mut seen,
        );

        let all_counts = index.entity_type_counts();
        assert!(
            all_counts.contains_key("transcript"),
            "entity_type_counts must include transcript"
        );
        assert!(
            all_counts.contains_key("bash_call"),
            "entity_type_counts must include bash_call"
        );

        let active_counts = index.entity_type_counts_active();
        assert!(
            !active_counts.contains_key("transcript"),
            "entity_type_counts_active must exclude transcript"
        );
        assert!(
            !active_counts.contains_key("bash_call"),
            "entity_type_counts_active must exclude bash_call"
        );
        assert!(
            active_counts.contains_key("knowledge"),
            "entity_type_counts_active must still include knowledge"
        );
        assert!(
            active_counts.contains_key("project_file"),
            "entity_type_counts_active must still include project_file"
        );
    }

    #[test]
    fn per_file_overlay_suppresses_covered_snapshot_edges() {
        use bbox_edge_sidecar::manifest::{ManifestIndex, OverlayManifest, WorkspaceIndexEntry};
        use bbox_edge_sidecar::snapshot::{dirty_overlay_dir, snapshot_dir};
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let project_id = "proj_overlay";
        let snap_id = "head-testsha-aabbccdd";

        // Write snapshot: one edge for hash "h1" and one for "h2"
        let snap_dir = snapshot_dir(edges_dir, project_id, snap_id);
        fs::create_dir_all(&snap_dir).unwrap();
        let edge_h1_snap = project_file_edge(project_id, "h1", "IN_FILE", EdgeProvenance::Derived);
        let edge_h2_snap = project_file_edge(project_id, "h2", "IN_FILE", EdgeProvenance::Derived);
        let mut snap_file = fs::File::create(snap_dir.join("project.jsonl")).unwrap();
        for e in &[&edge_h1_snap, &edge_h2_snap] {
            serde_json::to_writer(&mut snap_file, e).unwrap();
            snap_file.write_all(b"\n").unwrap();
        }

        // Write overlay: replacement edge for hash "h1" only
        let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
        fs::create_dir_all(&overlay_dir).unwrap();
        let edge_h1_overlay =
            project_file_edge(project_id, "h1", "DESCRIBES", EdgeProvenance::Derived);
        let mut overlay_file = fs::File::create(overlay_dir.join("project.jsonl")).unwrap();
        serde_json::to_writer(&mut overlay_file, &edge_h1_overlay).unwrap();
        overlay_file.write_all(b"\n").unwrap();
        drop(overlay_file);

        // Write overlay_manifest.json covering only h1
        let covered: std::collections::HashSet<String> = ["h1".to_string()].into();
        OverlayManifest::write_to(&overlay_dir, &covered).unwrap();

        // Write manifest-index
        let snap_rel = format!("workspace/{project_id}/snapshots/{snap_id}");
        let overlay_rel = format!("workspace/{project_id}/dirty-current");
        let manifest_path_rel = format!("workspace/{project_id}/manifest.json");
        // Create a stub workspace manifest so validation passes
        let ws_manifest_path =
            bbox_edge_sidecar::manifest::materialized_dir(edges_dir).join(&manifest_path_rel);
        fs::create_dir_all(ws_manifest_path.parent().unwrap()).unwrap();
        fs::write(&ws_manifest_path, "{}").unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            project_id,
            WorkspaceIndexEntry {
                manifest: manifest_path_rel,
                active_snapshot: Some(snap_rel),
                dirty_overlay: Some(overlay_rel),
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.load_sidecar_edges(edges_dir, None, &mut seen, true);

        // h1 overlay edge (DESCRIBES) must win over snapshot edge (IN_FILE)
        let h1_source = EntityRef::ProjectFile {
            project_id: project_id.into(),
            rel_path_hash: "h1".into(),
            chunk_hash: "0".repeat(64),
            occurrence_idx: 0,
        };
        let overlay_edges = index.forward_edges(&h1_source);
        assert!(
            overlay_edges.iter().any(|e| e.kind == "DESCRIBES"),
            "overlay edge must be loaded for covered hash h1"
        );
        assert!(
            !overlay_edges.iter().any(|e| e.kind == "IN_FILE"),
            "snapshot edge for covered hash h1 must be suppressed"
        );

        // h2 snapshot edge (IN_FILE) must survive — not covered by overlay
        let h2_source = EntityRef::ProjectFile {
            project_id: project_id.into(),
            rel_path_hash: "h2".into(),
            chunk_hash: "0".repeat(64),
            occurrence_idx: 0,
        };
        let snap_edges = index.forward_edges(&h2_source);
        assert!(
            snap_edges.iter().any(|e| e.kind == "IN_FILE"),
            "snapshot edge for uncovered hash h2 must survive"
        );
    }

    #[test]
    fn include_observed_false_skips_observed_lane() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        // Write an explicit edge to edges/explicit/p1.jsonl
        let explicit_dir = edges_dir.join("explicit");
        fs::create_dir_all(&explicit_dir).unwrap();
        let explicit_edge = serde_json::to_string(&Edge {
            source: EntityRef::Knowledge {
                id: "k_explicit".into(),
            },
            kind: "SUPERSEDES".into(),
            target: EntityRef::Knowledge { id: "k_old".into() },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        })
        .unwrap();
        fs::write(explicit_dir.join("p1.jsonl"), format!("{explicit_edge}\n")).unwrap();

        // Write an observed edge to edges/observed/p1.jsonl
        let observed_dir = edges_dir.join("observed");
        fs::create_dir_all(&observed_dir).unwrap();
        let obs_edge = serde_json::to_string(&observed_edge("sess-obs", "hashX", "p1")).unwrap();
        fs::write(observed_dir.join("p1.jsonl"), format!("{obs_edge}\n")).unwrap();

        // Load with include_observed=false
        let mut index_no_obs = EdgeIndex::default();
        let mut seen = HashSet::new();
        index_no_obs.load_sidecar_edges(edges_dir, None, &mut seen, false);

        let explicit_source = EntityRef::Knowledge {
            id: "k_explicit".into(),
        };
        let transcript_source = EntityRef::Transcript {
            provider: "claude".into(),
            session_id: "sess-obs".into(),
            line_offset: 0,
            event_idx: 0,
        };
        assert_eq!(
            index_no_obs.forward_edges(&explicit_source).len(),
            1,
            "explicit edge must load even with include_observed=false"
        );
        assert_eq!(
            index_no_obs.forward_edges(&transcript_source).len(),
            0,
            "observed edge must NOT load when include_observed=false"
        );

        // Load with include_observed=true — observed edge must appear
        let mut index_with_obs = EdgeIndex::default();
        let mut seen2 = HashSet::new();
        index_with_obs.load_sidecar_edges(edges_dir, None, &mut seen2, true);
        assert_eq!(
            index_with_obs.forward_edges(&transcript_source).len(),
            1,
            "observed edge must load when include_observed=true"
        );
    }
}
