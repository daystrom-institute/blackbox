use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

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
        if let Some(stores) = ctx.stores() {
            let kb = stores.kb.read();
            let entry = kb
                .entry(id)
                .ok_or_else(|| anyhow::anyhow!("knowledge entry {id} not found"))?;
            properties.insert("title".into(), entry.title.clone());
            properties.insert("content".into(), entry.content.clone());
            properties.insert("category".into(), format!("{:?}", entry.category));
            properties.insert("scope".into(), format!("{:?}", entry.scope));
            properties.insert("status".into(), format!("{:?}", entry.status));
            properties.insert("approval".into(), format!("{:?}", entry.approval));
            if let Some(project) = &entry.project {
                properties.insert("project".into(), project.clone());
            }
            if let Some(supersedes) = &entry.supersedes {
                properties.insert("supersedes".into(), supersedes.clone());
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["id", "title", "category", "scope", "status", "approval"],
            &[
                "SUPERSEDES",
                "DERIVED_FROM",
                "Contradicts",
                "KNOWLEDGE_FROM_SESSION",
                "KNOWLEDGE_FROM_BOARD",
            ],
            &["project", "category", "scope", "status"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("SUPERSEDES", false),
            expected("DERIVED_FROM", false),
            expected("Contradicts", false),
            expected("KNOWLEDGE_FROM_SESSION", false),
            expected("KNOWLEDGE_FROM_BOARD", false),
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
                "SUPERSEDES",
                "DERIVED_FROM",
                "Contradicts",
                "KNOWLEDGE_FROM_SESSION",
                "KNOWLEDGE_FROM_BOARD",
            ],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Knowledge { id } = r else {
            return None;
        };
        if let Some(stores) = ctx.stores() {
            if let Some(entry) = stores.kb.read().entry(id) {
                return Some(truncate_label(&entry.title));
            }
        }
        Some(truncate_label(id))
    }
}
