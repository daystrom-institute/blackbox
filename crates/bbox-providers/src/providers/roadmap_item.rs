use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, schema,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct RoadmapItemProvider;

impl InspectableEntityProvider for RoadmapItemProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::RoadmapItem
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::RoadmapItem { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::RoadmapItem { id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("id".into(), id.clone());
        properties.insert("lifecycle".into(), "historical_read_only".into());
        if let Some(stores) = ctx.stores() {
            let rm = stores.roadmap.read();
            if let Some(item) = rm.item(id) {
                properties.insert("title".into(), item.title.clone());
                properties.insert("body".into(), item.body.clone());
                properties.insert("status".into(), item.status.as_str().into());
                properties.insert("category".into(), item.category.as_str().into());
                properties.insert("priority".into(), item.priority.as_str().into());
                properties.insert("scope".into(), item.scope.clone());
                properties.insert("created_at".into(), item.created_at.clone());
                properties.insert("updated_at".into(), item.updated_at.clone());
                if let Some(project) = &item.project {
                    properties.insert("project".into(), project.clone());
                }
            } else {
                anyhow::bail!("roadmap item '{id}' not found");
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "id",
                "title",
                "status",
                "category",
                "priority",
                "scope",
                "lifecycle",
            ],
            &[
                "ROADMAP_SPAWNS",
                "ROADMAP_DEFERRED_FROM",
                "ROADMAP_DESIGNED_IN",
                "ROADMAP_DEPENDS_ON",
                "ROADMAP_BLOCKED_BY",
                "ROADMAP_SUPERSEDES",
                "ROADMAP_SUBSUMES",
                "ROADMAP_RELATED_TO",
            ],
            &["id"],
        )
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        match r {
            EntityRef::RoadmapItem { id } => Some(id.clone()),
            _ => None,
        }
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        _full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        vec![]
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("ROADMAP_SPAWNS", false),
            expected("ROADMAP_DEFERRED_FROM", false),
            expected("ROADMAP_DESIGNED_IN", false),
            expected("ROADMAP_DEPENDS_ON", false),
            expected("ROADMAP_BLOCKED_BY", false),
            expected("ROADMAP_SUPERSEDES", false),
            expected("ROADMAP_SUBSUMES", false),
            expected("ROADMAP_RELATED_TO", false),
        ]
    }
}
