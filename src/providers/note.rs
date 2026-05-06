use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct NoteProvider;

impl InspectableEntityProvider for NoteProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Note
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Note { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Note { note_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("note_id".into(), note_id.clone());
        if let Some(state) = ctx.state() {
            let notes = state.notes.read();
            let note = notes
                .all()
                .iter()
                .find(|note| note.id == *note_id)
                .ok_or_else(|| anyhow::anyhow!("note entity {note_id} not found"))?;
            properties.insert("kind".into(), format!("{:?}", note.kind));
            properties.insert("body".into(), note.body.clone());
            properties.insert("created_at".into(), note.created_at.clone());
            if let Some(task_id) = &note.task_id {
                properties.insert("task_id".into(), task_id.clone());
            }
            if let Some(session_id) = &note.session_id {
                properties.insert("session_id".into(), session_id.clone());
            }
            if let Some(thread_id) = &note.thread_id {
                properties.insert("thread_id".into(), thread_id.clone());
            }
            if let Some(project) = &note.project {
                properties.insert("project".into(), project.clone());
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "note_id",
                "kind",
                "body",
                "task_id",
                "session_id",
                "thread_id",
            ],
            &["NOTE_FROM_TASK", "NOTE_FROM_SESSION", "NOTE_IN_THREAD"],
            &["kind", "task_id", "session_id", "thread_id", "project"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("NOTE_FROM_TASK", false),
            expected("NOTE_FROM_SESSION", false),
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
            &["NOTE_FROM_TASK", "NOTE_FROM_SESSION", "NOTE_IN_THREAD"],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Note { note_id } = r else {
            return None;
        };
        if let Some(state) = ctx.state() {
            if let Some(note) = state
                .notes
                .read()
                .all()
                .iter()
                .find(|note| note.id == *note_id)
            {
                return Some(truncate_label(format!("{:?}: {}", note.kind, note.body)));
            }
        }
        Some(truncate_label(note_id))
    }
}
