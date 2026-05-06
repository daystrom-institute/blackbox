use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct BashCallProvider;

impl InspectableEntityProvider for BashCallProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::BashCall
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::BashCall { .. })
    }

    fn handles_virtual(&self) -> bool {
        true
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::BashCall { session, turn } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("session".into(), session.clone());
        properties.insert("turn".into(), turn.to_string());
        properties.insert("virtual".into(), "true".into());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["session", "turn", "virtual"],
            &["BASH_CALL_IN_SESSION", "BASH_CALL_PRODUCED_OUTPUT"],
            &["session"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("BASH_CALL_IN_SESSION", false),
            expected("BASH_CALL_PRODUCED_OUTPUT", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(
            full_neighborhood,
            &["BASH_CALL_IN_SESSION", "BASH_CALL_PRODUCED_OUTPUT"],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::BashCall { session, turn } = r else {
            return None;
        };
        Some(truncate_label(format!("{session}:{turn}")))
    }
}
