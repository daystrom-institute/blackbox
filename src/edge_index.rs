use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::chunker::{EdgeConfidence, EdgeProvenance};
use crate::entity_ref::EntityRef;
use crate::index::{EdgeProjectionDoc, TranscriptIndex};
use crate::knowledge::Knowledge;
use crate::notes::Notes;
use crate::orchestration::TaskStore;
use crate::threads::Threads;

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
}

pub struct EdgeStoreRefs<'a> {
    pub index: &'a TranscriptIndex,
    pub knowledge: &'a Knowledge,
    pub threads: &'a Threads,
    pub notes: &'a Notes,
    pub task_store: &'a TaskStore,
    pub edges_dir: PathBuf,
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
        if let Ok(docs) = stores.index.edge_projection_docs() {
            index.project_tantivy_edges(&docs, &mut seen);
        }
        index.project_sidecar_edges(&stores.edges_dir, &mut seen);

        tracing::info!(
            edges = index.edge_count(),
            sources = index.forward.len(),
            targets = index.reverse.len(),
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

    fn insert(&mut self, edge: Edge, seen: &mut HashSet<Edge>) {
        if !seen.insert(edge.clone()) {
            return;
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
        }
    }

    fn project_thread_edges(&mut self, threads: &Threads, seen: &mut HashSet<Edge>) {
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
            for pair in chunks.windows(2) {
                let Some(left) = pair[0]
                    .entity_id
                    .as_deref()
                    .and_then(|entity| EntityRef::parse(entity).ok())
                else {
                    continue;
                };
                let Some(right) = pair[1]
                    .entity_id
                    .as_deref()
                    .and_then(|entity| EntityRef::parse(entity).ok())
                else {
                    continue;
                };
                self.insert(
                    exact_edge(
                        left.clone(),
                        "NEXT_CHUNK",
                        right.clone(),
                        EdgeProvenance::Derived,
                    ),
                    seen,
                );
                self.insert(
                    exact_edge(right, "PREV_CHUNK", left, EdgeProvenance::Derived),
                    seen,
                );
            }
        }
    }

    fn project_sidecar_edges(&mut self, edges_dir: &Path, seen: &mut HashSet<Edge>) {
        let Ok(entries) = fs::read_dir(edges_dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
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
                    Ok(edge) => self.insert(edge, seen),
                    Err(err) => {
                        tracing::warn!(path = %path.display(), error = %err, "failed to parse edge sidecar line");
                    }
                }
            }
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
}
