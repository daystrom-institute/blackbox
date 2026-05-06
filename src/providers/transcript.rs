use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::edge_index::Edge;
use crate::entity_ref::{EntityRef, EntityType};

pub struct TranscriptProvider;

impl InspectableEntityProvider for TranscriptProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Transcript
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Transcript { .. })
    }

    fn get_entity(&self, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Transcript {
            provider,
            session_id,
            line_offset,
            event_idx,
        } = r
        else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("provider".into(), provider.clone());
        properties.insert("session_id".into(), session_id.clone());
        properties.insert("line_offset".into(), line_offset.to_string());
        properties.insert("event_idx".into(), event_idx.to_string());
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["provider", "session_id", "line_offset", "event_idx", "role"],
            &["IN_SESSION", "EDITED_FILE", "READ_FILE"],
            &["provider", "session_id", "role"],
        )
    }

    fn forward_edges(&self, _r: &EntityRef) -> Vec<Edge> {
        Vec::new()
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("IN_SESSION", true),
            expected("EDITED_FILE", false),
            expected("READ_FILE", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(
            full_neighborhood,
            &["IN_SESSION", "EDITED_FILE", "READ_FILE"],
        )
    }

    fn compact_label(&self, r: &EntityRef) -> Option<String> {
        let EntityRef::Transcript {
            provider,
            session_id,
            line_offset,
            ..
        } = r
        else {
            return None;
        };
        Some(truncate_label(format!(
            "{provider}:{session_id}@{line_offset}"
        )))
    }
}
