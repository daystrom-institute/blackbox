use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_indexing::projects::ProjectRecord;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProvenanceParams {
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GitProvenanceNote {
    commit: String,
    produced_by: ProducedBy,
    tool_calls: Vec<NoteToolCall>,
    #[serde(default)]
    knowledge_writes: Vec<KnowledgeWrite>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ProducedBy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    brofiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    arc_thread_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trigger: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NoteToolCall {
    tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    edge_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    byte_range: Option<[u64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct KnowledgeWrite {
    id: String,
    kind: String,
}

pub fn export_provenance(
    p: &ProvenanceParams,
    edge_index: &EdgeIndex,
    projects: &[ProjectRecord],
) -> Result<String> {
    let project_map = project_map(projects, p.project_id.as_deref());
    let mut grouped = BTreeMap::<(String, String), Vec<&Edge>>::new();
    for edge in edge_index.all_edges() {
        if !matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE") {
            continue;
        }
        let Some(project_id) = edge.metadata.get("anchor.project_id") else {
            continue;
        };
        if !project_map.contains_key(project_id) {
            continue;
        }
        let Some(commit) = edge.metadata.get("anchor.commit_sha_at_edit") else {
            continue;
        };
        grouped
            .entry((project_id.clone(), commit.clone()))
            .or_default()
            .push(edge);
    }

    let notes_ref = bbox_corpus_core::git::notes_ref("provenance");
    let mut notes_written = 0u64;
    // Track which roots we've already configured to avoid redundant git calls.
    let mut configured_roots: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ((project_id, commit), edges) in grouped {
        let Some(project) = project_map.get(&project_id) else {
            continue;
        };
        let root = Path::new(&project.canonical_path);
        // Auto-configure notes.mergeStrategy=union once per repo so
        // cross-machine provenance merges union rather than abort.
        if configured_roots.insert(project.canonical_path.clone()) {
            if let Err(e) = bbox_corpus_core::git::ensure_notes_merge_strategy_union(root) {
                tracing::warn!(
                    path = %root.display(),
                    "could not set notes.mergeStrategy union: {e:#}"
                );
            }
        }
        let note = note_from_edges(&commit, &edges, edge_index);
        let body = serde_json::to_string_pretty(&note)? + "\n";
        bbox_corpus_core::git::write_note(root, &notes_ref, &commit, &body)?;
        notes_written += 1;
    }

    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "notes_written": notes_written,
        "notes_ref": notes_ref,
    }))?)
}

pub fn import_provenance_to_edges_dir(
    p: &ProvenanceParams,
    projects: &[ProjectRecord],
    edges_dir: &Path,
) -> Result<u64> {
    let project_map = project_map(projects, p.project_id.as_deref());
    let notes_ref = bbox_corpus_core::git::notes_ref("provenance");
    let mut edges_imported = 0u64;
    for project in project_map.values() {
        let root = Path::new(&project.canonical_path);
        for (_note_sha, commit) in bbox_corpus_core::git::list_notes(root, &notes_ref)? {
            let Some(raw) = bbox_corpus_core::git::show_note(root, &notes_ref, &commit)? else {
                continue;
            };
            for raw_note in split_note_documents(&raw) {
                let Ok(note) = serde_json::from_str::<GitProvenanceNote>(raw_note) else {
                    continue;
                };
                let edges = edges_from_note(project, root, &note)?;
                edges_imported += bbox_edge_index::edge_index::append_explicit_edges(
                    edges_dir,
                    &project.project_id,
                    &edges,
                )? as u64;
            }
        }
    }
    Ok(edges_imported)
}

fn split_note_documents(raw: &str) -> Vec<&str> {
    raw.split(bbox_corpus_core::git::NOTE_DOCUMENT_SEPARATOR)
        .map(str::trim)
        .filter(|doc| !doc.is_empty())
        .collect()
}

fn project_map<'a>(
    projects: &'a [ProjectRecord],
    project_id: Option<&str>,
) -> HashMap<String, &'a ProjectRecord> {
    projects
        .iter()
        .filter(|project| project_id.is_none_or(|id| id == project.project_id))
        .map(|project| (project.project_id.clone(), project))
        .collect()
}

fn note_from_edges(commit: &str, edges: &[&Edge], edge_index: &EdgeIndex) -> GitProvenanceNote {
    let produced_by = produced_by_from_edges(edges, edge_index);
    GitProvenanceNote {
        commit: commit.to_string(),
        produced_by,
        tool_calls: edges.iter().map(|edge| tool_call_from_edge(edge)).collect(),
        knowledge_writes: Vec::new(),
    }
}

fn produced_by_from_edges(edges: &[&Edge], edge_index: &EdgeIndex) -> ProducedBy {
    let mut providers = BTreeSet::new();
    let mut session_ids = BTreeSet::new();
    let mut brofiles = BTreeSet::new();
    let mut arc_thread_ids = BTreeSet::new();
    for edge in edges {
        let EntityRef::Transcript {
            provider,
            session_id,
            ..
        } = &edge.source
        else {
            continue;
        };
        providers.insert(provider.clone());
        session_ids.insert(session_id.clone());
        let session_ref = EntityRef::Session {
            provider: provider.clone(),
            session_id: session_id.clone(),
        };
        brofiles.extend(
            edge_index
                .forward_edges(&session_ref)
                .iter()
                .filter(|edge| edge.kind == "SESSION_USED_BROFILE")
                .map(|edge| edge.target.to_string()),
        );
        arc_thread_ids.extend(
            edge_index
                .reverse_edges(&session_ref)
                .iter()
                .filter(|edge| edge.kind == "THREAD_HAS_SESSION")
                .map(|edge| edge.source.to_string()),
        );
    }
    ProducedBy {
        provider: providers.into_iter().next(),
        session_ids: session_ids.into_iter().collect(),
        brofiles: brofiles.into_iter().collect(),
        arc_thread_ids: arc_thread_ids.into_iter().collect(),
        trigger: None,
    }
}

fn tool_call_from_edge(edge: &Edge) -> NoteToolCall {
    let file = edge.metadata.get("anchor.file_path").cloned();
    let tool = edge
        .metadata
        .get("tool.name")
        .cloned()
        .unwrap_or_else(|| edge.kind.clone());
    NoteToolCall {
        tool,
        edge_kind: Some(edge.kind.clone()),
        source_ref: Some(edge.source.to_string()),
        file,
        byte_range: byte_range_from_metadata(&edge.metadata),
        turn: transcript_turn(&edge.source),
    }
}

fn byte_range_from_metadata(metadata: &BTreeMap<String, String>) -> Option<[u64; 2]> {
    Some([
        metadata.get("anchor.byte_start")?.parse().ok()?,
        metadata.get("anchor.byte_end")?.parse().ok()?,
    ])
}

fn transcript_turn(source: &EntityRef) -> Option<u32> {
    let EntityRef::Transcript {
        line_offset,
        event_idx,
        ..
    } = source
    else {
        return None;
    };
    u32::try_from(*line_offset)
        .ok()
        .map(|line| line.saturating_add(*event_idx))
}

fn edges_from_note(
    project: &ProjectRecord,
    root: &Path,
    note: &GitProvenanceNote,
) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    for call in &note.tool_calls {
        let Some(edge_kind) = edge_kind_for_call(call) else {
            continue;
        };
        let Some(source_ref) = call.source_ref.as_deref() else {
            continue;
        };
        let Ok(source) = EntityRef::parse(source_ref) else {
            continue;
        };
        let Some(file) = call.file.as_deref() else {
            continue;
        };
        let absolute_path = root.join(file);
        let target = match bbox_indexing::index::resolve_current_project_chunk_entity(
            project,
            root,
            &absolute_path,
            call.byte_range.map(|range| (range[0], range[1])),
        )? {
            Some(target) => target,
            None => continue,
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project.project_id.clone());
        metadata.insert("anchor.file_path".into(), file.to_string());
        metadata.insert("anchor.commit_sha_at_edit".into(), note.commit.clone());
        metadata.insert("tool.name".into(), call.tool.clone());
        if let Some([start, end]) = call.byte_range {
            metadata.insert("anchor.byte_start".into(), start.to_string());
            metadata.insert("anchor.byte_end".into(), end.to_string());
        }
        edges.push(Edge {
            source,
            kind: edge_kind.to_string(),
            target,
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata,
        });
    }
    Ok(edges)
}

fn edge_kind_for_call(call: &NoteToolCall) -> Option<&str> {
    if let Some(kind) = call.edge_kind.as_deref() {
        return Some(kind);
    }
    match call.tool.as_str() {
        "Read" | "read" => Some("READ_FILE"),
        "Edit" | "edit" | "Write" | "write" => Some("EDITED_FILE"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_note_json_round_trips() {
        let note = GitProvenanceNote {
            commit: "abc123".into(),
            produced_by: ProducedBy {
                provider: Some("claude".into()),
                session_ids: vec!["sess".into()],
                brofiles: vec!["brofile:keystone".into()],
                arc_thread_ids: Vec::new(),
                trigger: None,
            },
            tool_calls: vec![NoteToolCall {
                tool: "Edit".into(),
                edge_kind: Some("EDITED_FILE".into()),
                source_ref: Some("transcript:claude:sess:10:0".into()),
                file: Some("src/main.rs".into()),
                byte_range: Some([10, 20]),
                turn: Some(10),
            }],
            knowledge_writes: Vec::new(),
        };

        let raw = serde_json::to_string(&note).unwrap();
        let parsed: GitProvenanceNote = serde_json::from_str(&raw).unwrap();

        assert_eq!(parsed, note);
    }

    #[test]
    fn split_note_documents_accepts_appended_git_notes() {
        let raw = format!(
            "{{\"commit\":\"a\"}}\n{}\n{{\"commit\":\"b\"}}\n",
            bbox_corpus_core::git::NOTE_DOCUMENT_SEPARATOR
        );

        assert_eq!(
            split_note_documents(&raw),
            vec!["{\"commit\":\"a\"}", "{\"commit\":\"b\"}"]
        );
    }

    #[test]
    fn note_from_edges_aggregates_distinct_sessions() {
        let target = EntityRef::ProjectFile {
            project_id: "proj1234".into(),
            rel_path_hash: "path".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let edge_a = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-a".into(),
                line_offset: 10,
                event_idx: 0,
            },
            kind: "EDITED_FILE".into(),
            target: target.clone(),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        };
        let edge_b = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "sess-b".into(),
                line_offset: 11,
                event_idx: 0,
            },
            kind: "READ_FILE".into(),
            target,
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
        };
        let edge_index = EdgeIndex::from_edges_for_tests(vec![edge_a.clone(), edge_b.clone()]);

        let note = note_from_edges("abc123", &[&edge_a, &edge_b], &edge_index);

        assert_eq!(note.produced_by.session_ids, vec!["sess-a", "sess-b"]);
    }
}
