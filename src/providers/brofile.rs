use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct BrofileProvider;

impl InspectableEntityProvider for BrofileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Brofile
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Brofile { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Brofile { name } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("name".into(), name.clone());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["name", "provider", "model", "effort"],
            &[
                "SESSION_USED_BROFILE",
                "ARC_USED_BROFILE",
                "BOARD_REGISTERED_AGENT",
            ],
            &["name", "provider"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("SESSION_USED_BROFILE", false),
            expected("ARC_USED_BROFILE", false),
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
            &[
                "SESSION_USED_BROFILE",
                "ARC_USED_BROFILE",
                "BOARD_REGISTERED_AGENT",
            ],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Brofile { name } = r else {
            return None;
        };
        Some(truncate_label(name))
    }
}
