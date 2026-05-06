use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct CommitProvider;

impl InspectableEntityProvider for CommitProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Commit
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Commit { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Commit { repo_id, sha } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("repo_id".into(), repo_id.clone());
        properties.insert("sha".into(), sha.clone());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["repo_id", "sha", "subject", "author"],
            &[
                "COMMIT_PARENT",
                "COMMIT_PRODUCED_BY_ARC",
                "COMMIT_TOUCHED_FILE",
            ],
            &["repo_id", "sha"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("COMMIT_PARENT", false),
            expected("COMMIT_PRODUCED_BY_ARC", false),
            expected("COMMIT_TOUCHED_FILE", false),
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
                "COMMIT_PARENT",
                "COMMIT_PRODUCED_BY_ARC",
                "COMMIT_TOUCHED_FILE",
            ],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Commit { sha, .. } = r else {
            return None;
        };
        let short = sha.chars().take(7).collect::<String>();
        Some(truncate_label(short))
    }
}
