use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_indexing::project_graph_view::{
    ProjectGraphRead, ProjectGraphValidity, ProjectGraphViewEntry,
};
use bbox_knowledge::overlay::ProvisionalMode;
use bbox_project_graph::{GraphGeneration, ProjectGraphVertex, ValidationError};
use bbox_providers::providers::{
    EntityView, Neighborhood, ProjectGraphEntityResolver, empty_neighborhood_view,
};
use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_edge_index::edge_index::Edge;
use bro_core::WorkspaceId;
use serde::Serialize;

use crate::server::BlackboxServer;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphSummary {
    pub graph_id: String,
    pub status: &'static str,
    pub source: &'static str,
    pub checkout_id: Option<String>,
    pub vertex_count: usize,
    pub edge_count: usize,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphDescription {
    pub summary: GraphSummary,
    pub descriptor: Option<bbox_project_graph::GraphDescriptor>,
    pub schema: Option<bbox_project_graph::GraphSchema>,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphValidation {
    pub graph_id: String,
    pub valid: bool,
    pub source: &'static str,
    pub checkout_id: Option<String>,
    pub errors: Vec<ValidationError>,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedGraphVertex {
    pub canonical_ref: EntityRef,
    pub logical_ref: EntityRef,
    pub project_id: ProjectId,
    pub graph_id: String,
    pub vertex: ProjectGraphVertex,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
    pub provisional: bool,
    pub checkout_id: Option<WorkspaceId>,
    pub graph: std::sync::Arc<GraphGeneration>,
}

impl BlackboxServer {
    pub(crate) fn project_graph_list_domain(
        &self,
        project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphSummary>> {
        let (project_id, mode, own) = self.graph_read_context(project, provisional)?;
        let views = self.state.project_graph_views.read();
        let entries = match mode {
            ProvisionalMode::Published => views.list_published(&project_id),
            ProvisionalMode::Own => views.list_own(
                &project_id,
                own.as_ref().ok_or_else(|| anyhow!("own visibility requires checkout authority"))?,
            ),
            ProvisionalMode::All => {
                let mut entries = views.list_published(&project_id);
                for overlay in views.provisional_for_project(&project_id) {
                    entries.extend(overlay.graphs.values().filter_map(|value| match value {
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry) => Some(entry.clone()),
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Tombstone { .. } => None,
                    }));
                }
                entries
            }
        };
        Ok(entries.into_iter().map(summary).collect())
    }

    pub(crate) fn project_graph_describe_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphDescription>> {
        let entries = self.graph_entries(project, graph_id, provisional)?;
        Ok(entries.into_iter().map(|entry| {
            let summary = summary(entry.clone());
            GraphDescription {
                summary,
                descriptor: entry.graph().map(|graph| graph.descriptor.clone()),
                schema: entry.graph().map(|graph| graph.schema.clone()),
                generation: entry.generation,
            }
        }).collect())
    }

    pub(crate) fn project_graph_validate_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphValidation>> {
        Ok(self.graph_entries(project, graph_id, provisional)?.into_iter().map(|entry| {
            let (valid, errors) = match entry.validity.clone() {
                ProjectGraphValidity::Valid => (true, Vec::new()),
                ProjectGraphValidity::Invalid { errors } => (false, errors),
            };
            let source = source_label(&entry);
            GraphValidation {
                graph_id: entry.graph_id.clone(),
                valid,
                source,
                checkout_id: entry.generation.workspace_id.as_ref().map(ToString::to_string),
                errors,
                generation: entry.generation,
            }
        }).collect())
    }

    pub(crate) fn resolve_project_graph_vertex(
        &self,
        entity_ref: &EntityRef,
        provisional: Option<&str>,
    ) -> Result<ResolvedGraphVertex> {
        match entity_ref {
            EntityRef::ProjectGraphVertex { project_id, graph_id, vertex_id } => {
                self.resolve_published_form_vertex(project_id, graph_id, vertex_id, provisional)
            }
            EntityRef::ProvisionalProjectGraphVertex { scope_hash, checkout_id, graph_id, vertex_id } => {
                self.resolve_compound_vertex(scope_hash, checkout_id, graph_id, vertex_id)
            }
            _ => bail!("not a project graph vertex ref"),
        }
    }

    fn resolve_published_form_vertex(
        &self,
        project: &str,
        graph_id: &str,
        vertex_id: &str,
        provisional: Option<&str>,
    ) -> Result<ResolvedGraphVertex> {
        let (project_id, mode, own) = self.graph_read_context(Some(project), provisional)?;
        let views = self.state.project_graph_views.read();
        let scope_hash = views
            .published_view(&project_id)
            .map(|view| bbox_code_source::scope_hash(&view.scope))
            .ok_or_else(|| anyhow!("error.not_found: project has no accepted graph generation"))?;
        let mut candidates = Vec::new();
        match mode {
            ProvisionalMode::Published => push_read_candidate(&mut candidates, views.load_published(&project_id, graph_id))?,
            ProvisionalMode::Own => push_read_candidate(&mut candidates, views.load_own(&project_id, own.as_ref().unwrap(), graph_id))?,
            ProvisionalMode::All => {
                push_read_candidate(&mut candidates, views.load_published(&project_id, graph_id))?;
                for overlay in views.provisional_for_project(&project_id) {
                    if let Some(value) = overlay.graphs.get(graph_id)
                        && let bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry) = value
                    {
                        push_read_candidate(&mut candidates, read_entry(entry.clone()))?;
                    }
                }
            }
        }
        let mut resolved = candidates
            .into_iter()
            .filter_map(|entry| resolve_vertex(&project_id, &scope_hash, entry, vertex_id))
            .collect::<Vec<_>>();
        if resolved.is_empty() {
            bail!("error.not_found: project graph vertex was not found in {} visibility", mode_name(mode));
        }
        if mode == ProvisionalMode::All && resolved.len() > 1 {
            let refs = resolved.iter().map(|item| item.canonical_ref.render()).collect::<Vec<_>>().join(", ");
            bail!("error.project_graph_ambiguous: all visibility matched multiple generations: {refs}");
        }
        Ok(resolved.remove(0))
    }

    fn resolve_compound_vertex(&self, scope_hash: &str, checkout_id: &str, graph_id: &str, vertex_id: &str) -> Result<ResolvedGraphVertex> {
        let workspace_id = WorkspaceId::parse(checkout_id.to_string())?;
        let views = self.state.project_graph_views.read();
        for project in self
            .state
            .records_provider
            .records_snapshot()
            .records
            .iter()
        {
            let project_id = project.project_id.clone();
            let parsed = ProjectId::parse(project_id.clone())?;
            let Some(overlay) = views.provisional_overlay(&parsed, &workspace_id) else { continue; };
            if bbox_code_source::scope_hash(&overlay.scope) != scope_hash { continue; }
            let Some(value) = overlay.graphs.get(graph_id) else { bail!("error.not_found: provisional graph is not live"); };
            let entry = match value {
                bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry) => entry.clone(),
                bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Tombstone { .. } => bail!("error.not_found: provisional graph is tombstoned"),
            };
            return resolve_vertex(&parsed, scope_hash, entry, vertex_id)
                .ok_or_else(|| anyhow!("error.not_found: provisional graph vertex is not live"));
        }
        bail!("error.not_found: provisional graph scope or checkout is not live")
    }

    fn graph_entries(&self, project: &str, graph_id: &str, provisional: Option<&str>) -> Result<Vec<ProjectGraphViewEntry>> {
        let (project_id, mode, own) = self.graph_read_context(Some(project), provisional)?;
        let views = self.state.project_graph_views.read();
        let mut entries = Vec::new();
        match mode {
            ProvisionalMode::Published => push_read_candidate(&mut entries, views.load_published(&project_id, graph_id))?,
            ProvisionalMode::Own => push_read_candidate(&mut entries, views.load_own(&project_id, own.as_ref().unwrap(), graph_id))?,
            ProvisionalMode::All => {
                push_read_candidate(&mut entries, views.load_published(&project_id, graph_id))?;
                for overlay in views.provisional_for_project(&project_id) {
                    if let Some(bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry)) = overlay.graphs.get(graph_id) {
                        entries.push(entry.clone());
                    }
                }
            }
        }
        if entries.is_empty() { bail!("error.not_found: graph `{graph_id}` was not found in {} visibility", mode_name(mode)); }
        Ok(entries)
    }

    fn graph_read_context(&self, project: Option<&str>, provisional: Option<&str>) -> Result<(ProjectId, ProvisionalMode, Option<WorkspaceId>)> {
        let checkout = self.authoritative_session_checkout();
        let binding = self.authoritative_session_workspace_binding();
        let mode = ProvisionalMode::parse(provisional, checkout.is_some() || binding.is_some())?;
        let selected = match project {
            Some(raw) => self.validate_project_selection(raw)?,
            None => checkout.as_ref().map(|item| item.project_id.clone()).or_else(|| binding.as_ref().map(|item| item.project_id.clone())).ok_or_else(|| anyhow!("project is required without session checkout authority"))?,
        };
        let project_id = ProjectId::parse(selected)?;
        let own = binding.as_ref().filter(|item| item.project_id == project_id.as_str()).map(|item| item.workspace_id.clone()).or_else(|| checkout.as_ref().filter(|item| item.project_id == project_id.as_str()).and_then(|item| WorkspaceId::parse(item.checkout_id.clone()).ok()));
        if mode == ProvisionalMode::Own && own.is_none() { bail!("own visibility requires authoritative checkout authority for the selected project"); }
        Ok((project_id, mode, own))
    }
}

impl ProjectGraphEntityResolver for BlackboxServer {
    fn resolve_entity(&self, r: &EntityRef, provisional: Option<&str>) -> Result<EntityView> {
        let resolved = self.resolve_project_graph_vertex(r, provisional)?;
        let mut properties = BTreeMap::from([
            ("id".into(), resolved.vertex.id.clone()),
            ("type".into(), resolved.vertex.type_name.clone()),
            ("label".into(), resolved.vertex.label.clone()),
            ("project_id".into(), resolved.project_id.to_string()),
            ("graph_id".into(), resolved.graph_id.clone()),
            ("logical_ref".into(), resolved.logical_ref.render()),
            ("content_hash".into(), resolved.generation.content_hash.clone()),
            (
                "source".into(),
                if resolved.provisional {
                    "provisional"
                } else {
                    "published"
                }
                .into(),
            ),
            (
                "properties".into(),
                serde_json::to_string(&resolved.vertex.properties)?,
            ),
        ]);
        if let Some(checkout) = resolved.checkout_id.as_ref() {
            properties.insert("checkout_id".into(), checkout.to_string());
        }
        let (forward, reverse) = graph_neighborhood(&resolved);
        let edge = |source: EntityRef, kind: String, target: EntityRef| Edge {
            source,
            kind,
            target,
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
            project_id: Some(resolved.project_id.to_string()),
        };
        let canonical = resolved.canonical_ref.clone();
        let mut view = empty_neighborhood_view(&canonical, properties);
        view.neighborhood = Neighborhood {
            forward: forward
                .into_iter()
                .map(|(kind, target)| edge(canonical.clone(), kind, target))
                .collect(),
            reverse: reverse
                .into_iter()
                .map(|(kind, source)| edge(source, kind, canonical.clone()))
                .collect(),
        };
        Ok(view)
    }
}

fn read_entry(entry: ProjectGraphViewEntry) -> ProjectGraphRead {
    match entry.validity {
        ProjectGraphValidity::Valid => ProjectGraphRead::Valid(entry),
        ProjectGraphValidity::Invalid { .. } => ProjectGraphRead::Invalid(entry),
    }
}

fn push_read_candidate(entries: &mut Vec<ProjectGraphViewEntry>, read: ProjectGraphRead) -> Result<()> {
    match read {
        ProjectGraphRead::Missing | ProjectGraphRead::Tombstoned(_) => Ok(()),
        ProjectGraphRead::Valid(entry) | ProjectGraphRead::Invalid(entry) => { entries.push(entry); Ok(()) }
    }
}

fn resolve_vertex(
    project_id: &ProjectId,
    scope_hash: &str,
    entry: ProjectGraphViewEntry,
    vertex_id: &str,
) -> Option<ResolvedGraphVertex> {
    let graph = entry.graph()?.clone();
    let vertex = graph.vertices.get(vertex_id)?.clone();
    let logical_ref = EntityRef::ProjectGraphVertex { project_id: project_id.to_string(), graph_id: entry.graph_id.clone(), vertex_id: vertex_id.to_string() };
    let checkout_id = entry.generation.workspace_id.clone();
    let canonical_ref = match checkout_id.as_ref() {
        Some(checkout) => EntityRef::ProvisionalProjectGraphVertex {
            scope_hash: scope_hash.to_string(),
            checkout_id: checkout.to_string(),
            graph_id: entry.graph_id.clone(),
            vertex_id: vertex_id.to_string(),
        },
        None => logical_ref.clone(),
    };
    Some(ResolvedGraphVertex { canonical_ref, logical_ref, project_id: project_id.clone(), graph_id: entry.graph_id, vertex, generation: entry.generation, provisional: checkout_id.is_some(), checkout_id, graph })
}

pub(crate) fn graph_neighborhood(resolved: &ResolvedGraphVertex) -> (Vec<(String, EntityRef)>, Vec<(String, EntityRef)>) {
    let make_ref = |id: &str| match &resolved.canonical_ref {
        EntityRef::ProvisionalProjectGraphVertex { scope_hash, checkout_id, graph_id, .. } => EntityRef::ProvisionalProjectGraphVertex { scope_hash: scope_hash.clone(), checkout_id: checkout_id.clone(), graph_id: graph_id.clone(), vertex_id: id.to_string() },
        _ => EntityRef::ProjectGraphVertex { project_id: resolved.project_id.to_string(), graph_id: resolved.graph_id.clone(), vertex_id: id.to_string() },
    };
    let forward = resolved.graph.edges.iter().filter(|edge| edge.from == resolved.vertex.id).map(|edge| (edge.type_name.clone(), make_ref(&edge.to))).collect();
    let reverse = resolved.graph.edges.iter().filter(|edge| edge.to == resolved.vertex.id).map(|edge| (edge.type_name.clone(), make_ref(&edge.from))).collect();
    (forward, reverse)
}

fn summary(entry: ProjectGraphViewEntry) -> GraphSummary {
    let source = source_label(&entry);
    GraphSummary {
        graph_id: entry.graph_id.clone(),
        status: match entry.validity { ProjectGraphValidity::Valid => "valid", ProjectGraphValidity::Invalid { .. } => "invalid" },
        source,
        checkout_id: entry.generation.workspace_id.as_ref().map(ToString::to_string),
        vertex_count: entry.graph().map(|graph| graph.vertices.len()).unwrap_or(0),
        edge_count: entry.graph().map(|graph| graph.edges.len()).unwrap_or(0),
        content_hash: entry.generation.content_hash,
    }
}

fn source_label(entry: &ProjectGraphViewEntry) -> &'static str {
    if entry.generation.workspace_id.is_some() { "provisional" } else { "published" }
}

fn mode_name(mode: ProvisionalMode) -> &'static str {
    match mode { ProvisionalMode::Published => "published", ProvisionalMode::Own => "own", ProvisionalMode::All => "all" }
}
