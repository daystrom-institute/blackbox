use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct WhiteboardProvider;

impl InspectableEntityProvider for WhiteboardProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Whiteboard
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Whiteboard { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Whiteboard { board_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("board_id".into(), board_id.clone());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["board_id", "topic", "project", "phase"],
            &["BOARD_FROM_ARC", "BOARD_REGISTERED_AGENT"],
            &["project", "phase"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("BOARD_FROM_ARC", false),
            expected("BOARD_REGISTERED_AGENT", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(
            full_neighborhood,
            &["BOARD_FROM_ARC", "BOARD_REGISTERED_AGENT"],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Whiteboard { board_id } = r else {
            return None;
        };
        Some(truncate_label(board_id))
    }
}
