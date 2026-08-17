use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_providers::providers::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, NextHopDirection, ProviderContext, schema, truncate_label,
};

pub(crate) struct ProjectGraphVertexProvider {
    provisional: bool,
}

impl ProjectGraphVertexProvider {
    pub(crate) fn published() -> Self {
        Self { provisional: false }
    }

    pub(crate) fn provisional() -> Self {
        Self { provisional: true }
    }
}

impl InspectableEntityProvider for ProjectGraphVertexProvider {
    fn entity_type(&self) -> EntityType {
        if self.provisional {
            EntityType::ProvisionalProjectGraphVertex
        } else {
            EntityType::ProjectGraphVertex
        }
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        r.entity_type() == self.entity_type()
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ctx.project_graph_resolver()
            .ok_or_else(|| anyhow!("project graph provider requires a request resolver"))?
            .resolve_entity(r, ctx.provisional_mode())
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "id",
                "type",
                "label",
                "project_id",
                "graph_id",
                "logical_ref",
                "content_hash",
                "source",
                "checkout_id",
                "properties",
            ],
            &[],
            &["project_id", "graph_id", "type", "source"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        Vec::new()
    }

    /// Three lanes, in priority order.
    ///
    /// 1. AUTHORED schema hints, in schema order, direction-aware, INCLUDING
    ///    zero-count ones: a schema author who says "open findings matter here"
    ///    is asking for "(none)" to be visible, because absence is the answer
    ///    to the question the hint poses.
    /// 2. Tier-0 DERIVED hints (an edge type whose declared endpoints touch
    ///    this vertex type) that actually have edges, ranked by count.
    /// 3. Whatever edge families were OBSERVED but no hint covers, which is the
    ///    evidence-binding lane and anything a graph carries beyond its own
    ///    schema. Direction-blind and alphabetical, exactly as before, so a
    ///    vertex with no hints renders identically to the pre-hint provider.
    fn recommended_next_hops(&self, entity: &EntityView, full: &Neighborhood) -> Vec<NextHop> {
        let mut directed_counts = BTreeMap::<(String, NextHopDirection), usize>::new();
        for edge in &full.forward {
            *directed_counts
                .entry((edge.kind.clone(), NextHopDirection::Out))
                .or_default() += 1;
        }
        for edge in &full.reverse {
            *directed_counts
                .entry((edge.kind.clone(), NextHopDirection::In))
                .or_default() += 1;
        }
        let count_of = |family: &str, direction: NextHopDirection| {
            directed_counts
                .get(&(family.to_string(), direction))
                .copied()
                .unwrap_or(0)
        };

        let mut hops = Vec::new();
        let mut hinted_families = BTreeSet::<String>::new();
        let mut emitted = BTreeSet::<(String, NextHopDirection)>::new();

        for hint in entity.next_hop_hints.iter().filter(|hint| hint.authored) {
            let key = (hint.edge_family_name.clone(), hint.direction);
            if !emitted.insert(key) {
                continue;
            }
            hinted_families.insert(hint.edge_family_name.clone());
            hops.push(NextHop {
                count: count_of(&hint.edge_family_name, hint.direction),
                edge_family_name: hint.edge_family_name.clone(),
                direction: Some(hint.direction),
                label: hint.label.clone(),
                authored: true,
            });
        }

        let mut derived = entity
            .next_hop_hints
            .iter()
            .filter(|hint| !hint.authored)
            .filter(|hint| !emitted.contains(&(hint.edge_family_name.clone(), hint.direction)))
            .map(|hint| (count_of(&hint.edge_family_name, hint.direction), hint))
            .filter(|(count, _)| *count > 0)
            .collect::<Vec<_>>();
        derived.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.edge_family_name.cmp(&right.1.edge_family_name))
                .then_with(|| left.1.direction.cmp(&right.1.direction))
        });
        for (count, hint) in derived {
            if !emitted.insert((hint.edge_family_name.clone(), hint.direction)) {
                continue;
            }
            hinted_families.insert(hint.edge_family_name.clone());
            hops.push(NextHop {
                edge_family_name: hint.edge_family_name.clone(),
                count,
                direction: Some(hint.direction),
                label: hint.label.clone(),
                authored: false,
            });
        }

        // Observed-but-unhinted families, keyed on the family alone: a family
        // already surfaced in one direction is not repeated direction-blind.
        let mut observed = BTreeMap::<String, usize>::new();
        for edge in full.forward.iter().chain(full.reverse.iter()) {
            if hinted_families.contains(&edge.kind) {
                continue;
            }
            *observed.entry(edge.kind.clone()).or_default() += 1;
        }
        hops.extend(
            observed
                .into_iter()
                .map(|(edge_family_name, count)| NextHop {
                    edge_family_name,
                    count,
                    direction: None,
                    label: None,
                    authored: false,
                }),
        );
        hops
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        self.get_entity(ctx, r)
            .ok()
            .and_then(|view| view.properties.get("label").map(truncate_label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_edge_index::edge_index::Edge;
    use bbox_providers::providers::NextHopHint;
    use std::collections::BTreeMap;

    fn vertex_ref(id: &str) -> EntityRef {
        EntityRef::parse(&format!("project_graph_vertex:proj:records:{id}")).unwrap()
    }

    fn edge(source: &str, kind: &str, target: &str) -> Edge {
        Edge {
            source: vertex_ref(source),
            kind: kind.to_string(),
            target: vertex_ref(target),
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: None,
        }
    }

    fn hint(family: &str, direction: NextHopDirection, label: Option<&str>) -> NextHopHint {
        NextHopHint {
            edge_family_name: family.to_string(),
            direction,
            label: label.map(str::to_string),
            authored: label.is_some(),
        }
    }

    fn view(hints: Vec<NextHopHint>) -> EntityView {
        let mut view = bbox_providers::providers::empty_neighborhood_view(
            &vertex_ref("record-1"),
            BTreeMap::new(),
        );
        view.next_hop_hints = hints;
        view
    }

    #[test]
    fn authored_hints_lead_and_keep_zero_counts_with_direction_aware_totals() {
        let neighborhood = Neighborhood {
            forward: vec![
                edge("record-1", "gov:GOVERNS", "agreement-1"),
                edge("record-1", "gov:GOVERNS", "agreement-2"),
            ],
            reverse: vec![edge("correction-1", "gov:DELTA", "record-1")],
        };
        let hops = ProjectGraphVertexProvider::published().recommended_next_hops(
            &view(vec![
                // Declared but unobserved: must survive as "(none)".
                hint("gov:DIAGNOSES", NextHopDirection::In, Some("open findings")),
                hint("gov:GOVERNS", NextHopDirection::Out, Some("governed scope")),
                // Derived tail.
                hint("gov:DELTA", NextHopDirection::In, None),
                hint("gov:GOVERNS", NextHopDirection::In, None),
            ]),
            &neighborhood,
        );

        let rendered = hops
            .iter()
            .map(|hop| {
                (
                    hop.edge_family_name.as_str(),
                    hop.direction,
                    hop.count,
                    hop.authored,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec![
                ("gov:DIAGNOSES", Some(NextHopDirection::In), 0, true),
                ("gov:GOVERNS", Some(NextHopDirection::Out), 2, true),
                ("gov:DELTA", Some(NextHopDirection::In), 1, false),
            ],
            "authored hints lead in schema order and zero-count ones survive; \
             the derived in-direction of gov:GOVERNS has no edges and drops"
        );
        assert_eq!(hops[1].label.as_deref(), Some("governed scope"));
    }

    #[test]
    fn observed_families_no_hint_covers_stay_direction_blind_and_alphabetical() {
        let neighborhood = Neighborhood {
            forward: vec![edge("record-1", "evidence:SUPPORTS", "knowledge-1")],
            reverse: vec![
                edge("claim-1", "gov:CITES", "record-1"),
                edge("claim-2", "gov:CITES", "record-1"),
            ],
        };
        let hops = ProjectGraphVertexProvider::published()
            .recommended_next_hops(&view(Vec::new()), &neighborhood);
        assert_eq!(
            hops.iter()
                .map(|hop| (hop.edge_family_name.as_str(), hop.direction, hop.count))
                .collect::<Vec<_>>(),
            vec![("evidence:SUPPORTS", None, 1), ("gov:CITES", None, 2),],
            "a vertex with no hints keeps the pre-hint direction-blind, \
             alphabetical, combined-count rendering"
        );
    }

    #[test]
    fn a_hinted_family_is_not_repeated_direction_blind() {
        let neighborhood = Neighborhood {
            forward: vec![edge("record-1", "gov:SUPERSEDES", "record-0")],
            reverse: vec![edge("record-2", "gov:SUPERSEDES", "record-1")],
        };
        let hops = ProjectGraphVertexProvider::published().recommended_next_hops(
            &view(vec![hint(
                "gov:SUPERSEDES",
                NextHopDirection::Out,
                Some("prior version"),
            )]),
            &neighborhood,
        );
        assert_eq!(
            hops.iter()
                .map(|hop| (hop.edge_family_name.as_str(), hop.direction, hop.count))
                .collect::<Vec<_>>(),
            vec![("gov:SUPERSEDES", Some(NextHopDirection::Out), 1)]
        );
    }
}
