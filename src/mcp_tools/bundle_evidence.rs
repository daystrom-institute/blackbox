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
    for raw in &p.entity_refs {
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
    for path_id in &p.path_ids {
        match cache.get(PROCESS_SESSION_KEY, path_id) {
            Some(path) => paths.push(path),
            None => stale_path_ids.push(path_id.clone()),
        }
    }

    let refs = entities.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>();
    let intra_bundle_edges = intra_bundle_edges(edge_index, &refs);
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
        "degraded": {
            "stale_path_ids": stale_path_ids,
            "unresolved_entity_refs": unresolved_entity_refs,
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
