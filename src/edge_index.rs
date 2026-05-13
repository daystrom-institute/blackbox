use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::entity_ref::EntityRef;
use crate::index::{EdgeProjectionDoc, TranscriptIndex};
use crate::knowledge::{Knowledge, KnowledgeEdgeKind};
use crate::notes::Notes;
use crate::orchestration::TaskStore;
use crate::roadmap::Roadmap;
use crate::threads::{EdgeKind, EdgeTarget, Threads};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub provenance: EdgeProvenance,
    pub confidence: EdgeConfidence,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct EdgeIndex {
    forward: HashMap<EntityRef, Vec<Edge>>,
    reverse: HashMap<EntityRef, Vec<Edge>>,
    commit_anchor_index: HashMap<String, Vec<Edge>>,
    session_tool_calls: HashMap<(String, String), Vec<Edge>>,
}

pub struct EdgeStoreRefs<'a> {
    pub index: &'a TranscriptIndex,
    pub knowledge: &'a Knowledge,
    pub threads: &'a Threads,
    pub notes: &'a Notes,
    pub task_store: &'a TaskStore,
    pub roadmap: &'a Roadmap,
    pub edges_dir: PathBuf,
    pub registered_project_ids: Option<HashSet<String>>,
    pub include_tantivy_projection: bool,
}

impl EdgeIndex {
    pub fn rebuild(stores: &EdgeStoreRefs<'_>) -> Self {
        let started = Instant::now();
        let mut index = Self::default();
        let mut seen = HashSet::new();

        index.project_knowledge_edges(stores.knowledge, &mut seen);
        index.project_thread_edges(stores.threads, &mut seen);
        index.project_note_edges(stores.notes, &mut seen);
        index.project_task_edges(stores.task_store, &mut seen);
        index.project_roadmap_edges(stores.roadmap, &mut seen);
        if stores.include_tantivy_projection {
            if let Ok(docs) = stores.index.edge_projection_docs() {
                index.project_tantivy_edges(&docs, &mut seen);
            }
        } else {
            tracing::debug!("rebuilt EdgeIndex without Tantivy stored-doc projection");
        }

        match crate::manifest::try_load_manifest_index(&stores.edges_dir) {
            Ok(manifest_index) => {
                let active_paths = manifest_index.active_materialized_paths(&stores.edges_dir);
                index.load_manifest_active_paths(&active_paths, &mut seen);
                index.load_legacy_explicit_edges(
                    &stores.edges_dir,
                    stores.registered_project_ids.as_ref(),
                    &mut seen,
                );
                tracing::info!(
                    active_paths = active_paths.len(),
                    "loaded edges via manifest-index"
                );
            }
            Err(reason) => {
                if !matches!(
                    reason,
                    crate::manifest::ManifestFallbackReason::MissingNotMigrated
                ) {
                    tracing::warn!(?reason, "manifest-index fallback to legacy sidecar loading");
                }
                index.project_sidecar_edges(
                    &stores.edges_dir,
                    stores.registered_project_ids.as_ref(),
                    &mut seen,
                );
            }
        }

        tracing::info!(
            edges = index.edge_count(),
            sources = index.forward.len(),
            targets = index.reverse.len(),
            include_tantivy_projection = stores.include_tantivy_projection,
            elapsed_ms = started.elapsed().as_millis(),
            "rebuilt EdgeIndex"
        );
        index
    }

    #[allow(dead_code)]
    pub fn forward_edges(&self, source: &EntityRef) -> &[Edge] {
        self.forward.get(source).map(Vec::as_slice).unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub fn reverse_edges(&self, target: &EntityRef) -> &[Edge] {
        self.reverse.get(target).map(Vec::as_slice).unwrap_or(&[])
    }

    #[allow(dead_code)]
    pub fn forward_edges_filtered(&self, source: &EntityRef, kinds: &[&str]) -> Vec<&Edge> {
        self.forward_edges(source)
            .iter()
            .filter(|edge| kinds.iter().any(|kind| *kind == edge.kind))
            .collect()
    }

    #[allow(dead_code)]
    pub fn reverse_edges_filtered(&self, target: &EntityRef, kinds: &[&str]) -> Vec<&Edge> {
        self.reverse_edges(target)
            .iter()
            .filter(|edge| kinds.iter().any(|kind| *kind == edge.kind))
            .collect()
    }

    pub fn edge_count(&self) -> usize {
        self.forward.values().map(Vec::len).sum()
    }

    pub fn known_refs(&self) -> Vec<EntityRef> {
        let mut refs = HashSet::new();
        refs.extend(self.forward.keys().cloned());
        refs.extend(self.reverse.keys().cloned());
        let mut refs = refs.into_iter().collect::<Vec<_>>();
        refs.sort_by_key(|r| r.to_string());
        refs
    }

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

    pub(crate) fn all_edges(&self) -> impl Iterator<Item = &Edge> {
        self.forward.values().flat_map(|edges| edges.iter())
    }

    pub(crate) fn edges_with_anchor_commit(&self, commit_sha: &str) -> Vec<&Edge> {
        self.commit_anchor_index
            .get(commit_sha)
            .map(|edges| edges.iter().collect())
            .unwrap_or_default()
    }

    pub(crate) fn session_tool_call_edges(&self, provider: &str, session_id: &str) -> Vec<&Edge> {
        self.session_tool_calls
            .get(&(provider.to_string(), session_id.to_string()))
            .map(|edges| edges.iter().collect())
            .unwrap_or_default()
    }

    fn insert(&mut self, edge: Edge, seen: &mut HashSet<Edge>) {
        // Dedupe on the logical edge identity (source, kind, target,
        // provenance, confidence) — metadata varies per emission instance
        // (anchor.byte_start, anchor.commit_sha_at_edit, etc.) so leaving it
        // in the dedup key produces an N-way duplicate of the same logical
        // relationship. Symptom: a session that wrote a file 7 times shows
        // 7 identical EDITED_BY_SESSION edges in inspect/find_paths/notable.
        // First emission wins for the metadata-bearing storage.
        let mut key = edge.clone();
        key.metadata.clear();
        if !seen.insert(key) {
            return;
        }
        if edge.kind == "EDITED_FILE" {
            if let Some(commit_sha) = edge.metadata.get("anchor.commit_sha_at_edit") {
                self.commit_anchor_index
                    .entry(commit_sha.clone())
                    .or_default()
                    .push(edge.clone());
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
                    .push(edge.clone());
            }
        }
        self.reverse
            .entry(edge.target.clone())
            .or_default()
            .push(edge.clone());
        self.forward
            .entry(edge.source.clone())
            .or_default()
            .push(edge);
    }

    #[cfg(test)]
    pub(crate) fn from_edges_for_tests(edges: Vec<Edge>) -> Self {
        let mut index = Self::default();
        let mut seen = HashSet::new();
        for edge in edges {
            index.insert(edge, &mut seen);
        }
        index
    }

    fn project_knowledge_edges(&mut self, knowledge: &Knowledge, seen: &mut HashSet<Edge>) {
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

    fn project_thread_edges(&mut self, threads: &Threads, seen: &mut HashSet<Edge>) {
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

    fn project_note_edges(&mut self, notes: &Notes, seen: &mut HashSet<Edge>) {
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

    fn project_task_edges(&mut self, task_store: &TaskStore, seen: &mut HashSet<Edge>) {
        for task in task_store.all_tasks() {
            let inner = task.inner.lock();
            let Some(label) = inner.bro_label.as_ref() else {
                continue;
            };
            if label.contains("::") || inner.session_id == "pending" {
                continue;
            }
            self.insert(
                exact_edge(
                    EntityRef::Session {
                        provider: inner.provider.as_str().to_string(),
                        session_id: inner.session_id.clone(),
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

    fn project_roadmap_edges(&mut self, roadmap: &Roadmap, seen: &mut HashSet<Edge>) {
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
                crate::roadmap::RoadmapEdgeKind::Spawns => "ROADMAP_SPAWNS",
                crate::roadmap::RoadmapEdgeKind::DeferredFrom => "ROADMAP_DEFERRED_FROM",
                crate::roadmap::RoadmapEdgeKind::DesignedIn => "ROADMAP_DESIGNED_IN",
                crate::roadmap::RoadmapEdgeKind::DependsOn => "ROADMAP_DEPENDS_ON",
                crate::roadmap::RoadmapEdgeKind::BlockedBy => "ROADMAP_BLOCKED_BY",
                crate::roadmap::RoadmapEdgeKind::Supersedes => "ROADMAP_SUPERSEDES",
                crate::roadmap::RoadmapEdgeKind::Subsumes => "ROADMAP_SUBSUMES",
                crate::roadmap::RoadmapEdgeKind::RelatedTo => "ROADMAP_RELATED_TO",
            };
            self.insert(
                exact_edge(source, kind, target, EdgeProvenance::Derived),
                seen,
            );
        }
    }

    fn project_tantivy_edges(&mut self, docs: &[EdgeProjectionDoc], seen: &mut HashSet<Edge>) {
        let mut by_file: HashMap<String, Vec<&EdgeProjectionDoc>> = HashMap::new();
        for doc in docs {
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
                            provider: doc.account.clone(),
                            session_id: doc.session_id.clone(),
                        },
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
            } else if doc.doc_type == "project_file" {
                by_file.entry(doc.file_path.clone()).or_default().push(doc);
            }
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
        seen: &mut HashSet<Edge>,
    ) {
        let managed_derived_dir = managed_derived_edges_dir(edges_dir);
        let projects_with_managed = scan_managed_derived_project_ids(&managed_derived_dir);
        self.project_sidecar_edges_in_dir(
            edges_dir,
            registered_project_ids,
            seen,
            &projects_with_managed,
        );
        let Ok(entries) = fs::read_dir(&managed_derived_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            self.project_sidecar_edges_in_dir(&path, registered_project_ids, seen, &HashSet::new());
        }
    }

    fn project_sidecar_edges_in_dir(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<Edge>,
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
        seen: &mut HashSet<Edge>,
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

    fn insert_sidecar_edge(&mut self, edge: Edge, seen: &mut HashSet<Edge>) {
        let derived = derived_tool_projection(&edge);
        self.insert(edge, seen);
        if let Some(edge) = derived {
            self.insert(edge, seen);
        }
    }

    fn load_manifest_active_paths(&mut self, paths: &[PathBuf], seen: &mut HashSet<Edge>) {
        for path in paths {
            self.project_sidecar_edges_file(path, seen, false);
        }
    }

    fn load_legacy_explicit_edges(
        &mut self,
        edges_dir: &Path,
        registered_project_ids: Option<&HashSet<String>>,
        seen: &mut HashSet<Edge>,
    ) {
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
            let skip_derived =
                sidecar_file_stem(&path).is_some_and(|stem| projects_with_managed.contains(stem));
            self.project_sidecar_edges_file(&path, seen, skip_derived);
        }
        for namespace_dir in fs::read_dir(&managed_derived_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
        {
            let ns_path = namespace_dir.path();
            let ns_name = ns_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if ns_name == "project" {
                continue;
            }
            self.project_sidecar_edges_in_dir(
                &ns_path,
                registered_project_ids,
                seen,
                &HashSet::new(),
            );
        }
    }
}

pub(crate) fn edges_dir_from_bro_store(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| store_dir.join("edges"))
}

pub(crate) fn edges_dir_from_projects_path(projects_path: &Path) -> PathBuf {
    projects_path
        .parent()
        .map(|parent| parent.join("edges"))
        .unwrap_or_else(|| PathBuf::from("edges"))
}

fn managed_derived_edges_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("derived")
}

fn sidecar_file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}

fn scan_managed_derived_project_ids(managed_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Ok(namespace_entries) = fs::read_dir(managed_dir) else {
        return ids;
    };
    for ns_entry in namespace_entries.filter_map(Result::ok) {
        let ns_path = ns_entry.path();
        if !ns_path.is_dir() {
            continue;
        }
        let Ok(project_entries) = fs::read_dir(&ns_path) else {
            continue;
        };
        for proj_entry in project_entries.filter_map(Result::ok) {
            let proj_path = proj_entry.path();
            if proj_path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = sidecar_file_stem(&proj_path) {
                ids.insert(stem.to_string());
            }
        }
    }
    ids
}

fn sidecar_project_is_registered(
    path: &Path,
    registered_project_ids: Option<&HashSet<String>>,
) -> bool {
    let Some(registered_project_ids) = registered_project_ids else {
        return true;
    };
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if matches!(stem, "agents") {
        return true;
    }
    registered_project_ids.contains(stem)
}

pub(crate) fn append_project_edges(
    edges_dir: &Path,
    project_id: &str,
    edges: &[crate::chunker::Edge],
) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
        };
        serde_json::to_writer(&mut file, &persisted)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn replace_project_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[crate::chunker::Edge],
) -> Result<()> {
    let dir = managed_derived_edges_dir(edges_dir).join(namespace);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{project_id}.jsonl"));
    if edges.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(());
    }

    let tmp_path = path.with_extension("jsonl.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)?;
    for edge in edges {
        let persisted = Edge {
            source: edge.source.clone(),
            kind: edge.kind.clone(),
            target: edge.target.clone(),
            provenance: edge.provenance,
            confidence: edge.confidence,
            metadata: BTreeMap::new(),
        };
        serde_json::to_writer(&mut file, &persisted)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    drop(file);
    fs::rename(tmp_path, path)?;
    Ok(())
}

pub(crate) fn append_edges(edges_dir: &Path, project_id: &str, edges: &[Edge]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    for edge in edges {
        serde_json::to_writer(&mut file, edge)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

pub(crate) fn append_edges_dedup(
    edges_dir: &Path,
    project_id: &str,
    edges: &[Edge],
) -> Result<usize> {
    if edges.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(edges_dir)?;
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut seen = HashSet::new();
    if let Ok(file) = fs::File::open(&path) {
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(edge) = serde_json::from_str::<Edge>(&line) {
                seen.insert(edge_import_key(&edge));
            }
        }
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut written = 0usize;
    for edge in edges {
        if !seen.insert(edge_import_key(edge)) {
            continue;
        }
        serde_json::to_writer(&mut file, edge)?;
        file.write_all(b"\n")?;
        written += 1;
    }
    Ok(written)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EdgeSidecarCompactionStats {
    pub project_id: String,
    pub applied: bool,
    pub existed: bool,
    pub legacy_path: String,
    pub backup_path: Option<String>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub lines_seen: u64,
    pub retained_lines: u64,
    pub derived_edges_removed: u64,
    pub explicit_edges_retained: u64,
    pub malformed_lines_retained: u64,
    pub blank_lines_dropped: u64,
}

pub(crate) fn compact_legacy_sidecar(
    edges_dir: &Path,
    project_id: &str,
    apply: bool,
) -> Result<EdgeSidecarCompactionStats> {
    let path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut stats = EdgeSidecarCompactionStats {
        project_id: project_id.to_string(),
        applied: false,
        existed: path.exists(),
        legacy_path: path.display().to_string(),
        backup_path: None,
        bytes_before: 0,
        bytes_after: 0,
        lines_seen: 0,
        retained_lines: 0,
        derived_edges_removed: 0,
        explicit_edges_retained: 0,
        malformed_lines_retained: 0,
        blank_lines_dropped: 0,
    };
    if !path.exists() {
        return Ok(stats);
    }

    stats.bytes_before = fs::metadata(&path)?.len();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tmp_path = path.with_file_name(format!(
        "{project_id}.jsonl.compact-{stamp}-{}.tmp",
        std::process::id()
    ));
    let mut writer = if apply {
        Some(BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp_path)?,
        ))
    } else {
        None
    };

    let file = fs::File::open(&path)?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        stats.lines_seen += 1;
        if line.trim().is_empty() {
            stats.blank_lines_dropped += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(&line) {
            Ok(edge) if edge.provenance == EdgeProvenance::Derived => {
                stats.derived_edges_removed += 1;
            }
            Ok(_) => {
                stats.explicit_edges_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            Err(_) => {
                stats.malformed_lines_retained += 1;
                stats.retained_lines += 1;
                stats.bytes_after += line.len() as u64 + 1;
                if let Some(writer) = writer.as_mut() {
                    writer.write_all(line.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
        }
    }

    if !apply || stats.derived_edges_removed == 0 && stats.blank_lines_dropped == 0 {
        if let Some(mut writer) = writer {
            writer.flush()?;
            drop(writer);
            let _ = fs::remove_file(&tmp_path);
        }
        return Ok(stats);
    }

    let backup_path = path.with_file_name(format!("{project_id}.jsonl.bak-{stamp}"));
    if let Some(mut writer) = writer {
        writer.flush()?;
        writer.get_ref().sync_all()?;
    } else {
        anyhow::bail!("internal error: compaction apply requested without writer");
    }
    fs::rename(&path, &backup_path)?;
    match fs::rename(&tmp_path, &path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::rename(&backup_path, &path);
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
    }
    stats.applied = true;
    stats.backup_path = Some(backup_path.display().to_string());
    Ok(stats)
}

fn edge_import_key(edge: &Edge) -> String {
    let mut hasher = Sha256::new();
    hasher.update(edge.source.to_string());
    hasher.update(b"\0");
    hasher.update(&edge.kind);
    hasher.update(b"\0");
    hasher.update(edge.target.to_string());
    hasher.update(b"\0");
    if let Some(commit) = edge.metadata.get("anchor.commit_sha_at_edit") {
        hasher.update(commit);
    }
    hex::encode(hasher.finalize())
}

fn derived_tool_projection(edge: &Edge) -> Option<Edge> {
    if edge.kind != "EDITED_FILE" {
        return None;
    }
    let EntityRef::Transcript {
        provider,
        session_id,
        ..
    } = &edge.source
    else {
        return None;
    };
    Some(Edge {
        source: edge.target.clone(),
        kind: "EDITED_BY_SESSION".to_string(),
        target: EntityRef::Session {
            provider: provider.clone(),
            session_id: session_id.clone(),
        },
        provenance: EdgeProvenance::Derived,
        confidence: EdgeConfidence::Exact,
        metadata: edge.metadata.clone(),
    })
}

fn exact_edge(
    source: EntityRef,
    kind: &str,
    target: EntityRef,
    provenance: EdgeProvenance,
) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance,
        confidence: EdgeConfidence::Exact,
        metadata: BTreeMap::new(),
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

fn line_provenance_is_derived(line: &str) -> bool {
    let Some(pos) = line.find("\"provenance\"") else {
        return false;
    };
    let rest = &line[pos + "\"provenance\"".len()..];
    let rest = rest.trim_start();
    if !rest.starts_with(':') {
        return false;
    }
    let after_colon = rest[1..].trim_start();
    after_colon.starts_with("\"derived\"")
}

// ---------------------------------------------------------------------------
// Phase 2: Lifecycle-specific write APIs
// ---------------------------------------------------------------------------
//
// Caller audit (recorded here so it does not rot):
//
//   materialized  = computed current workspace/repo view (Derived provenance)
//   observed      = event/provenance history, usually Tool provenance (Explicit)
//   explicit      = user/agent-authored durable fact (Explicit)
//   global        = non-project graph support (Explicit)
//
// append_project_edges callers (legacy append path):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges callers (full Edge with metadata):
//   (none — all production callers moved to lifecycle APIs)
//
// append_edges_dedup callers:
//   (none — all production callers moved to lifecycle APIs)
//
// replace_project_edges callers (managed derived replacement):
//   (none directly — wrapped by lifecycle APIs below)
//
// Lifecycle API routing:
//   project_files.rs  → replace_materialized_edges_incremental ("project")
//   git_history.rs    → replace_materialized_edges (full) or merge_materialized_edges (incremental) ("git")
//   tool_edges.rs     → append_observed_edges
//   provenance.rs     → append_explicit_edges
//   routes.rs         → append_explicit_edges (global agents.jsonl)
//   workflow/ops.rs   → append_explicit_edges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeLane {
    Explicit,
    Observed,
}

pub(crate) fn append_explicit_edges(
    edges_dir: &Path,
    project_id: &str,
    edges: &[Edge],
) -> Result<usize> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_explicit_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges_dedup(edges_dir, project_id, edges)
}

pub(crate) fn append_observed_edges(
    edges_dir: &Path,
    project_id: &str,
    edges: &[Edge],
) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance != EdgeProvenance::Derived,
            "append_observed_edges: rejected Derived edge kind={} source={:?}",
            e.kind,
            e.source,
        );
    }
    append_edges(edges_dir, project_id, edges)
}

pub(crate) fn replace_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    edges: &[crate::chunker::Edge],
) -> Result<()> {
    for e in edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    replace_project_edges(edges_dir, namespace, project_id, edges)
}

pub(crate) fn read_managed_derived_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
) -> Result<Vec<Edge>> {
    let path = managed_derived_edges_dir(edges_dir)
        .join(namespace)
        .join(format!("{project_id}.jsonl"));
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let reader = std::io::BufReader::new(file);
    let mut edges = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
            edges.push(edge);
        }
    }
    Ok(edges)
}

pub(crate) fn rel_path_hashes_of(edges: &[crate::chunker::Edge]) -> HashSet<String> {
    let mut hashes = HashSet::new();
    for e in edges {
        if let EntityRef::ProjectFile { rel_path_hash, .. } = &e.source {
            hashes.insert(rel_path_hash.clone());
        }
        if let EntityRef::ProjectFile { rel_path_hash, .. } = &e.target {
            hashes.insert(rel_path_hash.clone());
        }
    }
    hashes
}

fn edge_touches_any_path_hash(edge: &Edge, stale_hashes: &HashSet<String>) -> bool {
    match (&edge.source, &edge.target) {
        (EntityRef::ProjectFile { rel_path_hash, .. }, _)
        | (_, EntityRef::ProjectFile { rel_path_hash, .. }) => stale_hashes.contains(rel_path_hash),
        _ => false,
    }
}

pub(crate) fn replace_materialized_edges_incremental(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[crate::chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "replace_materialized_edges_incremental: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let stale_hashes = rel_path_hashes_of(new_edges);
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let preserved: Vec<crate::chunker::Edge> = existing
        .into_iter()
        .filter(|e| !edge_touches_any_path_hash(e, &stale_hashes))
        .map(|e| crate::chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    let mut merged = preserved;
    merged.extend_from_slice(new_edges);
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

pub(crate) fn merge_materialized_edges(
    edges_dir: &Path,
    namespace: &str,
    project_id: &str,
    new_edges: &[crate::chunker::Edge],
) -> Result<()> {
    for e in new_edges {
        debug_assert!(
            e.provenance == EdgeProvenance::Derived,
            "merge_materialized_edges: rejected non-Derived edge kind={} provenance={:?}",
            e.kind,
            e.provenance,
        );
    }
    if new_edges.is_empty() {
        return Ok(());
    }
    let existing = read_managed_derived_edges(edges_dir, namespace, project_id)?;
    let mut seen: HashSet<String> = existing.iter().map(edge_import_key).collect();
    let mut merged: Vec<crate::chunker::Edge> = existing
        .into_iter()
        .map(|e| crate::chunker::Edge {
            source: e.source,
            kind: e.kind,
            target: e.target,
            provenance: e.provenance,
            confidence: e.confidence,
        })
        .collect();
    for e in new_edges {
        let key = edge_import_key(&Edge {
            source: e.source.clone(),
            kind: e.kind.clone(),
            target: e.target.clone(),
            provenance: e.provenance,
            confidence: e.confidence,
            metadata: BTreeMap::new(),
        });
        if seen.insert(key) {
            merged.push(e.clone());
        }
    }
    replace_project_edges(edges_dir, namespace, project_id, &merged)
}

// ---------------------------------------------------------------------------
// Phase 2: Legacy edge extraction dry-run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyExtractionPlan {
    pub project_id: String,
    pub legacy_path: String,
    pub total_lines: u64,
    pub derived_lines: u64,
    pub tool_lines: u64,
    pub explicit_lines: u64,
    pub malformed_lines: u64,
    pub blank_lines: u64,
    pub managed_replacement_exists: bool,
    pub extractable: bool,
}

pub fn plan_legacy_edge_extraction(
    edges_dir: &Path,
    project_id: &str,
) -> Result<LegacyExtractionPlan> {
    let legacy_path = edges_dir.join(format!("{project_id}.jsonl"));
    let mut plan = LegacyExtractionPlan {
        project_id: project_id.to_string(),
        legacy_path: legacy_path.display().to_string(),
        ..Default::default()
    };

    let managed = managed_derived_edges_dir(edges_dir);
    plan.managed_replacement_exists = managed
        .join("project")
        .join(format!("{project_id}.jsonl"))
        .exists()
        || managed
            .join("git")
            .join(format!("{project_id}.jsonl"))
            .exists();

    let file = match fs::File::open(&legacy_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(plan),
        Err(e) => return Err(e.into()),
    };

    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        plan.total_lines += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            plan.blank_lines += 1;
            continue;
        }
        match serde_json::from_str::<Edge>(trimmed) {
            Ok(edge) => match edge.provenance {
                EdgeProvenance::Derived => plan.derived_lines += 1,
                EdgeProvenance::Explicit => {
                    let is_tool = edge.kind == "READ_FILE"
                        || edge.kind == "EDITED_FILE"
                        || edge.kind == "RAN_BASH";
                    if is_tool {
                        plan.tool_lines += 1;
                    } else {
                        plan.explicit_lines += 1;
                    }
                }
                EdgeProvenance::Implicit => plan.explicit_lines += 1,
            },
            Err(_) => plan.malformed_lines += 1,
        }
    }

    plan.extractable = plan.managed_replacement_exists && plan.derived_lines > 0;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn knowledge_links_project_authored_edges() {
        use crate::knowledge::{
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
        use crate::threads::{ThreadParams, Threads};

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
        let edge = crate::chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[edge.clone(), edge]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen);

        assert_eq!(index.forward_edges(&source).len(), 1);
        assert_eq!(index.reverse_edges(&target).len(), 1);
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
            &[crate::chunker::Edge {
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
            &[crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen);

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
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen);

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
            &[crate::chunker::Edge {
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
            &[crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), Some(&registered), &mut seen);

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
        let first = crate::chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: first_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        let second = crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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
        let legacy = crate::chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: legacy_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[legacy]).unwrap();
        let managed = crate::chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: managed_target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        replace_project_edges(dir.path(), "project", "proj1234", &[managed]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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
        let legacy_derived = crate::chunker::Edge {
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

        let managed = crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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
        let derived = crate::chunker::Edge {
            source: source.clone(),
            kind: "NEXT_SECTION".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        };
        append_project_edges(dir.path(), "proj1234", &[derived]).unwrap();

        let mut index = EdgeIndex::default();
        let mut seen = HashSet::new();
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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
        let legacy_derived = crate::chunker::Edge {
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

        let managed = crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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
        let derived = crate::chunker::Edge {
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
        index.project_sidecar_edges(dir.path(), None, &mut seen);

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

    fn derived_chunker_edge(kind: &str) -> crate::chunker::Edge {
        crate::chunker::Edge {
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
        let n = append_explicit_edges(dir.path(), "p1", &[e.clone()]).unwrap();
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
        append_project_edges(dir.path(), "p1", &[derived.clone()]).unwrap();

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
            replace_materialized_edges(dir.path(), "project", "p1", &[edge.clone()]).unwrap();
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

        let file_a = crate::chunker::Edge {
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
        let file_b = crate::chunker::Edge {
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

        let file_a_updated = crate::chunker::Edge {
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
        let edge = crate::chunker::Edge {
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

        replace_materialized_edges_incremental(dir.path(), "project", "p1", &[edge.clone()])
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

        let old_commit_edge = crate::chunker::Edge {
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
        let new_commit_edge = crate::chunker::Edge {
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

        replace_materialized_edges(dir.path(), "git", "p1", &[old_commit_edge.clone()]).unwrap();
        let after_full = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
        assert_eq!(after_full.len(), 1);

        merge_materialized_edges(dir.path(), "git", "p1", &[new_commit_edge.clone()]).unwrap();
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
        let edge = crate::chunker::Edge {
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

        merge_materialized_edges(dir.path(), "git", "p1", &[edge.clone()]).unwrap();
        merge_materialized_edges(dir.path(), "git", "p1", &[edge]).unwrap();

        let after = read_managed_derived_edges(dir.path(), "git", "p1").unwrap();
        assert_eq!(after.len(), 1, "re-merge with same edge must not duplicate");
    }
}
