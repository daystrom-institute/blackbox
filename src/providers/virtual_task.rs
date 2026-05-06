use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct TaskProvider;

impl InspectableEntityProvider for TaskProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Task
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Task { .. })
    }

    fn handles_virtual(&self) -> bool {
        true
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Task { task_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("task_id".into(), task_id.clone());
        properties.insert("virtual".into(), "true".into());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["task_id", "virtual"],
            &["TASK_PRODUCED_NOTE", "NOTE_FROM_TASK"],
            &["task_id"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("TASK_PRODUCED_NOTE", false),
            expected("NOTE_FROM_TASK", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(full_neighborhood, &["TASK_PRODUCED_NOTE", "NOTE_FROM_TASK"])
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Task { task_id } = r else {
            return None;
        };
        Some(truncate_label(task_id))
    }
}
