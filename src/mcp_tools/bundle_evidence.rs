use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::edge_index::EdgeIndex;
use crate::entity_loader;
use crate::entity_ref::EntityRef;
use crate::mcp_tools::find_paths::{render_node, render_path};
use crate::path_cache::{CachedPath, PROCESS_SESSION_KEY, PathCache};
use crate::providers::ProviderContext;

const ENTITY_CAP: usize = 50;
const PATH_CAP: usize = 20;
const INTRA_BUNDLE_EDGE_CAP: usize = 200;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BundleEvidenceParams {
    pub question: String,
    pub entity_refs: Vec<String>,
    pub path_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UnresolvedEntityRef {
    #[serde(rename = "ref")]
    entity_ref: String,
    error: String,
}

pub fn bundle_evidence(
    p: &BundleEvidenceParams,
    ctx: &ProviderContext<'_>,
    edge_index: &EdgeIndex,
    cache: &mut PathCache,
) -> Result<String> {
    let mut entities = Vec::new();
    let mut unresolved_entity_refs = Vec::new();
    if p.entity_refs.is_empty() {
        return Ok(not_found_bundle(Vec::new()));
    }
    let omitted_entity_refs = p.entity_refs.len().saturating_sub(ENTITY_CAP);
    for raw in p.entity_refs.iter().take(ENTITY_CAP) {
        let r = match EntityRef::parse(raw) {
            Ok(r) => r,
            Err(err) => {
                unresolved_entity_refs.push(UnresolvedEntityRef {
                    entity_ref: raw.clone(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        let view = match entity_loader::load(ctx, &r) {
            Ok(view) => view,
            Err(err) => {
                unresolved_entity_refs.push(UnresolvedEntityRef {
                    entity_ref: r.to_string(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        entities.push((r, view.properties));
    }
    if entities.is_empty() {
        return Ok(not_found_bundle(unresolved_entity_refs));
    }

    let mut paths = Vec::new();
    let mut stale_path_ids = Vec::new();
    let omitted_path_ids = p.path_ids.len().saturating_sub(PATH_CAP);
    for path_id in p.path_ids.iter().take(PATH_CAP) {
        match cache.get(PROCESS_SESSION_KEY, path_id) {
            Some(path) => paths.push(path),
            None => stale_path_ids.push(path_id.clone()),
        }
    }

    let refs = entities.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>();
    let mut intra_bundle_edges = intra_bundle_edges(edge_index, &refs);
    let intra_bundle_edges_truncated = intra_bundle_edges.len() > INTRA_BUNDLE_EDGE_CAP;
    intra_bundle_edges.truncate(INTRA_BUNDLE_EDGE_CAP);
    let mut intra_bundle_convergences = intra_bundle_convergences(edge_index, &refs);
    let intra_bundle_convergences_truncated =
        intra_bundle_convergences.len() > INTRA_BUNDLE_EDGE_CAP;
    intra_bundle_convergences.truncate(INTRA_BUNDLE_EDGE_CAP);
    let text = render_text(
        ctx,
        &p.question,
        &entities,
        &paths,
        &stale_path_ids,
        &unresolved_entity_refs,
    );
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "text": text,
        "question": p.question,
        "entities": entities.iter().map(|(r, properties)| json!({
            "entity_ref": r.to_string(),
            "label": render_node(ctx, r),
            "properties": properties,
        })).collect::<Vec<_>>(),
        "paths": paths.iter().map(|path| json!({
            "id": path.id,
            "summary": render_path(ctx, path),
            "steps": path.steps,
        })).collect::<Vec<_>>(),
        "intra_bundle_edges": intra_bundle_edges,
        "intra_bundle_convergences": intra_bundle_convergences,
        "degraded": {
            "stale_path_ids": stale_path_ids,
            "unresolved_entity_refs": unresolved_entity_refs,
            "omitted_entity_refs": omitted_entity_refs,
            "omitted_path_ids": omitted_path_ids,
            "intra_bundle_edges_truncated": intra_bundle_edges_truncated,
            "intra_bundle_convergences_truncated": intra_bundle_convergences_truncated,
        }
    }))?)
}

fn intra_bundle_edges(edge_index: &EdgeIndex, refs: &[EntityRef]) -> Vec<serde_json::Value> {
    let set = refs.iter().collect::<HashSet<_>>();
    let mut edges = Vec::new();
    for source in refs {
        for edge in edge_index.forward_edges(source) {
            if set.contains(&edge.target) {
                edges.push(json!({
                    "source": edge.source.to_string(),
                    "kind": edge.kind,
                    "target": edge.target.to_string(),
                }));
            }
        }
    }
    edges
}

/// Surfaces 2-hop convergences: pairs of bundled entities (A, B) that share
/// a common neighbor C via outgoing or incoming edges. Examples:
/// - two project_files both EDITED_BY_SESSION the same session ("touched in
///   the same conversation")
/// - two project_files both COMMIT_TOUCHED_FILE the same commit ("changed
///   together")
/// - a knowledge entry and a session it was KNOWLEDGE_FROM_SESSION'd from
/// Direct edges (1-hop) already surface in `intra_bundle_edges`; this fills
/// in the structurally more common case where bundled chunks aren't
/// directly connected but share an originating event.
fn intra_bundle_convergences(
    edge_index: &EdgeIndex,
    refs: &[EntityRef],
) -> Vec<serde_json::Value> {
    use std::collections::HashMap;
    // For each ref, gather the set of (neighbor_ref, edge_kind, direction)
    // tuples reachable in one hop. Direction lets us distinguish the source
    // and report it back to the user.
    type Neighbor = (EntityRef, String, &'static str);
    let mut neighbors_by_ref: HashMap<usize, Vec<Neighbor>> = HashMap::new();
    for (idx, r) in refs.iter().enumerate() {
        let mut neighbors = Vec::new();
        for edge in edge_index.forward_edges(r) {
            neighbors.push((edge.target.clone(), edge.kind.clone(), "out"));
        }
        for edge in edge_index.reverse_edges(r) {
            neighbors.push((edge.source.clone(), edge.kind.clone(), "in"));
        }
        neighbors_by_ref.insert(idx, neighbors);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::<(EntityRef, EntityRef, EntityRef)>::new();
    for i in 0..refs.len() {
        for j in (i + 1)..refs.len() {
            let a_neighbors = match neighbors_by_ref.get(&i) {
                Some(n) => n,
                None => continue,
            };
            let b_neighbors = match neighbors_by_ref.get(&j) {
                Some(n) => n,
                None => continue,
            };
            // Index B's neighbors by ref for O(1) intersection lookup.
            let b_index: HashMap<&EntityRef, Vec<&Neighbor>> = b_neighbors
                .iter()
                .fold(HashMap::new(), |mut acc, n| {
                    acc.entry(&n.0).or_default().push(n);
                    acc
                });
            for an in a_neighbors {
                let Some(b_matches) = b_index.get(&an.0) else {
                    continue;
                };
                // Skip self-references — a chunk's own IN_FILE proxy
                // isn't a meaningful convergence between siblings.
                if an.0 == refs[i] || an.0 == refs[j] {
                    continue;
                }
                for bn in b_matches {
                    let key = (refs[i].clone(), refs[j].clone(), an.0.clone());
                    if !seen.insert(key) {
                        continue;
                    }
                    out.push(json!({
                        "a": refs[i].to_string(),
                        "b": refs[j].to_string(),
                        "via": an.0.to_string(),
                        "via_label": render_node_short(&an.0),
                        "a_edge_kind": an.1,
                        "a_direction": an.2,
                        "b_edge_kind": bn.1,
                        "b_direction": bn.2,
                    }));
                }
            }
        }
    }
    out
}

fn render_node_short(r: &EntityRef) -> String {
    // Compact labeling — the bundle viewer can call inspect for details.
    r.to_string()
}

fn render_text(
    ctx: &ProviderContext<'_>,
    question: &str,
    entities: &[(EntityRef, BTreeMap<String, String>)],
    paths: &[CachedPath],
    stale_path_ids: &[String],
    unresolved_entity_refs: &[UnresolvedEntityRef],
) -> String {
    let mut text = format!("## Evidence Bundle\n\nQuestion: {question}\n\n### Entities\n");
    for (r, _) in entities {
        text.push_str(&format!("- {}\n", render_node(ctx, r)));
    }
    text.push_str("\n### Paths\n");
    for path in paths {
        text.push_str(&format!("- {}: {}\n", path.id, render_path(ctx, path)));
    }
    if !stale_path_ids.is_empty() {
        text.push_str(&format!(
            "\nDegraded: stale path IDs: {}\n",
            stale_path_ids.join(", ")
        ));
    }
    if !unresolved_entity_refs.is_empty() {
        text.push_str("\nDegraded: unresolved entity refs:\n");
        for unresolved in unresolved_entity_refs {
            text.push_str(&format!(
                "- {}: {}\n",
                unresolved.entity_ref, unresolved.error
            ));
        }
    }
    text
}

fn not_found_bundle(unresolved_entity_refs: Vec<UnresolvedEntityRef>) -> String {
    json!({
        "status": "error.not_found",
        "error": {
            "code": "error.not_found",
            "message": "No requested entity refs resolved",
            "unresolved_entity_refs": unresolved_entity_refs,
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_path_ids_degrade_without_failing_bundle() {
        let params = BundleEvidenceParams {
            question: "what changed?".into(),
            entity_refs: vec!["knowledge:abc123".into()],
            path_ids: vec!["P999".into()],
        };
        let rendered = bundle_evidence(
            &params,
            &ProviderContext::empty_for_tests(),
            &EdgeIndex::default(),
            &mut PathCache::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["degraded"]["stale_path_ids"][0], "P999");
    }

    #[test]
    fn unresolved_entity_refs_degrade_when_some_resolve() {
        let params = BundleEvidenceParams {
            question: "what changed?".into(),
            entity_refs: vec![
                "knowledge:a".into(),
                "knowledge:b".into(),
                "knowledge:c".into(),
                "not-a-ref".into(),
                "also-not-a-ref".into(),
            ],
            path_ids: Vec::new(),
        };
        let rendered = bundle_evidence(
            &params,
            &ProviderContext::empty_for_tests(),
            &EdgeIndex::default(),
            &mut PathCache::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["entities"].as_array().unwrap().len(), 3);
        assert_eq!(
            value["degraded"]["unresolved_entity_refs"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn all_unresolved_entity_refs_return_not_found() {
        let params = BundleEvidenceParams {
            question: "what changed?".into(),
            entity_refs: vec!["not-a-ref".into()],
            path_ids: Vec::new(),
        };
        let rendered = bundle_evidence(
            &params,
            &ProviderContext::empty_for_tests(),
            &EdgeIndex::default(),
            &mut PathCache::default(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["status"], "error.not_found");
    }
}
