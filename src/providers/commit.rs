use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, base_view, ensure_type, expected, next_hops, schema, truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};

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
        if let Some(state) = ctx.state() {
            let indexed = state
                .idx
                .read()
                .entity_properties(&r.to_string())?
                .ok_or_else(|| anyhow::anyhow!("commit entity {r} not found"))?;
            properties.extend(indexed);
        }
        Ok(base_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &["repo_id", "sha", "subject", "author"],
            &[
                "COMMIT_PARENT",
                "COMMIT_PRODUCED_BY_ARC",
                "COMMIT_TOUCHED_FILE",
            ],
            &["repo_id", "sha"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("COMMIT_PARENT", false),
            expected("COMMIT_PRODUCED_BY_ARC", false),
            expected("COMMIT_TOUCHED_FILE", false),
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
                "COMMIT_PARENT",
                "COMMIT_PRODUCED_BY_ARC",
                "COMMIT_TOUCHED_FILE",
            ],
        )
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Commit { sha, .. } = r else {
            return None;
        };
        let short = sha.chars().take(7).collect::<String>();
        if let Some(state) = ctx.state() {
            if let Ok(Some(properties)) = state.idx.read().entity_properties(&r.to_string()) {
                if let Some(preview) = properties.get("content_preview") {
                    let subject = preview.lines().next().unwrap_or(preview);
                    return Some(truncate_label(format!("{short} {subject}")));
                }
            }
        }
        Some(truncate_label(short))
    }
}
