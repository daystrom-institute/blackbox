use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct TaskProvider;

impl InspectableEntityProvider for TaskProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Task
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Task { .. })
    }

    fn handles_virtual(&self) -> bool {
        true
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Task { task_id } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("task_id".into(), task_id.clone());
        properties.insert("virtual".into(), "true".into());
        if let Some(state) = ctx.state() {
            let task = state
                .task_store
                .read()
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("task entity {task_id} not found"))?;
            if let serde_json::Value::Object(obj) = crate::orchestration::task_result_json(&task) {
                for key in ["provider", "sessionId", "status", "elapsed", "result"] {
                    if let Some(value) = obj.get(key) {
                        properties.insert(key.to_string(), value_to_property(value));
                    }
                }
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["task_id", "virtual"],
            &["TASK_PRODUCED_NOTE", "NOTE_FROM_TASK"],
            &["task_id"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("TASK_PRODUCED_NOTE", false),
            expected("NOTE_FROM_TASK", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(full_neighborhood, &["TASK_PRODUCED_NOTE", "NOTE_FROM_TASK"])
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Task { task_id } = r else {
            return None;
        };
        if let Some(state) = ctx.state() {
            if let Some(task) = state.task_store.read().get(task_id) {
                if let serde_json::Value::Object(obj) =
                    crate::orchestration::task_result_json(&task)
                {
                    if let Some(status) = obj.get("status") {
                        return Some(truncate_label(format!(
                            "{task_id}: {}",
                            value_to_property(status)
                        )));
                    }
                }
            }
        }
        Some(truncate_label(task_id))
    }
}

fn value_to_property(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}
