use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rmcp::schemars;
use serde::Deserialize;
use serde_json::json;

use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_provenance::{
    GitProvenanceNote, MAX_NOTE_DOCUMENT_BYTES, NoteToolCall, fragment_note, parse_note_document,
    serialize_note, split_note_documents,
};

use super::provenance_plan::note_from_edges;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProvenanceParams {
    #[serde(default)]
    pub project_id: Option<String>,
}

/// One caller-authorized checkout root. Project identity and filesystem
/// authority arrive as separate fields.
#[derive(Debug, Clone)]
pub struct ProvenanceProject {
    pub project_id: String,
    pub project_root: PathBuf,
}

pub type LegacyTargetResolver<'a> =
    dyn Fn(&str, &Path, &Path, Option<(u64, u64)>) -> Result<Option<EntityRef>> + 'a;

pub fn export_provenance(edge_index: &EdgeIndex, projects: &[ProvenanceProject]) -> Result<String> {
    let project_map = project_map(projects);
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
    let mut configured_roots: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for ((project_id, commit), edges) in grouped {
        let Some(project) = project_map.get(&project_id) else {
            continue;
        };
        let root = project.project_root.as_path();
        // Auto-configure notes.mergeStrategy=union once per repo so
        // cross-machine provenance merges union rather than abort.
        if configured_roots.insert(project.project_root.clone()) {
            if bbox_corpus_core::git::ensure_notes_merge_strategy_union(root).is_err() {
                tracing::warn!(
                    "error.checkout_io_failed: could not set provenance note merge strategy"
                );
            }
        }
        let note = note_from_edges(&commit, &edges, edge_index);
        let documents = fragment_note(&note, MAX_NOTE_DOCUMENT_BYTES)?
            .into_iter()
            .map(|part| serialize_note(&part))
            .collect::<Result<Vec<_>>>()?;
        let applied = bbox_provenance::append_note_documents_dedup(
            root, &notes_ref, &commit, &documents,
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "error.checkout_io_failed: provenance note could not be written to the validated checkout"
            )
        })?;
        notes_written += applied.written;
    }

    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "notes_written": notes_written,
        "notes_ref": notes_ref,
    }))?)
}

pub fn import_provenance_to_edges_dir(
    projects: &[ProvenanceProject],
    edges_dir: &Path,
    resolve_legacy_target: &LegacyTargetResolver<'_>,
) -> Result<u64> {
    let notes_ref = bbox_corpus_core::git::notes_ref("provenance");
    let mut edges_imported = 0u64;
    for project in projects {
        let root = project.project_root.as_path();
        let notes = bbox_corpus_core::git::list_notes(root, &notes_ref).map_err(|_| {
            anyhow::anyhow!(
                "error.checkout_io_failed: provenance notes could not be listed from the validated checkout"
            )
        })?;
        for (_note_sha, commit) in notes {
            let Some(raw) = bbox_corpus_core::git::show_note(root, &notes_ref, &commit).map_err(
                |_| {
                    anyhow::anyhow!(
                        "error.checkout_io_failed: provenance note could not be read from the validated checkout"
                    )
                },
            )? else {
                continue;
            };
            for raw_note in split_note_documents(&raw) {
                let Ok(note) = parse_note_document(raw_note) else {
                    continue;
                };
                let edges =
                    edges_from_note(&project.project_id, root, &note, resolve_legacy_target)?;
                edges_imported += bbox_edge_index::edge_index::append_explicit_edges(
                    edges_dir,
                    &project.project_id,
                    &edges,
                )
                .map_err(|_| {
                    anyhow::anyhow!(
                        "error.provenance_store_unavailable: imported provenance edges could not be persisted"
                    )
                })? as u64;
            }
        }
    }
    Ok(edges_imported)
}

fn project_map<'a>(projects: &'a [ProvenanceProject]) -> HashMap<String, &'a ProvenanceProject> {
    projects
        .iter()
        .map(|project| (project.project_id.clone(), project))
        .collect()
}

fn edges_from_note(
    project_id: &str,
    root: &Path,
    note: &GitProvenanceNote,
    resolve_legacy_target: &LegacyTargetResolver<'_>,
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
        let file = call.file.as_deref();
        let target = if note.schema_version >= bbox_provenance::SCHEMA_VERSION_V2 {
            validated_target_for_project(call, project_id)
        } else {
            None
        };
        let target = match target {
            Some(target) => target,
            None => {
                let Some(file) = file else {
                    continue;
                };
                let absolute_path = root.join(file);
                match resolve_legacy_target(
                    project_id,
                    root,
                    &absolute_path,
                    call.byte_range.map(|range| (range[0], range[1])),
                )
                .map_err(|_| {
                    anyhow::anyhow!(
                        "error.checkout_io_failed: provenance target could not be resolved in the validated checkout"
                    )
                })? {
                    Some(target) => target,
                    None => continue,
                }
            }
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.to_string());
        if let Some(file) = file {
            metadata.insert("anchor.file_path".into(), file.to_string());
        }
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

fn validated_target_for_project(call: &NoteToolCall, project_id: &str) -> Option<EntityRef> {
    let target = EntityRef::parse(call.target_ref.as_deref()?).ok()?;
    match &target {
        EntityRef::ProjectFile {
            project_id: target_project_id,
            ..
        }
        | EntityRef::ProjectFileV2 {
            project_id: target_project_id,
            ..
        } if target_project_id == project_id => Some(target),
        _ => None,
    }
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
    use bbox_provenance::ProducedBy;

    #[test]
    fn provenance_note_json_round_trips() {
        let note = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy {
                provider: Some("claude".into()),
                session_ids: vec!["sess".into()],
                brofiles: vec!["brofile:keystone".into()],
                arc_thread_ids: Vec::new(),
                trigger: None,
            },
            vec![NoteToolCall {
                tool: "Edit".into(),
                edge_kind: Some("EDITED_FILE".into()),
                source_ref: Some("transcript:claude:sess:10:0".into()),
                target_ref: Some(format!("project_file:proj1234:path:{}:0", "a".repeat(64))),
                file: Some("src/main.rs".into()),
                byte_range: Some([10, 20]),
                turn: Some(10),
            }],
            Vec::new(),
        );

        let raw = serialize_note(&note).unwrap();
        let parsed = parse_note_document(&raw).unwrap();

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
        assert_eq!(note.schema_version, bbox_provenance::SCHEMA_VERSION_V2);
        assert!(note.tool_calls.iter().all(|call| call.target_ref.is_some()));
    }
}
