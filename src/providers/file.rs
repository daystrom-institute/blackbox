use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderContext, empty_neighborhood_view, ensure_type, schema, truncate_label,
};
use crate::entity_ref::{EntityRef, EntityType};
use crate::projects::ProjectRecord;

pub struct FileProvider;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedFile {
    pub(crate) project_id: String,
    pub(crate) project_root: PathBuf,
    pub(crate) file_path: PathBuf,
    pub(crate) relative_path: String,
    pub(crate) content: Vec<u8>,
}

impl InspectableEntityProvider for FileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::File
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::File { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::File { path } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("path".into(), path.clone());
        if ctx.state().is_some() {
            let resolved = resolve_file(ctx, path)?;
            properties.insert("project_id".into(), resolved.project_id);
            properties.insert(
                "project_root".into(),
                resolved.project_root.to_string_lossy().into_owned(),
            );
            properties.insert(
                "file_path".into(),
                resolved.file_path.to_string_lossy().into_owned(),
            );
            properties.insert("relative_path".into(), resolved.relative_path);
            properties.insert("bytes".into(), resolved.content.len().to_string());
            properties.insert("content_preview".into(), preview(&resolved.content));
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "path",
                "project_id",
                "project_root",
                "file_path",
                "relative_path",
                "bytes",
                "content_preview",
            ],
            &["IN_PROJECT"],
            &["path", "project_id", "relative_path"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        Vec::new()
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        _full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        Vec::new()
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::File { path } = r else {
            return None;
        };
        Some(truncate_label(path))
    }
}

pub(crate) fn resolve_file(ctx: &ProviderContext<'_>, path: &str) -> Result<ResolvedFile> {
    let state = ctx
        .state()
        .ok_or_else(|| anyhow!("file refs require a registered project context"))?;
    let projects = state.projects.read().list();
    if projects.is_empty() {
        bail!("file refs require at least one registered project");
    }

    let raw = Path::new(path);
    let (project, file_path) = if raw.is_absolute() {
        resolve_absolute(raw, &projects)?
    } else {
        resolve_relative(raw, &projects)?
    };
    let project_root = canonical_project_root(&project)
        .with_context(|| format!("canonicalizing project root {}", project.canonical_path))?;
    let relative_path = file_path
        .strip_prefix(&project_root)
        .map(|path| path.to_string_lossy().into_owned())
        .with_context(|| {
            format!(
                "file {} is not under registered project {}",
                file_path.display(),
                project_root.display()
            )
        })?;
    let content =
        fs::read(&file_path).with_context(|| format!("reading file {}", file_path.display()))?;

    Ok(ResolvedFile {
        project_id: project.project_id,
        project_root,
        file_path,
        relative_path,
        content,
    })
}

fn resolve_relative(raw: &Path, projects: &[ProjectRecord]) -> Result<(ProjectRecord, PathBuf)> {
    let mut matches = Vec::new();
    for project in projects {
        let root = match canonical_project_root(project) {
            Ok(root) => root,
            Err(_) => continue,
        };
        let candidate = root.join(raw);
        if !candidate.exists() {
            continue;
        }
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("canonicalizing file {}", candidate.display()))?;
        if canonical.is_file() && canonical.starts_with(&root) {
            matches.push((project.clone(), canonical));
        }
    }

    match matches.len() {
        0 => bail!(
            "file ref `{}` did not match a file under any registered project",
            raw.display()
        ),
        1 => Ok(matches.remove(0)),
        _ => bail!(
            "file ref `{}` is ambiguous across registered projects",
            raw.display()
        ),
    }
}

fn resolve_absolute(raw: &Path, projects: &[ProjectRecord]) -> Result<(ProjectRecord, PathBuf)> {
    let canonical =
        fs::canonicalize(raw).with_context(|| format!("canonicalizing file {}", raw.display()))?;
    if !canonical.is_file() {
        bail!("file ref `{}` is not a file", raw.display());
    }

    let mut best: Option<(ProjectRecord, PathBuf, usize)> = None;
    for project in projects {
        let root = match canonical_project_root(project) {
            Ok(root) => root,
            Err(_) => continue,
        };
        if canonical.starts_with(&root) {
            let depth = root.components().count();
            if best
                .as_ref()
                .map(|(_, _, best_depth)| depth > *best_depth)
                .unwrap_or(true)
            {
                best = Some((project.clone(), root, depth));
            }
        }
    }

    best.map(|(project, _, _)| (project, canonical))
        .ok_or_else(|| {
            anyhow!(
                "file ref `{}` is outside all registered projects",
                raw.display()
            )
        })
}

fn canonical_project_root(project: &ProjectRecord) -> Result<PathBuf> {
    fs::canonicalize(&project.canonical_path)
        .with_context(|| format!("canonicalizing {}", project.canonical_path))
}

fn preview(content: &[u8]) -> String {
    let text = String::from_utf8_lossy(content);
    text.chars().take(400).collect()
}
