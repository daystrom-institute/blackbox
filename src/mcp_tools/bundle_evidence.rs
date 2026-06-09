use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    /// Entity property payload mode. `full` preserves legacy behavior.
    /// `summary` truncates long string properties. `none` omits properties.
    #[serde(default)]
    pub property_mode: Option<String>,
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
    let property_mode = PropertyMode::from_param(p.property_mode.as_deref());
    // Build `degraded` from only the signal-bearing facts. A clean bundle —
    // nothing stale, nothing unresolved, nothing omitted or truncated, full
    // property mode — degraded to nothing, so the whole block is dropped rather
    // than emitting `{stale_path_ids: [], omitted_entity_refs: 0, ...}` padding
    // on every healthy response. property_mode is surfaced only when it altered
    // the payload (summary/none); "full" is the no-op default a reader assumes.
    let mut degraded = serde_json::Map::new();
    if !stale_path_ids.is_empty() {
        degraded.insert("stale_path_ids".into(), json!(stale_path_ids));
    }
    if !unresolved_entity_refs.is_empty() {
        degraded.insert(
            "unresolved_entity_refs".into(),
            json!(unresolved_entity_refs),
        );
    }
    if omitted_entity_refs > 0 {
        degraded.insert("omitted_entity_refs".into(), json!(omitted_entity_refs));
    }
    if omitted_path_ids > 0 {
        degraded.insert("omitted_path_ids".into(), json!(omitted_path_ids));
    }
    if intra_bundle_edges_truncated {
        degraded.insert("intra_bundle_edges_truncated".into(), json!(true));
    }
    if intra_bundle_convergences_truncated {
        degraded.insert("intra_bundle_convergences_truncated".into(), json!(true));
    }
    if property_mode != PropertyMode::Full {
        degraded.insert("property_mode".into(), json!(property_mode.as_str()));
    }
    let mut out = json!({
        "status": "ok",
        "text": text,
        "question": p.question,
        "entities": entities.iter().map(|(r, properties)| json!({
            "entity_ref": r.to_string(),
            "label": render_node(ctx, r),
            "properties": properties_for_mode(properties, property_mode),
        })).collect::<Vec<_>>(),
        "paths": paths.iter().map(|path| json!({
            "id": path.id,
            "summary": render_path(ctx, path),
            "steps": path.steps,
        })).collect::<Vec<_>>(),
    });
    // Intra-bundle structure is the exception, not the rule — most bundles have
    // none. Emit these arrays only when non-empty so the common case doesn't
    // carry two empty lists.
    if !intra_bundle_edges.is_empty() {
        out["intra_bundle_edges"] = json!(intra_bundle_edges);
    }
    if !intra_bundle_convergences.is_empty() {
        out["intra_bundle_convergences"] = json!(intra_bundle_convergences);
    }
    if !degraded.is_empty() {
        out["degraded"] = Value::Object(degraded);
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyMode {
    Full,
    Summary,
    None,
}

impl PropertyMode {
    fn from_param(raw: Option<&str>) -> Self {
        match raw.unwrap_or("full").trim().to_ascii_lowercase().as_str() {
            "summary" | "compact" => Self::Summary,
            "none" | "omit" | "omitted" => Self::None,
            _ => Self::Full,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Summary => "summary",
            Self::None => "none",
        }
    }
}

fn properties_for_mode(properties: &BTreeMap<String, String>, mode: PropertyMode) -> Value {
    match mode {
        PropertyMode::Full => json!(properties),
        PropertyMode::None => json!({}),
        PropertyMode::Summary => json!(
            properties
                .iter()
                .map(|(key, value)| (key.clone(), truncate_property(value, 600)))
                .collect::<BTreeMap<_, _>>()
        ),
    }
}

fn truncate_property(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("...[truncated]");
    out
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
fn intra_bundle_convergences(edge_index: &EdgeIndex, refs: &[EntityRef]) -> Vec<serde_json::Value> {
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
            let b_index: HashMap<&EntityRef, Vec<&Neighbor>> =
                b_neighbors.iter().fold(HashMap::new(), |mut acc, n| {
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
            property_mode: None,
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
                "packet:domain:phase-decompose/triage".into(),
                "artifact:packet/phase-decompose/triage@1".into(),
                "knowledge:b".into(),
                "knowledge:c".into(),
                "not-a-ref".into(),
                "also-not-a-ref".into(),
            ],
            path_ids: Vec::new(),
            property_mode: None,
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
        assert_eq!(value["entities"].as_array().unwrap().len(), 5);
        assert!(
            value["entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entity| entity["entity_ref"] == "packet:domain:phase-decompose/triage")
        );
        assert!(
            value["entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entity| entity["entity_ref"] == "artifact:packet/phase-decompose/triage@1")
        );
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
            property_mode: None,
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

    #[test]
    fn clean_bundle_omits_degraded_and_empty_intra_arrays() {
        // A single resolvable ref, full property mode, no paths: nothing
        // degraded and no intra-bundle structure. The padding blocks should
        // be absent entirely.
        crate::system_memory::init_for_tests();
        let params = BundleEvidenceParams {
            question: "what is the opening sequence?".into(),
            entity_refs: vec!["system_memory:sm-agentic-opening-sequence".into()],
            path_ids: Vec::new(),
            property_mode: None,
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
        for absent in [
            "degraded",
            "intra_bundle_edges",
            "intra_bundle_convergences",
        ] {
            assert!(
                value.get(absent).is_none(),
                "clean bundle should omit {absent}: {value}"
            );
        }
    }

    #[test]
    fn summary_property_mode_truncates_long_properties() {
        let mut properties = BTreeMap::new();
        properties.insert("content".to_string(), "x".repeat(700));
        properties.insert("title".to_string(), "short".to_string());

        let summarized = properties_for_mode(&properties, PropertyMode::Summary);

        assert_eq!(summarized["title"], "short");
        let content = summarized["content"].as_str().unwrap();
        assert!(content.len() < 700);
        assert!(content.ends_with("...[truncated]"));
    }

    #[test]
    fn none_property_mode_omits_properties() {
        let mut properties = BTreeMap::new();
        properties.insert("content".to_string(), "x".repeat(700));

        let omitted = properties_for_mode(&properties, PropertyMode::None);

        assert!(omitted.as_object().unwrap().is_empty());
    }

    #[test]
    fn system_memory_refs_are_bundleable() {
        crate::system_memory::init_for_tests();
        let params = BundleEvidenceParams {
            question: "what is the opening sequence?".into(),
            entity_refs: vec!["system_memory:sm-agentic-opening-sequence".into()],
            path_ids: Vec::new(),
            property_mode: Some("summary".into()),
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
        assert_eq!(
            value["entities"][0]["entity_ref"],
            "system_memory:sm-agentic-opening-sequence"
        );
        assert_eq!(
            value["entities"][0]["properties"]["id"],
            "sm-agentic-opening-sequence"
        );
        assert!(
            value["entities"][0]["properties"]["content"]
                .as_str()
                .unwrap()
                .ends_with("...[truncated]")
        );
    }
}
