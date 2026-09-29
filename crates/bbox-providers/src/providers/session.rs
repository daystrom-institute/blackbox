use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct SessionProvider;

impl InspectableEntityProvider for SessionProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Session
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Session { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Session {
            provider,
            session_id,
        } = r
        else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("provider".into(), provider.clone());
        properties.insert("session_id".into(), session_id.clone());
        if let Some(stores) = ctx.stores() {
            // read_recursive: see the invariant on `CorpusStores::idx`.
            let indexed = stores
                .idx
                .read_recursive()
                .session_properties(provider, session_id)?
                .ok_or_else(|| anyhow::anyhow!("session entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Session {
            provider,
            session_id,
        } = r
        else {
            return None;
        };
        let short = session_id.chars().take(12).collect::<String>();
        if let Some(stores) = ctx.stores() {
            // read_recursive: see the invariant on `CorpusStores::idx`.
            if let Ok(Some(properties)) = stores
                .idx
                .read_recursive()
                .session_properties(provider, session_id)
            {
                if let Some(prompt) = properties.get("first_user_prompt") {
                    return Some(truncate_label(prompt));
                }
            }
        }
        Some(truncate_label(format!("session {provider}:{short}")))
    }
}
