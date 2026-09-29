use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct BashCallProvider;

impl InspectableEntityProvider for BashCallProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::BashCall
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::BashCall { .. })
    }

    fn handles_virtual(&self) -> bool {
        true
    }

    fn get_entity(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::BashCall { session, turn } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("session".into(), session.clone());
        properties.insert("turn".into(), turn.to_string());
        properties.insert("virtual".into(), "true".into());
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::BashCall { session, turn } = r else {
            return None;
        };
        Some(truncate_label(format!("{session}:{turn}")))
    }
}
