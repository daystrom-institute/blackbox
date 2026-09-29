use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, ensure_type,
    truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

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
        if let Some(stores) = ctx.stores() {
            let threads = stores.threads.read();
            let thread = threads
                .all()
                .iter()
                .find(|thread| thread.id == *thread_id)
                .ok_or_else(|| anyhow::anyhow!("thread entity {thread_id} not found"))?;
            properties.insert("topic".into(), thread.topic.clone());
            properties.insert("project".into(), thread.project.clone());
            properties.insert("status".into(), thread.status.as_ref().to_string());
            if let Some(kind) = thread.kind {
                properties.insert("kind".into(), kind.as_ref().to_string());
            }
            if let Some(name) = &thread.name {
                properties.insert("name".into(), name.clone());
            }
            if let Some(handoff_doc) = &thread.handoff_doc {
                properties.insert("handoff_doc".into(), handoff_doc.clone());
            }
            properties.insert("notes_count".into(), thread.notes.len().to_string());
            properties.insert("sessions_count".into(), thread.sessions.len().to_string());
            properties.insert("edges_count".into(), thread.edges.len().to_string());
            if !thread.notes.is_empty() {
                properties.insert("inline_notes".into(), thread.notes.join("\n---\n"));
            }
            if !thread.sessions.is_empty() {
                properties.insert(
                    "sessions".into(),
                    thread
                        .sessions
                        .iter()
                        .map(|session| {
                            let label = session.name.as_deref().unwrap_or(&session.session_id);
                            format!("{} ({})", label, session.provider)
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            if !thread.edges.is_empty() {
                properties.insert(
                    "thread_edges".into(),
                    thread
                        .edges
                        .iter()
                        .map(|edge| {
                            let note = edge
                                .note
                                .as_deref()
                                .map(|note| format!(" — {note}"))
                                .unwrap_or_default();
                            format!(
                                "{} -> {}:{}{}",
                                edge.kind.as_ref(),
                                edge.target_type.as_ref(),
                                edge.target,
                                note
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Thread { thread_id } = r else {
            return None;
        };
        if let Some(stores) = ctx.stores() {
            if let Some(thread) = stores
                .threads
                .read()
                .all()
                .iter()
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
