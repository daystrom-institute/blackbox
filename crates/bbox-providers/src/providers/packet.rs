use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct PacketProvider;

impl InspectableEntityProvider for PacketProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Packet
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Packet { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Packet { selector } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("selector".into(), selector.clone());
        if let Some(stores) = ctx.stores() {
            let packet = stores.packets.read().load(selector)?;
            properties.insert("id".into(), packet.id);
            properties.insert("domain".into(), packet.domain);
            properties.insert("scope".into(), packet.scope);
            if let Some(project) = packet.project {
                properties.insert("project".into(), project);
            }
            properties.insert("rule_count".into(), packet.rules.len().to_string());
            properties.insert(
                "classification_lattice".into(),
                packet.classification_lattice.join(","),
            );
            properties.insert("created_at".into(), packet.created_at);
            properties.insert("updated_at".into(), packet.updated_at);
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "selector",
                "id",
                "domain",
                "scope",
                "project",
                "rule_count",
                "classification_lattice",
                "created_at",
                "updated_at",
            ],
            &["DERIVED_FROM", "SUPERSEDES"],
            &["selector", "id", "domain", "scope"],
        )
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

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Packet { selector } = r else {
            return None;
        };
        Some(truncate_label(selector))
    }
}
