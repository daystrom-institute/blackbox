use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::entity_ref::EntityRef;
use crate::index::{EdgeProjectionDoc, TranscriptIndex};
use crate::knowledge::{Knowledge, KnowledgeEdgeKind};
use crate::notes::Notes;
use crate::orchestration::TaskStore;
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
    pub edges_dir: PathBuf,
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
        if stores.include_tantivy_projection {
            if let Ok(docs) = stores.index.edge_projection_docs() {
                index.project_tantivy_edges(&docs, &mut seen);
            }
        } else {
            tracing::debug!("rebuilt EdgeIndex without Tantivy stored-doc projection");
        }
        index.project_sidecar_edges(&stores.edges_dir, &mut seen);

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

    fn project_sidecar_edges(&mut self, edges_dir: &Path, seen: &mut HashSet<Edge>) {
        let Ok(entries) = fs::read_dir(edges_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                tracing::debug!(path = %path.display(), "skipping non-jsonl edge sidecar file");
                continue;
            }
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let reader = std::io::BufReader::new(file);
            for line in reader.lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Edge>(&line) {
                    Ok(edge) => {
                        self.insert_sidecar_edge(edge, seen);
                    }
                    Err(err) => {
                        tracing::warn!(path = %path.display(), error = %err, "failed to parse edge sidecar line");
                    }
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
        index.project_sidecar_edges(dir.path(), &mut seen);

        assert_eq!(index.forward_edges(&source).len(), 1);
        assert_eq!(index.reverse_edges(&target).len(), 1);
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
        index.project_sidecar_edges(dir.path(), &mut seen);

        assert!(index
            .forward_edges(&file)
            .iter()
            .any(|edge| edge.kind == "EDITED_BY_SESSION"
                && edge.target
                    == (EntityRef::Session {
                        provider: "claude".into(),
                        session_id: "sess-1".into(),
                    })));
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
}
