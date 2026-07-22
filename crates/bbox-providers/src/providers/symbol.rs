use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, expected, next_hops, schema, truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct SymbolProvider;
pub struct SymbolV2Provider;

impl InspectableEntityProvider for SymbolProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Symbol
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Symbol { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        symbol_entity(ctx, r)
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
        let (_, _, qualified_name, _) = symbol_parts(r)?;
        Some(truncate_label(qualified_name))
    }
}

impl InspectableEntityProvider for SymbolV2Provider {
    fn entity_type(&self) -> EntityType {
        EntityType::SymbolV2
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::SymbolV2 { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        symbol_entity(ctx, r)
    }

    fn schema(&self) -> EntitySchemaView {
        let mut view = SymbolProvider.schema();
        view.entity_type = EntityType::SymbolV2;
        view.properties.insert(1, "snapshot_id".into());
        view.filterable_fields.insert(1, "snapshot_id".into());
        view
    }

    fn expected_edge_families(&self, r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        SymbolProvider.expected_edge_families(r)
    }

    fn recommended_next_hops(
        &self,
        entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        SymbolProvider.recommended_next_hops(entity, full_neighborhood)
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let (_, snapshot_id, qualified_name, _) = symbol_parts(r)?;
        let suffix = snapshot_id.map(|id| format!("@{id}")).unwrap_or_default();
        Some(truncate_label(format!("{qualified_name}{suffix}")))
    }
}

fn symbol_parts(r: &EntityRef) -> Option<(&str, Option<&str>, &str, &str)> {
    match r {
        EntityRef::Symbol {
            project_id,
            qualified_name,
            defn_hash,
        } => Some((project_id, None, qualified_name, defn_hash)),
        EntityRef::SymbolV2 {
            project_id,
            snapshot_id,
            qualified_name,
            defn_hash,
        } => Some((project_id, Some(snapshot_id), qualified_name, defn_hash)),
        _ => None,
    }
}

fn symbol_entity(ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
    let Some((project_id, snapshot_id, qualified_name, defn_hash)) = symbol_parts(r) else {
        unreachable!();
    };
    let mut properties = BTreeMap::new();
    properties.insert("project_id".into(), project_id.to_string());
    if let Some(snapshot_id) = snapshot_id {
        properties.insert("snapshot_id".into(), snapshot_id.to_string());
    }
    properties.insert("qualified_name".into(), qualified_name.to_string());
    properties.insert("defn_hash".into(), defn_hash.to_string());
    if ctx.stores().is_some() {
        match ctx.indexed_entity_properties(&r.to_string())? {
            Some(indexed) => {
                properties.extend(indexed);
            }
            None => {
                // Symbols are edge-projected vertices: the indexer derives
                // DEFINED_IN/CONTAINS_SYMBOL/CALLS edges but writes no
                // entity doc (gap-496fe07f). When the call site supplied
                // the edge sidecar, edge participation IS existence; a
                // well-formed ref nothing points at stays not_found.
                let edge_backed = ctx.edge_index().is_some_and(|edges| {
                    !edges.forward_edges(r).is_empty() || !edges.reverse_edges(r).is_empty()
                });
                if !edge_backed {
                    anyhow::bail!("symbol entity {r} not found");
                }
                properties.insert("source".into(), "edge_projection".into());
            }
        }
    }
    Ok(empty_neighborhood_view(r, properties))
}
