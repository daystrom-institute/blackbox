use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

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
        if let Some(state) = ctx.state() {
            let indexed = state
                .idx
                .read()
                .session_properties(provider, session_id)?
                .ok_or_else(|| anyhow::anyhow!("session entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["provider", "session_id", "first_user_prompt"],
            &["THREAD_HAS_SESSION", "SESSION_USED_BROFILE", "IN_SESSION"],
            &["provider", "session_id"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("THREAD_HAS_SESSION", false),
            expected("SESSION_USED_BROFILE", false),
            expected("IN_SESSION", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(
            full_neighborhood,
            &["THREAD_HAS_SESSION", "SESSION_USED_BROFILE", "IN_SESSION"],
        )
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
        if let Some(state) = ctx.state() {
            if let Ok(Some(properties)) = state.idx.read().session_properties(provider, session_id)
            {
                if let Some(prompt) = properties.get("first_user_prompt") {
                    return Some(truncate_label(prompt));
                }
            }
        }
        Some(truncate_label(format!("session {provider}:{short}")))
    }
}
