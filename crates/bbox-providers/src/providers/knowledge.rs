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

pub struct ProvisionalKnowledgeProvider;

impl InspectableEntityProvider for ProvisionalKnowledgeProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProvisionalKnowledge
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProvisionalKnowledge { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        let EntityRef::ProvisionalKnowledge {
            scope_hash,
            checkout_id,
            entry_id,
        } = r
        else {
            anyhow::bail!("expected provisional knowledge ref");
        };
        let ref_string = r.to_string();
        let kb = ctx
            .knowledge_view()
            .ok_or_else(|| anyhow::anyhow!("provisional knowledge requires a visibility view"))?;
        let entry = kb.entry(&ref_string).ok_or_else(|| {
            anyhow::anyhow!("provisional knowledge entry {ref_string} is not visible")
        })?;
        let mut properties = BTreeMap::new();
        properties.insert("id".into(), entry_id.clone());
        properties.insert("scope_hash".into(), scope_hash.clone());
        properties.insert("checkout_id".into(), checkout_id.clone());
        properties.insert("logical_ref".into(), format!("knowledge:{entry_id}"));
        insert_entry_properties(&mut properties, entry);
        if let Some(metadata) = kb.view_metadata(&ref_string) {
            if let Some(scope) = &metadata.published_scope {
                properties.insert("repo_id".into(), scope.repo_id().to_string());
                properties.insert(
                    "bbox_root_relpath".into(),
                    scope.bbox_root_relpath().to_string(),
                );
            }
            if let Some(content_hash) = &metadata.content_hash {
                properties.insert("content_hash".into(), content_hash.clone());
            }
            if let Some(stamp) = &metadata.overlay_snapshot_id {
                properties.insert("overlay_snapshot_id".into(), stamp.clone());
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let ref_string = r.to_string();
        ctx.knowledge_view()
            .and_then(|kb| kb.entry(&ref_string))
            .map(|entry| truncate_label(&entry.title))
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
