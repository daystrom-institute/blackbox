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

pub type PinnedLegacyTargetResolver<'a> =
    dyn Fn(&str, Option<(u64, u64)>) -> Result<Option<EntityRef>> + 'a;
pub type PinnedTargetMembership<'a> = dyn Fn(&EntityRef) -> Result<bool> + 'a;

/// Checkout-derived provenance edges prepared entirely in memory. Callers
/// must revalidate every lease used to build this value before publishing it
/// to the durable edge sidecars.
#[derive(Debug, Default)]
pub struct PreparedProvenanceImport {
    edges_by_project: BTreeMap<String, Vec<Edge>>,
}

impl PreparedProvenanceImport {
    pub fn edge_count(&self) -> u64 {
        self.edges_by_project
            .values()
            .map(|edges| edges.len() as u64)
            .sum()
    }

    pub fn ordered_import_keys(&self) -> Vec<String> {
        let mut keys = self
            .edges_by_project
            .values()
            .flatten()
            .map(bbox_edge_index::edge_index::edge_import_key)
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn encoded_edge_bytes(&self) -> Result<u64> {
        self.edges_by_project
            .values()
            .flatten()
            .try_fold(0_u64, |total, edge| {
                total
                    .checked_add(serde_json::to_vec(edge)?.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("prepared provenance edge size overflow"))
            })
    }

    pub fn merge(&mut self, other: Self) {
        for (project_id, edges) in other.edges_by_project {
            self.edges_by_project
                .entry(project_id)
                .or_default()
                .extend(edges);
        }
    }
}

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

    let notes_ref = bbox_corpus_core::git::notes_ref("provenance")?;
    // Prepare every document before the first checkout mutation. A malformed
    // later note must not leave earlier targets applied without a receipt.
    let mut prepared = Vec::with_capacity(grouped.len());
    for ((project_id, commit), edges) in grouped {
        let Some(project) = project_map.get(&project_id) else {
            continue;
        };
        anyhow::ensure!(
            project_id.len() <= 256 && commit.len() <= 128,
            "error.provenance_export_invalid_target: oversized target identity; no checkout mutation was attempted"
        );
        let note = note_from_edges(&commit, &edges, edge_index);
        let documents = fragment_note(&note, MAX_NOTE_DOCUMENT_BYTES)
            .map_err(anyhow::Error::from)
            .and_then(|parts| parts.iter().map(serialize_note).collect::<Result<Vec<_>>>())
            .map_err(|_| anyhow::anyhow!(
                "error.provenance_export_prepare_failed: documents could not be prepared; no checkout mutation was attempted"
            ))?;
        prepared.push(PreparedExportTarget {
            project_id,
            commit,
            root: project.project_root.clone(),
            documents,
        });
    }
    let mut configured_roots = std::collections::HashSet::new();
    export_prepared_targets(&prepared, &notes_ref, |target| {
        if configured_roots.insert(target.root.clone())
            && bbox_corpus_core::git::ensure_notes_merge_strategy_union(&target.root).is_err()
        {
            tracing::warn!(
                "error.checkout_io_failed: could not set provenance note merge strategy"
            );
        }
        bbox_provenance::append_note_documents_dedup(
            &target.root,
            &notes_ref,
            &target.commit,
            &target.documents,
        )
        .map(|outcome| outcome.written)
    })
}

struct PreparedExportTarget {
    project_id: String,
    commit: String,
    root: PathBuf,
    documents: Vec<String>,
}

fn export_prepared_targets(
    targets: &[PreparedExportTarget],
    notes_ref: &str,
    mut write: impl FnMut(&PreparedExportTarget) -> Result<u64>,
) -> Result<String> {
    let mut notes_written = 0u64;
    for (index, target) in targets.iter().enumerate() {
        match write(target) {
            Ok(written) => notes_written = notes_written.saturating_add(written),
            Err(_) => {
                // The writer may append some fragments before failing. Only
                // previous complete targets contribute to the known count.
                // Do not project raw process errors or local checkout paths.
                let failed_target = json!({
                    "index": index, "project_id": target.project_id, "commit": target.commit,
                    "may_have_written": true,
                });
                return Ok(serde_json::to_string_pretty(&json!({
                    "status": "partial",
                    "error": "error.checkout_io_failed",
                    "message": "Provenance export stopped after a note-write failure. Earlier completed targets remain applied; the failed target may contain appended fragments.",
                    "notes_written": notes_written,
                    "notes_written_scope": "completed_targets_only",
                    "completed_targets": index,
                    "total_targets": targets.len(),
                    "unattempted_targets": targets.len() - index - 1,
                    "failed_target": failed_target,
                    "notes_ref": notes_ref,
                }))?);
            }
        }
    }
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok", "notes_written": notes_written, "notes_ref": notes_ref,
        "completed_targets": targets.len(), "total_targets": targets.len(),
    }))?)
}

/// Read and resolve Git-note provenance without mutating the edge store.
pub fn prepare_provenance_import(
    projects: &[ProvenanceProject],
    resolve_legacy_target: &LegacyTargetResolver<'_>,
) -> Result<PreparedProvenanceImport> {
    let notes_ref = bbox_corpus_core::git::notes_ref("provenance")?;
    let mut prepared = PreparedProvenanceImport::default();
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
                let Some(note) = parse_note_for_target(raw_note, &commit) else {
                    continue;
                };
                let edges =
                    edges_from_note(&project.project_id, root, &note, resolve_legacy_target)?;
                prepared
                    .edges_by_project
                    .entry(project.project_id.clone())
                    .or_default()
                    .extend(edges);
            }
        }
    }
    Ok(prepared)
}

/// Parse and resolve one authenticated producer import against a caller-pinned
/// corpus generation. V1 documents use the path-free relative-path resolver;
/// V2 documents must carry a valid project-file target that is a member of
/// that exact generation. Invalid typed targets fail the whole generation
/// instead of falling back to the legacy path lane.
pub fn prepare_authenticated_provenance_import(
    project_id: &str,
    import_generation_id: &str,
    documents: &[(String, String, String)],
    resolve_legacy_target: &PinnedLegacyTargetResolver<'_>,
    target_is_member: &PinnedTargetMembership<'_>,
) -> Result<PreparedProvenanceImport> {
    if project_id.trim().is_empty() {
        anyhow::bail!("authenticated provenance import has no project id");
    }
    let mut prepared = PreparedProvenanceImport::default();
    if import_generation_id.trim().is_empty() {
        anyhow::bail!("authenticated provenance import has no generation id");
    }
    for (note_commit, document_sha256, document) in documents {
        let note = parse_note_document(document)
            .map_err(|error| anyhow::anyhow!("invalid provenance document: {error}"))?;
        if &note.commit != note_commit {
            anyhow::bail!("provenance document commit does not match its manifest key");
        }
        if note.schema_version >= bbox_provenance::SCHEMA_VERSION_V2 {
            for call in note
                .tool_calls
                .iter()
                .filter(|call| authenticated_edge_kind_for_call(call).is_some())
            {
                let target = validated_target_for_project(call, project_id)
                    .ok_or_else(|| anyhow::anyhow!("invalid v2 provenance target_ref"))?;
                if !target_is_member(&target)? {
                    anyhow::bail!("v2 provenance target is not in the pinned project corpus");
                }
            }
        }
        let edges = edges_from_authenticated_note(
            project_id,
            import_generation_id,
            document_sha256,
            &note,
            resolve_legacy_target,
            target_is_member,
        )?;
        prepared
            .edges_by_project
            .entry(project_id.to_string())
            .or_default()
            .extend(edges);
    }
    Ok(prepared)
}

fn parse_note_for_target(raw: &str, target_commit: &str) -> Option<GitProvenanceNote> {
    let note = parse_note_document(raw).ok()?;
    (note.commit == target_commit).then_some(note)
}

/// Publish a previously prepared provenance import. The daemon calls this
/// only while its publication guard remains held after final lease
/// revalidation.
pub fn publish_prepared_provenance_import(
    prepared: PreparedProvenanceImport,
    edges_dir: &Path,
) -> Result<u64> {
    publish_prepared_provenance_import_bounded(prepared, edges_dir, u64::MAX)
}

pub fn publish_prepared_provenance_import_bounded(
    prepared: PreparedProvenanceImport,
    edges_dir: &Path,
    max_existing_lane_bytes: u64,
) -> Result<u64> {
    let mut edges_imported = 0u64;
    for (project_id, edges) in prepared.edges_by_project {
        edges_imported +=
            bbox_edge_index::edge_index::append_explicit_edges_atomic_bounded(
                edges_dir,
                &project_id,
                &edges,
                max_existing_lane_bytes,
            )
                .map_err(|_| {
                    anyhow::anyhow!(
                        "error.provenance_store_unavailable: imported provenance edges could not be persisted"
                    )
                })? as u64;
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
            project_id: None,
        });
    }
    Ok(edges)
}

fn edges_from_authenticated_note(
    project_id: &str,
    import_generation_id: &str,
    document_sha256: &str,
    note: &GitProvenanceNote,
    resolve_legacy_target: &PinnedLegacyTargetResolver<'_>,
    target_is_member: &PinnedTargetMembership<'_>,
) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    for call in &note.tool_calls {
        let Some(edge_kind) = authenticated_edge_kind_for_call(call) else {
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
            let target = validated_target_for_project(call, project_id)
                .ok_or_else(|| anyhow::anyhow!("invalid v2 provenance target_ref"))?;
            if !target_is_member(&target)? {
                anyhow::bail!("v2 provenance target is not in the pinned project corpus");
            }
            target
        } else {
            let Some(file) = file else {
                continue;
            };
            let Some(target) =
                resolve_legacy_target(file, call.byte_range.map(|range| (range[0], range[1])))?
            else {
                continue;
            };
            target
        };
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.to_string());
        if let Some(file) = file {
            metadata.insert("anchor.file_path".into(), file.to_string());
        }
        metadata.insert("anchor.commit_sha_at_edit".into(), note.commit.clone());
        metadata.insert(
            "provenance.import_generation_id".into(),
            import_generation_id.to_string(),
        );
        metadata.insert(
            "provenance.document_sha256".into(),
            document_sha256.to_string(),
        );
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
            project_id: None,
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

fn authenticated_edge_kind_for_call(call: &NoteToolCall) -> Option<&str> {
    bbox_provenance::authenticated_edge_kind_for_call(call)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_provenance::ProducedBy;

    #[test]
    fn export_failure_preserves_completed_counts_and_current_uncertainty() {
        let targets = (0..4)
            .map(|index| PreparedExportTarget {
                project_id: format!("project-{index}"),
                commit: format!("commit-{index}"),
                root: PathBuf::from("unused-synthetic-root"),
                documents: vec!["synthetic-note".into()],
            })
            .collect::<Vec<_>>();
        let mut attempted = Vec::new();
        let reply = export_prepared_targets(&targets, "refs/notes/bbox", |target| {
            attempted.push(target.commit.clone());
            if attempted.len() == 2 {
                anyhow::bail!("synthetic-secret-process-output");
            }
            Ok(3)
        })
        .unwrap();
        assert_eq!(attempted, ["commit-0", "commit-1"]);
        assert!(!reply.contains("synthetic-secret-process-output"));
        assert!(!reply.contains("unused-synthetic-root"));
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["notes_written"], 3);
        assert_eq!(value["completed_targets"], 1);
        assert_eq!(value["unattempted_targets"], 2);
        assert_eq!(value["failed_target"]["commit"], "commit-1");
        assert_eq!(value["failed_target"]["may_have_written"], true);
        let success: serde_json::Value = serde_json::from_str(
            &export_prepared_targets(&targets, "refs/notes/bbox", |_| Ok(2)).unwrap(),
        )
        .unwrap();
        assert_eq!(success["status"], "ok");
        assert_eq!(success["notes_written"], 8);
        assert_eq!(success["completed_targets"], 4);
    }

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
    fn note_import_rejects_document_commit_mismatch() {
        let note = GitProvenanceNote::new_v2(
            "commit-a",
            ProducedBy {
                provider: None,
                session_ids: Vec::new(),
                brofiles: Vec::new(),
                arc_thread_ids: Vec::new(),
                trigger: None,
            },
            Vec::new(),
            Vec::new(),
        );
        let raw = serialize_note(&note).unwrap();

        assert!(parse_note_for_target(&raw, "commit-a").is_some());
        assert!(parse_note_for_target(&raw, "commit-b").is_none());
    }

    #[test]
    fn prepared_import_does_not_touch_sidecars_until_publish() {
        let dir = tempfile::tempdir().unwrap();
        let edge = Edge {
            source: EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "session-one".into(),
                line_offset: 7,
                event_idx: 0,
            },
            kind: "READ_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: "project-one".into(),
                rel_path_hash: "path".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata: BTreeMap::new(),
            project_id: None,
        };
        let mut prepared = PreparedProvenanceImport::default();
        prepared
            .edges_by_project
            .insert("project-one".into(), vec![edge]);

        let sidecar = dir.path().join("explicit/project-one.jsonl");
        assert!(!sidecar.exists());
        assert_eq!(
            publish_prepared_provenance_import(prepared, dir.path()).unwrap(),
            1
        );
        assert!(sidecar.exists());
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
            project_id: None,
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
            project_id: None,
        };
        let edge_index = EdgeIndex::from_edges_for_tests(vec![edge_a.clone(), edge_b.clone()]);

        let note = note_from_edges("abc123", &[&edge_a, &edge_b], &edge_index);

        assert_eq!(note.produced_by.session_ids, vec!["sess-a", "sess-b"]);
        assert_eq!(note.schema_version, bbox_provenance::SCHEMA_VERSION_V2);
        assert!(note.tool_calls.iter().all(|call| call.target_ref.is_some()));
    }

    #[test]
    fn authenticated_v2_import_fails_closed_on_foreign_or_inactive_targets() {
        let note = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy::default(),
            vec![NoteToolCall {
                tool: "Edit".into(),
                edge_kind: None,
                source_ref: Some("transcript:test:session:1:0".into()),
                target_ref: Some(format!(
                    "project_file:other-project:path:{}:0",
                    "a".repeat(64)
                )),
                file: Some("src/lib.rs".into()),
                byte_range: Some([0, 1]),
                turn: Some(1),
            }],
            Vec::new(),
        );
        let document = serialize_note(&note).unwrap();
        assert!(
            prepare_authenticated_provenance_import(
                "project-one",
                "pgi_test",
                &[("abc123".into(), "a".repeat(64), document)],
                &|_, _| unreachable!("v2 must not fall back to a V1 resolver"),
                &|_| Ok(true),
            )
            .is_err()
        );

        let target = format!("project_file:project-one:path:{}:0", "a".repeat(64));
        let mut note = note;
        note.tool_calls[0].target_ref = Some(target);
        let document = serialize_note(&note).unwrap();
        assert!(
            prepare_authenticated_provenance_import(
                "project-one",
                "pgi_test",
                &[("abc123".into(), "a".repeat(64), document)],
                &|_, _| unreachable!("v2 must not fall back to a V1 resolver"),
                &|_| Ok(false),
            )
            .is_err()
        );
    }

    #[test]
    fn authenticated_import_ignores_non_file_calls() {
        let target = format!("project_file:project-one:path:{}:0", "a".repeat(64));
        let note = GitProvenanceNote::new_v2(
            "abc123",
            ProducedBy::default(),
            vec![
                NoteToolCall {
                    tool: "Bash".into(),
                    edge_kind: Some("RAN_BASH".into()),
                    source_ref: Some("transcript:test:session:1:0".into()),
                    target_ref: None,
                    file: None,
                    byte_range: None,
                    turn: Some(1),
                },
                NoteToolCall {
                    tool: "Read".into(),
                    edge_kind: Some("READ_FILE".into()),
                    source_ref: Some("transcript:test:session:1:0".into()),
                    target_ref: Some(target),
                    file: Some("src/lib.rs".into()),
                    byte_range: Some([0, 1]),
                    turn: Some(1),
                },
            ],
            Vec::new(),
        );
        let document = serialize_note(&note).unwrap();
        let prepared = prepare_authenticated_provenance_import(
            "project-one",
            "pgi_test",
            &[("abc123".into(), "a".repeat(64), document)],
            &|_, _| unreachable!("v2 must not fall back to a V1 resolver"),
            &|_| Ok(true),
        )
        .unwrap();
        assert_eq!(prepared.edge_count(), 1);
        assert_eq!(
            prepared.edges_by_project["project-one"][0].kind,
            "READ_FILE"
        );
    }

    #[test]
    fn authenticated_v1_import_uses_the_pinned_relative_path_resolver() {
        let note = GitProvenanceNote {
            schema_version: bbox_provenance::SCHEMA_VERSION_V1,
            commit: "abc123".into(),
            part: None,
            produced_by: ProducedBy::default(),
            tool_calls: vec![NoteToolCall {
                tool: "Read".into(),
                edge_kind: None,
                source_ref: Some("transcript:test:session:1:0".into()),
                target_ref: None,
                file: Some("src/lib.rs".into()),
                byte_range: Some([4, 8]),
                turn: Some(1),
            }],
            knowledge_writes: vec![bbox_provenance::KnowledgeWrite {
                id: "ignored".into(),
                kind: "remember".into(),
            }],
        };
        let document = serialize_note(&note).unwrap();
        let target = EntityRef::ProjectFile {
            project_id: "project-one".into(),
            rel_path_hash: "path".into(),
            chunk_hash: "b".repeat(64),
            occurrence_idx: 0,
        };
        let prepared = prepare_authenticated_provenance_import(
            "project-one",
            "pgi_test",
            &[("abc123".into(), "a".repeat(64), document)],
            &|path, range| {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(range, Some((4, 8)));
                Ok(Some(target.clone()))
            },
            &|_| unreachable!("v1 has no typed target membership probe"),
        )
        .unwrap();
        assert_eq!(prepared.edge_count(), 1);
        assert_eq!(prepared.ordered_import_keys().len(), 1);
        let edge = &prepared.edges_by_project["project-one"][0];
        assert_eq!(
            edge.metadata
                .get("provenance.import_generation_id")
                .map(String::as_str),
            Some("pgi_test")
        );
        assert_eq!(
            edge.metadata
                .get("provenance.document_sha256")
                .map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }
}
