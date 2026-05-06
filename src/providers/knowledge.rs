use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct KnowledgeProvider;

impl InspectableEntityProvider for KnowledgeProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Knowledge
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Knowledge { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Knowledge { id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("id".into(), id.clone());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["id", "title", "category", "scope", "status", "approval"],
            &[
                "SUPERSEDES",
                "DERIVED_FROM",
                "Contradicts",
                "KNOWLEDGE_FROM_SESSION",
                "KNOWLEDGE_FROM_BOARD",
            ],
            &["project", "category", "scope", "status"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("SUPERSEDES", false),
            expected("DERIVED_FROM", false),
            expected("Contradicts", false),
            expected("KNOWLEDGE_FROM_SESSION", false),
            expected("KNOWLEDGE_FROM_BOARD", false),
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
                "SUPERSEDES",
                "DERIVED_FROM",
                "Contradicts",
                "KNOWLEDGE_FROM_SESSION",
                "KNOWLEDGE_FROM_BOARD",
            ],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Knowledge { id } = r else {
            return None;
        };
        Some(truncate_label(id))
    }
}
