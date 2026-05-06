use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    empty_neighborhood_view, ensure_type, expected, next_hops, schema, truncate_label,
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext,
};
use crate::entity_ref::{EntityRef, EntityType};

pub struct AgentProvider;

impl InspectableEntityProvider for AgentProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Agent
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Agent { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Agent { name, version } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("name".into(), name.clone());
        properties.insert("version".into(), version.to_string());
        if let Some(state) = ctx.state() {
            let catalog = state.artifacts.read();
            if let Some(v) = catalog
                .load_artifact_value(crate::artifacts::ArtifactKind::Agent, name)
                .ok()
                .flatten()
            {
                if let Some(manifest) = v.get("manifest").and_then(|m| m.as_object()) {
                    if let Some(desc) = manifest.get("description").and_then(|d| d.as_str()) {
                        properties.insert("description".into(), desc.to_string());
                    }
                    if let Some(bro) = manifest.get("brofile_ref").and_then(|b| b.as_str()) {
                        properties.insert("brofile_ref".into(), bro.to_string());
                    }
                    if let Some(wtu) = manifest.get("when_to_use").and_then(|w| w.as_array()) {
                        properties.insert(
                            "when_to_use".into(),
                            wtu.iter()
                                .filter_map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join("; "),
                        );
                    }
                }
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "name",
                "version",
                "description",
                "brofile_ref",
                "when_to_use",
            ],
            &["DERIVED_FROM", "SUPERSEDES"],
            &["name", "version"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        vec![
            expected("DERIVED_FROM", false),
            expected("SUPERSEDES", false),
        ]
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        next_hops(full_neighborhood, &["DERIVED_FROM", "SUPERSEDES"])
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::Agent { name, version } = r else {
            return None;
        };
        Some(truncate_label(format!("{name}@v{version}")))
    }
}
