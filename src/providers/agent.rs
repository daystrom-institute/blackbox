use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct AgentProvider;

impl InspectableEntityProvider for AgentProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Agent
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Agent { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Agent { name, version } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("name".into(), name.clone());
        properties.insert("version".into(), version.to_string());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "name",
                "version",
                "description",
                "brofile_ref",
                "when_to_use",
            ],
            &["DERIVED_FROM", "SUPERSEDES"],
            &["name", "version"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("DERIVED_FROM", false),
            expected("SUPERSEDES", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(full_neighborhood, &["DERIVED_FROM", "SUPERSEDES"])
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Agent { name, version } = r else {
            return None;
        };
        Some(truncate_label(format!("{name}@v{version}")))
    }
}
