use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct ThreadProvider;

impl InspectableEntityProvider for ThreadProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Thread
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Thread { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Thread { thread_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("thread_id".into(), thread_id.clone());
        if let Some(state) = ctx.state() {
            let threads = state.threads.read();
            let thread = threads
                .all()
                .into_iter()
                .find(|thread| thread.id == *thread_id)
                .ok_or_else(|| anyhow::anyhow!("thread entity {thread_id} not found"))?;
            properties.insert("topic".into(), thread.topic.clone());
            properties.insert("project".into(), thread.project.clone());
            properties.insert("status".into(), format!("{:?}", thread.status));
            properties.insert("kind".into(), format!("{:?}", thread.kind));
            if let Some(name) = &thread.name {
                properties.insert("name".into(), name.clone());
            }
        }
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["thread_id", "name", "topic", "kind", "status"],
            &[
                "THREAD_HAS_SESSION",
                "ARC_USED_BROFILE",
                "ARC_OPENED_BOARD",
                "ARC_PRODUCED_COMMIT",
                "NOTE_IN_THREAD",
            ],
            &["kind", "status", "project"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("THREAD_HAS_SESSION", false),
            expected("ARC_USED_BROFILE", false),
            expected("ARC_OPENED_BOARD", false),
            expected("ARC_PRODUCED_COMMIT", false),
            expected("NOTE_IN_THREAD", false),
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
                "THREAD_HAS_SESSION",
                "ARC_USED_BROFILE",
                "ARC_OPENED_BOARD",
                "ARC_PRODUCED_COMMIT",
                "NOTE_IN_THREAD",
            ],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Thread { thread_id } = r else {
            return None;
        };
        if let Some(state) = ctx.state() {
            if let Some(thread) = state
                .threads
                .read()
                .all()
                .into_iter()
                .find(|thread| thread.id == *thread_id)
            {
                if let Some(name) = &thread.name {
                    return Some(truncate_label(name));
                }
                return Some(truncate_label(&thread.topic));
            }
        }
        Some(truncate_label(thread_id))
    }
}
