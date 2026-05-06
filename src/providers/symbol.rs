use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    empty_neighborhood_view, ensure_type, expected, next_hops, schema, truncate_label,
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct SymbolProvider;

impl InspectableEntityProvider for SymbolProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Symbol
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Symbol { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Symbol {
            project_id,
            qualified_name,
            defn_hash,
        } = r
        else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("project_id".into(), project_id.clone());
        properties.insert("qualified_name".into(), qualified_name.clone());
        properties.insert("defn_hash".into(), defn_hash.clone());
        if let Some(state) = ctx.state() {
            let indexed = state
                .idx
                .read()
                .entity_properties(&r.to_string())?
                .ok_or_else(|| anyhow::anyhow!("symbol entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["project_id", "qualified_name", "defn_hash", "language"],
            &[
                "CALLS",
                "DEFINED_IN",
                "IMPLEMENTS_TRAIT",
                "EDITED_IN_COMMIT",
            ],
            &["project_id", "language", "qualified_name"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("CALLS", false),
            expected("DEFINED_IN", true),
            expected("IMPLEMENTS_TRAIT", false),
            expected("EDITED_IN_COMMIT", false),
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
                "DEFINED_IN",
                "IMPLEMENTS_TRAIT",
                "EDITED_IN_COMMIT",
            ],
        )
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Symbol { qualified_name, .. } = r else {
            return None;
        };
        Some(truncate_label(qualified_name))
    }
}
