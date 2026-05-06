use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct WhiteboardProvider;

impl InspectableEntityProvider for WhiteboardProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Whiteboard
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Whiteboard { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Whiteboard { board_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("board_id".into(), board_id.clone());
        if let Some(state) = ctx.state() {
            let board = state
                .whiteboards
                .get(board_id)
                .ok_or_else(|| anyhow::anyhow!("whiteboard entity {board_id} not found"))?;
            let board = board.read();
            properties.insert("topic".into(), board.topic.clone());
            properties.insert("project".into(), board.project.clone());
            properties.insert("phase".into(), format!("{:?}", board.phase));
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["board_id", "topic", "project", "phase"],
            &["BOARD_FROM_ARC", "BOARD_REGISTERED_AGENT"],
            &["project", "phase"],
        )
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

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Whiteboard { board_id } = r else {
            return None;
        };
        if let Some(state) = ctx.state() {
            if let Some(board) = state.whiteboards.get(board_id) {
                return Some(truncate_label(&board.read().topic));
            }
        }
        Some(truncate_label(board_id))
    }
}
