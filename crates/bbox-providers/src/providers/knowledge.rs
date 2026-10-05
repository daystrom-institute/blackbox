use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_knowledge::knowledge::KnowledgeEntry;

pub struct KnowledgeProvider;

impl InspectableEntityProvider for KnowledgeProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Knowledge
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Knowledge { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Knowledge { id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("id".into(), id.clone());
        if let Some(kb) = ctx.knowledge_view() {
            let entry = kb
                .entry(id)
                .or_else(|| kb.entry_for_logical_ref(&format!("knowledge:{id}")))
                .ok_or_else(|| anyhow::anyhow!("knowledge entry {id} not found"))?;
            insert_entry_properties(&mut properties, entry);
        } else if let Some(stores) = ctx.stores() {
            let kb = stores.kb.read();
            let entry = kb
                .entry(id)
                .ok_or_else(|| anyhow::anyhow!("knowledge entry {id} not found"))?;
            insert_entry_properties(&mut properties, entry);
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Knowledge { id } = r else {
            return None;
        };
        if let Some(entry) = ctx.knowledge_view().and_then(|kb| {
            kb.entry(id)
                .or_else(|| kb.entry_for_logical_ref(&format!("knowledge:{id}")))
        }) {
            return Some(truncate_label(&entry.title));
        }
        if let Some(stores) = ctx.stores() {
            if let Some(entry) = stores.kb.read().entry(id) {
                return Some(truncate_label(&entry.title));
            }
        }
        Some(truncate_label(id))
    }
}

fn insert_entry_properties(properties: &mut BTreeMap<String, String>, entry: &KnowledgeEntry) {
    properties.insert("title".into(), entry.title.clone());
    properties.insert("content".into(), entry.content.clone());
    properties.insert("category".into(), format!("{:?}", entry.category));
    properties.insert("scope".into(), format!("{:?}", entry.scope));
    if let Some(project) = &entry.project {
        properties.insert("project".into(), project.clone());
    }
}
