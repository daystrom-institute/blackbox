use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct ProjectFileProvider;

impl InspectableEntityProvider for ProjectFileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProjectFile
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProjectFile { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::ProjectFile {
            project_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        } = r
        else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("project_id".into(), project_id.clone());
        properties.insert("rel_path_hash".into(), rel_path_hash.clone());
        properties.insert("chunk_hash".into(), chunk_hash.clone());
        properties.insert("occurrence_idx".into(), occurrence_idx.to_string());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "project_id",
                "rel_path_hash",
                "chunk_hash",
                "occurrence_idx",
                "chunk_kind",
                "language",
                "symbol",
            ],
            &[
                "CALLS",
                "CALLED_BY",
                "CONTAINS_SYMBOL",
                "IN_FILE",
                "EDITED_IN_COMMIT",
                "NEXT_SECTION",
                "LINKS_TO_FILE",
                "LINKS_TO_SECTION",
                "DESCRIBES",
            ],
            &["project_id", "chunk_kind", "language"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("CALLS", false),
            expected("CALLED_BY", false),
            expected("CONTAINS_SYMBOL", false),
            expected("IN_FILE", true),
            expected("EDITED_IN_COMMIT", false),
            expected("NEXT_SECTION", false),
            expected("LINKS_TO_FILE", false),
            expected("LINKS_TO_SECTION", false),
            expected("DESCRIBES", false),
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
                "CALLS",
                "CALLED_BY",
                "CONTAINS_SYMBOL",
                "IN_FILE",
                "EDITED_IN_COMMIT",
                "NEXT_SECTION",
                "LINKS_TO_FILE",
                "LINKS_TO_SECTION",
                "DESCRIBES",
            ],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::ProjectFile {
            rel_path_hash,
            occurrence_idx,
            ..
        } = r
        else {
            return None;
        };
        Some(truncate_label(format!("{rel_path_hash}#{occurrence_idx}")))
    }
}
