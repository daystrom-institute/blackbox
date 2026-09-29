use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct CommitProvider;

impl InspectableEntityProvider for CommitProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Commit
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Commit { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Commit { repo_id, sha } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("repo_id".into(), repo_id.clone());
        properties.insert("sha".into(), sha.clone());
        if ctx.stores().is_some() {
            let indexed = ctx
                .indexed_entity_properties_with_content(&r.to_string())?
                .ok_or_else(|| anyhow::anyhow!("commit entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Commit { sha, .. } = r else {
            return None;
        };
        let short = sha.chars().take(7).collect::<String>();
        if ctx.stores().is_some() {
            if let Ok(Some(properties)) = ctx.indexed_entity_properties(&r.to_string()) {
                if let Some(preview) = properties.get("content_preview") {
                    let subject = preview.lines().next().unwrap_or(preview);
                    return Some(truncate_label(format!("{short} {subject}")));
                }
            }
        }
        Some(truncate_label(short))
    }
}
