//! Pure corpus-side provenance inventory and generation-bound pagination.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::identity::PublishedScope;
use bbox_edge_index::edge_index::{Edge, EdgeIndex};
use bbox_provenance::{
    FragmentError, MAX_NOTE_DOCUMENT_BYTES, MAX_PAGE_DOCUMENT_BYTES, MAX_PAGE_DOCUMENTS,
    NoteToolCall, ProducedBy, ProvenanceExportDocument, ProvenanceExportPage, ProvenanceExportPlan,
    fragment_note,
};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

const MAX_SERIALIZED_PAGE_BYTES: usize = 56 * 1024;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ProvenanceExportPlanParams {
    /// Opaque cursor returned by the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Generation returned by the first page. Required with a cursor.
    #[serde(default)]
    pub generation: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PlanCursor {
    commit: String,
    part_index: u32,
}

pub fn export_plan_page(
    params: &ProvenanceExportPlanParams,
    scope: PublishedScope,
    project_id: &str,
    notes_ref: &str,
    edge_index: &EdgeIndex,
) -> Result<ProvenanceExportPage> {
    let edges = edge_index.all_edges();
    let plan = build_plan_from_observed_edges(scope, project_id, notes_ref, edges, edge_index)?;
    export_plan_page_from_plan(params, &plan)
}

pub fn export_plan_page_from_plan(
    params: &ProvenanceExportPlanParams,
    plan: &ProvenanceExportPlan,
) -> Result<ProvenanceExportPage> {
    validate_requested_generation(params, plan)?;
    paginate_plan(params.cursor.as_deref(), plan)
}

/// Build the pure provenance plan from an explicitly supplied observed-lane
/// inventory. Relation lookups may use the published index, but primary file
/// events never come from it; imported explicit edges therefore cannot leak
/// back into an export.
pub fn build_plan_from_observed_edges<'a>(
    scope: PublishedScope,
    project_id: &str,
    notes_ref: &str,
    observed_edges: impl IntoIterator<Item = &'a Edge>,
    edge_index: &EdgeIndex,
) -> Result<ProvenanceExportPlan> {
    let mut grouped = BTreeMap::<String, Vec<&Edge>>::new();
    for edge in observed_edges {
        if !matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE")
            || edge.metadata.get("anchor.project_id").map(String::as_str) != Some(project_id)
        {
            continue;
        }
        let Some(commit) = edge.metadata.get("anchor.commit_sha_at_edit") else {
            continue;
        };
        if !matches!(
            &edge.target,
            EntityRef::ProjectFile {
                project_id: target_project_id,
                ..
            } | EntityRef::ProjectFileV2 {
                project_id: target_project_id,
                ..
            } if target_project_id == project_id
        ) {
            bail!(
                "error.invalid_provenance_target: tracked file edge does not target this project"
            );
        }
        grouped.entry(commit.clone()).or_default().push(edge);
    }

    let mut documents = Vec::new();
    for (commit, edges) in grouped {
        let note = note_from_edges(&commit, &edges, edge_index);
        let parts = fragment_note(&note, MAX_NOTE_DOCUMENT_BYTES).map_err(fragment_error)?;
        for part in parts {
            documents.push(ProvenanceExportDocument::from_note(&part)?);
        }
    }
    ProvenanceExportPlan::new(scope, project_id, notes_ref, documents)
}

pub(crate) fn note_from_edges(
    commit: &str,
    edges: &[&Edge],
    edge_index: &EdgeIndex,
) -> bbox_provenance::GitProvenanceNote {
    let produced_by = produced_by_from_edges(edges, edge_index);
    let mut tool_calls = edges
        .iter()
        .map(|edge| tool_call_from_edge(edge))
        .collect::<Vec<_>>();
    tool_calls.sort_by(|left, right| {
        (
            &left.tool,
            &left.edge_kind,
            &left.source_ref,
            &left.target_ref,
            &left.file,
            left.byte_range,
            left.turn,
        )
            .cmp(&(
                &right.tool,
                &right.edge_kind,
                &right.source_ref,
                &right.target_ref,
                &right.file,
                right.byte_range,
                right.turn,
            ))
    });
    bbox_provenance::GitProvenanceNote::new_v2(commit, produced_by, tool_calls, Vec::new())
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
        target_ref: Some(edge.target.to_string()),
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

fn fragment_error(error: FragmentError) -> anyhow::Error {
    match error {
        FragmentError::ToolCallTooLarge { .. } => {
            anyhow!("error.tool_call_too_large: {error}")
        }
        FragmentError::DocumentBaseTooLarge { .. } => {
            anyhow!("error.note_metadata_too_large: {error}")
        }
        _ => anyhow!(error),
    }
}

fn validate_requested_generation(
    params: &ProvenanceExportPlanParams,
    plan: &ProvenanceExportPlan,
) -> Result<()> {
    if params.cursor.is_some() && params.generation.is_none() {
        bail!("error.stale_generation: generation is required with a provenance cursor");
    }
    if params
        .generation
        .as_deref()
        .is_some_and(|generation| generation != plan.generation)
    {
        bail!("error.stale_generation: provenance inventory changed");
    }
    Ok(())
}

fn paginate_plan(
    cursor: Option<&str>,
    plan: &ProvenanceExportPlan,
) -> Result<ProvenanceExportPage> {
    let start = cursor_start(cursor, &plan.documents)?;
    let mut documents = Vec::new();
    let mut document_bytes = 0usize;

    for document in plan.documents.iter().skip(start) {
        if documents.len() == MAX_PAGE_DOCUMENTS
            || document_bytes + document.document.len() > MAX_PAGE_DOCUMENT_BYTES
        {
            break;
        }
        let mut candidate = documents.clone();
        candidate.push(document.clone());
        let candidate_cursor = Some(encode_cursor(document)?);
        let candidate_page = plan.page(candidate, candidate_cursor);
        if serialized_page_len(&candidate_page)? > MAX_SERIALIZED_PAGE_BYTES {
            if documents.is_empty() {
                bail!(
                    "error.page_document_too_large: one provenance document exceeds the page envelope"
                );
            }
            break;
        }
        document_bytes += document.document.len();
        documents.push(document.clone());
    }

    let consumed = start + documents.len();
    let next_cursor = if consumed < plan.documents.len() {
        Some(encode_cursor(
            documents
                .last()
                .context("provenance page made no progress")?,
        )?)
    } else {
        None
    };
    let page = plan.page(documents, next_cursor);
    if serialized_page_len(&page)? > MAX_SERIALIZED_PAGE_BYTES {
        bail!("error.page_too_large: provenance page exceeds its serialized envelope cap");
    }
    Ok(page)
}

fn cursor_start(cursor: Option<&str>, documents: &[ProvenanceExportDocument]) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let cursor = decode_cursor(cursor)?;
    documents
        .iter()
        .position(|document| {
            document.commit == cursor.commit && document.part_index == cursor.part_index
        })
        .map(|position| position + 1)
        .ok_or_else(|| anyhow!("error.invalid_cursor: provenance cursor is not in this generation"))
}

fn encode_cursor(document: &ProvenanceExportDocument) -> Result<String> {
    Ok(hex::encode(serde_json::to_vec(&PlanCursor {
        commit: document.commit.clone(),
        part_index: document.part_index,
    })?))
}

fn decode_cursor(cursor: &str) -> Result<PlanCursor> {
    let bytes = hex::decode(cursor).context("error.invalid_cursor: cursor is not hexadecimal")?;
    serde_json::from_slice(&bytes).context("error.invalid_cursor: cursor payload is malformed")
}

fn serialized_page_len(page: &ProvenanceExportPage) -> Result<usize> {
    Ok(serde_json::to_vec(page)?.len())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::entity_ref::EntityRef;

    use super::*;

    fn commit(number: usize) -> String {
        format!("{number:040x}")
    }

    fn edge(project_id: &str, commit: &str, turn: u32, path_payload: usize) -> Edge {
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.into());
        metadata.insert("anchor.commit_sha_at_edit".into(), commit.into());
        metadata.insert(
            "anchor.file_path".into(),
            format!("src/{}-{turn}.rs", "x".repeat(path_payload)),
        );
        metadata.insert("tool.name".into(), "Edit".into());
        Edge {
            source: EntityRef::Transcript {
                provider: "test".into(),
                session_id: format!("session-{turn}"),
                line_offset: u64::from(turn),
                event_idx: 0,
            },
            kind: "EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: project_id.into(),
                rel_path_hash: format!("path-{turn}"),
                chunk_hash: format!("{turn:064x}"),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata,
            project_id: None,
        }
    }

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo", ".").unwrap()
    }

    #[test]
    fn pagination_is_deterministic_generation_bound_and_capped() {
        let edges = (1..=80)
            .map(|number| edge("project", &commit(number), number as u32, 500))
            .collect();
        let index = EdgeIndex::from_edges_for_tests(edges);
        let first = export_plan_page(
            &ProvenanceExportPlanParams::default(),
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap();
        assert!(first.next_cursor.is_some());
        assert!(first.documents.len() <= MAX_PAGE_DOCUMENTS);
        assert!(serialized_page_len(&first).unwrap() <= MAX_SERIALIZED_PAGE_BYTES);

        let second = export_plan_page(
            &ProvenanceExportPlanParams {
                cursor: first.next_cursor.clone(),
                generation: Some(first.generation.clone()),
            },
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap();
        assert_ne!(first.documents, second.documents);
        assert_eq!(first.generation, second.generation);

        let missing_generation = export_plan_page(
            &ProvenanceExportPlanParams {
                cursor: first.next_cursor,
                generation: None,
            },
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap_err();
        assert!(
            missing_generation
                .to_string()
                .contains("error.stale_generation")
        );

        let stale_generation = export_plan_page(
            &ProvenanceExportPlanParams {
                cursor: None,
                generation: Some("stale".into()),
            },
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap_err();
        assert!(
            stale_generation
                .to_string()
                .contains("error.stale_generation")
        );

        let invalid_cursor = export_plan_page(
            &ProvenanceExportPlanParams {
                cursor: Some("00".into()),
                generation: Some(first.generation),
            },
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap_err();
        assert!(invalid_cursor.to_string().contains("error.invalid_cursor"));
    }

    #[test]
    fn generation_ignores_edge_insertion_order() {
        let first_edge = edge("project", &commit(1), 1, 10);
        let second_edge = edge("project", &commit(1), 2, 10);
        let forward =
            EdgeIndex::from_edges_for_tests(vec![first_edge.clone(), second_edge.clone()]);
        let reverse = EdgeIndex::from_edges_for_tests(vec![second_edge, first_edge]);
        let params = ProvenanceExportPlanParams::default();
        let first = export_plan_page(
            &params,
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &forward,
        )
        .unwrap();
        let second = export_plan_page(
            &params,
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &reverse,
        )
        .unwrap();
        assert_eq!(first.generation, second.generation);
        assert_eq!(first.documents, second.documents);
    }

    #[test]
    fn direct_observed_inventory_excludes_imported_and_ran_bash_edges() {
        let observed = edge("project", &commit(1), 1, 10);
        let imported = edge("project", &commit(2), 2, 10);
        let mut ran_bash = edge("project", &commit(3), 3, 10);
        ran_bash.kind = "RAN_BASH".into();
        let relation_index =
            EdgeIndex::from_edges_for_tests(vec![observed.clone(), imported, ran_bash.clone()]);
        let direct_lane = [observed, ran_bash];
        let plan = build_plan_from_observed_edges(
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            direct_lane.iter(),
            &relation_index,
        )
        .unwrap();
        assert_eq!(plan.documents.len(), 1);
        assert_eq!(plan.documents[0].commit, commit(1));
    }

    #[test]
    fn large_commit_fragments_and_oversized_call_fails_explicitly() {
        let edges = (1..=60)
            .map(|turn| edge("project", &commit(1), turn, 700))
            .collect();
        let index = EdgeIndex::from_edges_for_tests(edges);
        let page = export_plan_page(
            &ProvenanceExportPlanParams::default(),
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap();
        assert!(page.documents.len() > 1);

        let oversized = EdgeIndex::from_edges_for_tests(vec![edge(
            "project",
            &commit(2),
            1,
            MAX_NOTE_DOCUMENT_BYTES * 2,
        )]);
        let error = export_plan_page(
            &ProvenanceExportPlanParams::default(),
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &oversized,
        )
        .unwrap_err();
        assert!(error.to_string().contains("error.tool_call_too_large"));
    }

    #[test]
    fn cross_project_target_fails_during_planning() {
        let mut wrong = edge("project", &commit(1), 1, 10);
        wrong.target = EntityRef::ProjectFile {
            project_id: "other-project".into(),
            rel_path_hash: "path".into(),
            chunk_hash: "a".repeat(64),
            occurrence_idx: 0,
        };
        let index = EdgeIndex::from_edges_for_tests(vec![wrong]);
        let error = export_plan_page(
            &ProvenanceExportPlanParams::default(),
            scope(),
            "project",
            "refs/notes/bbox/provenance",
            &index,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("error.invalid_provenance_target")
        );
    }

    #[test]
    fn planner_source_has_no_checkout_or_authority_reads() {
        let source = include_str!("provenance_plan.rs");
        let forbidden = [
            concat!("std::", "fs"),
            concat!("std::process::", "Command"),
            concat!("bbox_", "config"),
            concat!("elect_", "publisher"),
            concat!("write_", "note"),
            concat!("show_", "note"),
        ];
        for token in forbidden {
            assert!(
                !source.contains(token),
                "planner contains forbidden token {token}"
            );
        }
    }
}
