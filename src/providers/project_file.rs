use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct ProjectFileProvider;

impl InspectableEntityProvider for ProjectFileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProjectFile
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProjectFile { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
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
        if let Some(state) = ctx.state() {
            let indexed = state
                .idx
                .read()
                .entity_properties(&r.to_string())?
                .ok_or_else(|| anyhow::anyhow!("project file entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(empty_neighborhood_view(r, properties))
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
                "EDITED_BY_SESSION",
                "EDITED_IN_COMMIT",
                "NEXT_SECTION",
                "LINKS_TO_FILE",
                "LINKS_TO_SECTION",
                "DESCRIBES",
            ],
            &["project_id", "chunk_kind", "language"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("CONTAINS_SYMBOL", false),
            expected("DEFINED_IN", false),
            expected("CALLS", false),
            expected("CALLED_BY", false),
            expected("EDITED_FILE", false),
            expected("READ_FILE", false),
            expected("EDITED_BY_SESSION", false),
            expected("EDITED_IN_COMMIT", false),
            expected("COMMIT_TOUCHED_FILE", false),
            expected("DESCRIBES", false),
            expected("LINKS_TO_FILE", false),
            expected("LINKS_TO_SECTION", false),
            expected("IN_FILE", true),
            expected("NEXT_SECTION", false),
            expected("NEXT_CHUNK", false),
            expected("PREV_CHUNK", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        // Order matters — `select_notable_edges` walks this list and picks the
        // first PER_DIRECTION_EDGE_LIMIT matches. Keep semantic edges
        // (provenance, symbol/call relationships) ahead of structural noise
        // (IN_FILE self-proxy, NEXT_SECTION/NEXT_CHUNK siblings) so the seed
        // surface tells the agent something actionable instead of "this chunk
        // is in a file with other chunks". The structural kinds stay in the
        // list as fallback when nothing semantic exists.
        next_hops(
            full_neighborhood,
            &[
                // Symbol/call graph
                "CONTAINS_SYMBOL",
                "DEFINED_IN",
                "CALLS",
                "CALLED_BY",
                // Provenance: who touched this chunk and from where
                "EDITED_FILE",
                "READ_FILE",
                "EDITED_BY_SESSION",
                "EDITED_IN_COMMIT",
                "COMMIT_TOUCHED_FILE",
                // Semantic linking
                "DESCRIBES",
                "LINKS_TO_FILE",
                "LINKS_TO_SECTION",
                // Structural fallback
                "IN_FILE",
                "NEXT_SECTION",
                "NEXT_CHUNK",
                "PREV_CHUNK",
            ],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::ProjectFile {
            rel_path_hash,
            occurrence_idx,
            ..
        } = r
        else {
            return None;
        };
        if let Some(state) = ctx.state() {
            if let Ok(Some(properties)) = state.idx.read().entity_properties(&r.to_string()) {
                if let Some(path) = properties.get("file_path") {
                    return Some(truncate_label(path));
                }
                if let Some(preview) = properties.get("content_preview") {
                    return Some(truncate_label(preview));
                }
            }
        }
        Some(truncate_label(format!("{rel_path_hash}#{occurrence_idx}")))
    }
}
