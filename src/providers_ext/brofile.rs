use std::collections::BTreeMap;

use anyhow::Result;

use crate::providers::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct BrofileProvider;

impl InspectableEntityProvider for BrofileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Brofile
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Brofile { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Brofile { name } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("name".into(), name.clone());
        if let Some(state) = ctx
            .ext()
            .and_then(|ext| ext.downcast_ref::<crate::server::state::SharedState>())
        {
            let brofile =
                crate::orchestration::brofile::list_brofiles("global", &state.store_dir, None)
                    .into_iter()
                    .find(|brofile| brofile.name == *name)
                    .ok_or_else(|| anyhow::anyhow!("brofile entity {name} not found"))?;
            properties.insert("provider".into(), brofile.provider.as_str().into());
            if let Some(model) = brofile.model {
                properties.insert("model".into(), model);
            }
            if let Some(effort) = brofile.effort {
                properties.insert("effort".into(), effort);
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Brofile { name } = r else {
            return None;
        };
        Some(truncate_label(name))
    }
}
