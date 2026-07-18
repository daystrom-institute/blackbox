use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, expected, schema, truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct ProjectGraphProvider;

impl InspectableEntityProvider for ProjectGraphProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProjectGraphVertex
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProjectGraphVertex { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        let properties = match ctx.project_graph_properties(r)? {
            Some(properties) => properties,
            None if ctx.stores().is_none() => ref_properties(r),
            None => anyhow::bail!("project graph vertex {r} not found"),
        };
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "scope_id",
                "graph_id",
                "id",
                "type",
                "label",
                "generation",
                "namespace",
                "authority",
                "property.*",
            ],
            &[
                "meta:INSTANCE_OF",
                "meta:FROM_TYPE",
                "meta:TO_TYPE",
                "project-defined",
            ],
            &["scope_id", "graph_id", "type"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("meta:INSTANCE_OF", true),
            expected("meta:FROM_TYPE", false),
            expected("meta:TO_TYPE", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        let mut counts = BTreeMap::<String, usize>::new();
        for edge in full_neighborhood
            .forward
            .iter()
            .chain(full_neighborhood.reverse.iter())
        {
            *counts.entry(edge.kind.clone()).or_default() += 1;
        }
        let mut kinds = counts.keys().cloned().collect::<Vec<_>>();
        kinds.sort_by_key(|kind| (kind.starts_with("meta:"), kind.clone()));
        kinds
            .into_iter()
            .map(|kind| NextHop {
                count: counts[&kind],
                edge_family_name: kind,
            })
            .collect()
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        ctx.project_graph_properties(r)
            .ok()
            .flatten()
            .and_then(|properties| properties.get("label").cloned())
            .or_else(|| match r {
                EntityRef::ProjectGraphVertex { vertex_id, .. } => Some(vertex_id.clone()),
                _ => None,
            })
            .map(truncate_label)
    }
}

fn ref_properties(r: &EntityRef) -> BTreeMap<String, String> {
    match r {
        EntityRef::ProjectGraphVertex {
            scope_id,
            graph_id,
            vertex_id,
        } => BTreeMap::from([
            ("scope_id".into(), scope_id.clone()),
            ("graph_id".into(), graph_id.clone()),
            ("id".into(), vertex_id.clone()),
            ("label".into(), vertex_id.clone()),
        ]),
        _ => BTreeMap::new(),
    }
}
