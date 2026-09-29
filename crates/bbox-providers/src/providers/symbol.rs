use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, truncate_label,
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
                // Symbols have no entity doc of their own and the daemon keeps
                // no edge graph, so a symbol ref has no existence proof: it
                // resolves only where an indexed entity doc backs it.
                anyhow::bail!("symbol entity {r} not found");
            }
        }
    }
    Ok(empty_neighborhood_view(r, properties))
}
