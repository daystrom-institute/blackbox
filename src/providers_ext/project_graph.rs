use anyhow::{Result, anyhow};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_providers::providers::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, schema, truncate_label,
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

    fn recommended_next_hops(&self, _entity: &EntityView, full: &Neighborhood) -> Vec<NextHop> {
        let mut counts = BTreeMap::<String, usize>::new();
        for edge in full.forward.iter().chain(full.reverse.iter()) {
            *counts.entry(edge.kind.clone()).or_default() += 1;
        }
        counts
            .into_iter()
            .map(|(edge_family_name, count)| NextHop {
                edge_family_name,
                count,
            })
            .collect()
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        self.get_entity(ctx, r)
            .ok()
            .and_then(|view| view.properties.get("label").map(truncate_label))
    }
}
use std::collections::BTreeMap;
