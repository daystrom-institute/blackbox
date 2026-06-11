use std::collections::BTreeMap;

use anyhow::{Result, bail};

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, expected, next_hops, schema,
    truncate_label,
};
use crate::artifacts::ArtifactKind;
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct ArtifactProvider;

impl InspectableEntityProvider for ArtifactProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::Artifact
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::Artifact { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::Artifact {
            kind,
            name,
            version,
        } = r
        else {
            unreachable!();
        };
        let artifact_kind = parse_artifact_kind(kind)?;
        let mut properties = BTreeMap::new();
        properties.insert("kind".into(), kind.clone());
        properties.insert("name".into(), name.clone());
        if let Some(version) = version {
            properties.insert("version".into(), version.clone());
        }
        if let Some(stores) = ctx.stores() {
            let catalog = stores.artifacts.read();
            let meta = match version {
                Some(version) => catalog.metadata_for_version(artifact_kind, name, version)?,
                None => catalog.metadata_for(artifact_kind, name)?,
            }
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "artifact entity {kind}/{name}{} not found",
                    version
                        .as_ref()
                        .map(|v| format!("@{v}"))
                        .unwrap_or_default()
                )
            })?;
            properties.insert("version".into(), meta.version);
            properties.insert("source".into(), meta.source);
            properties.insert("installed_at".into(), meta.installed_at);
            properties.insert("active".into(), meta.active.to_string());
            if let Some(content_sha256) = meta.content_sha256 {
                properties.insert("content_sha256".into(), content_sha256);
            }
            if let Some(project_id) = meta.project_id {
                properties.insert("project_id".into(), project_id);
            }
            if let Some(project_path) = meta.project_path {
                properties.insert("project_path".into(), project_path);
            }
            if let Some(superseded_by) = meta.superseded_by {
                properties.insert("superseded_by".into(), superseded_by);
            }
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "kind",
                "name",
                "version",
                "source",
                "installed_at",
                "active",
                "content_sha256",
                "project_id",
                "project_path",
                "superseded_by",
            ],
            &["DERIVED_FROM", "SUPERSEDES"],
            &["kind", "name", "version", "active"],
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
        let EntityRef::Artifact {
            kind,
            name,
            version,
        } = r
        else {
            return None;
        };
        let suffix = version
            .as_ref()
            .map(|version| format!("@{version}"))
            .unwrap_or_default();
        Some(truncate_label(format!("{kind}/{name}{suffix}")))
    }
}

fn parse_artifact_kind(value: &str) -> Result<ArtifactKind> {
    Ok(match value {
        "workflow" => ArtifactKind::Workflow,
        "packet" => ArtifactKind::Packet,
        "brofile" => ArtifactKind::Brofile,
        "agent" => ArtifactKind::Agent,
        "atom" => ArtifactKind::Atom,
        "team" => ArtifactKind::Team,
        "cron" => ArtifactKind::Cron,
        _ => bail!("unknown artifact kind `{value}`"),
    })
}
