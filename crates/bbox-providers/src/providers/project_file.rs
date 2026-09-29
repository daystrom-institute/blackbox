use std::collections::BTreeMap;

use anyhow::Result;

use super::{
    EntityView, InspectableEntityProvider, ProviderContext, empty_neighborhood_view, truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};

pub struct ProjectFileProvider;
pub struct ProjectFileV2Provider;

impl InspectableEntityProvider for ProjectFileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProjectFile
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProjectFile { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        project_file_entity(ctx, r)
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        project_file_label(ctx, r)
    }
}

impl InspectableEntityProvider for ProjectFileV2Provider {
    fn entity_type(&self) -> EntityType {
        EntityType::ProjectFileV2
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::ProjectFileV2 { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        project_file_entity(ctx, r)
    }

    fn compact_label(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        project_file_label(ctx, r)
    }
}

fn project_file_parts(r: &EntityRef) -> Option<(&str, Option<&str>, &str, &str, u32)> {
    match r {
        EntityRef::ProjectFile {
            project_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        } => Some((project_id, None, rel_path_hash, chunk_hash, *occurrence_idx)),
        EntityRef::ProjectFileV2 {
            project_id,
            snapshot_id,
            rel_path_hash,
            chunk_hash,
            occurrence_idx,
        } => Some((
            project_id,
            Some(snapshot_id),
            rel_path_hash,
            chunk_hash,
            *occurrence_idx,
        )),
        _ => None,
    }
}

fn project_file_entity(ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
    let Some((project_id, snapshot_id, rel_path_hash, chunk_hash, occurrence_idx)) =
        project_file_parts(r)
    else {
        unreachable!();
    };
    let mut properties = BTreeMap::new();
    properties.insert("project_id".into(), project_id.to_string());
    if let Some(snapshot_id) = snapshot_id {
        properties.insert("snapshot_id".into(), snapshot_id.to_string());
    }
    properties.insert("rel_path_hash".into(), rel_path_hash.to_string());
    properties.insert("chunk_hash".into(), chunk_hash.to_string());
    properties.insert("occurrence_idx".into(), occurrence_idx.to_string());
    if ctx.stores().is_some() {
        let indexed = ctx
            .indexed_entity_properties(&r.to_string())?
            .ok_or_else(|| anyhow::anyhow!("project file entity {r} not found"))?;
        properties.extend(indexed);
    }
    Ok(empty_neighborhood_view(r, properties))
}

fn project_file_label(ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
    let (_, snapshot_id, rel_path_hash, _, occurrence_idx) = project_file_parts(r)?;
    if ctx.stores().is_some() {
        if let Ok(Some(properties)) = ctx.indexed_entity_properties(&r.to_string()) {
            // P3-E: the compact graph label is the relative path (or the
            // rendered `display_path` when the response boundary produced one),
            // never a host absolute path.
            if let Some(path) = properties
                .get("display_path")
                .or_else(|| properties.get("relative_path"))
                .or_else(|| properties.get("file_path"))
            {
                return Some(truncate_label(path));
            }
            if let Some(preview) = properties.get("content_preview") {
                return Some(truncate_label(preview));
            }
        }
    }
    let suffix = snapshot_id.map(|id| format!("@{id}")).unwrap_or_default();
    Some(truncate_label(format!(
        "{rel_path_hash}#{occurrence_idx}{suffix}"
    )))
}
