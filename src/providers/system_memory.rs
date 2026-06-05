use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, schema, truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct SystemMemoryProvider;

impl InspectableEntityProvider for SystemMemoryProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::SystemMemory
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::SystemMemory { .. })
    }

    fn get_entity(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::SystemMemory { id } = r else {
            unreachable!();
        };
        let memory = crate::system_memory::get(id)
            .ok_or_else(|| anyhow::anyhow!("system memory {id} not found"))?;
        let mut properties = BTreeMap::new();
        properties.insert("id".into(), memory.id.clone());
        properties.insert("title".into(), memory.title.clone());
        properties.insert("tags".into(), memory.tags.join(","));
        properties.insert("content".into(), memory.content.clone());
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["id", "title", "tags", "content"],
            &[],
            &["id", "title", "tags"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        Vec::new()
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        _full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        Vec::new()
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::SystemMemory { id } = r else {
            return None;
        };
        crate::system_memory::get(id)
            .map(|memory| truncate_label(&memory.title))
            .or_else(|| Some(truncate_label(id)))
    }
}
