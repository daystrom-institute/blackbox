use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_edge_index::edge_index::Edge;
use bbox_indexing::project_graph_view::{
    ProjectGraphRead, ProjectGraphValidity, ProjectGraphViewEntry,
};
use bbox_knowledge::overlay::ProvisionalMode;
use bbox_project_graph::{
    EvidenceBinding, EvidenceEndpointObservation, EvidenceEndpointStatus, GraphAuthority,
    GraphGeneration, ProjectGraphVertex, ValidationError,
};
use bbox_providers::providers::{
    EntityView, Neighborhood, ProjectGraphEntityResolver, empty_neighborhood_view,
};
use bro_core::WorkspaceId;
use serde::Serialize;

use crate::server::BlackboxServer;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphSummary {
    pub graph_id: String,
    pub status: &'static str,
    pub source: &'static str,
    pub checkout_id: Option<String>,
    /// Reflected counts: authored rows plus schema-as-data vertices/edges
    /// (vertex/edge type definitions) plus `meta:INSTANCE_OF` edges. Kept
    /// for compatibility with existing callers that read the full
    /// materialized graph.
    pub vertex_count: usize,
    pub edge_count: usize,
    /// Authored counts: rows sourced directly from vertices.jsonl and
    /// edges.jsonl, before schema-as-data reflection. This is what an
    /// author comparing against their source files expects to see.
    pub authored_vertex_count: usize,
    pub authored_edge_count: usize,
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
        let mut entries = match mode {
            ProvisionalMode::Published => views.list_published(&project_id),
            ProvisionalMode::Own => views.list_own(
                &project_id,
                own.as_ref()
                    .ok_or_else(|| anyhow!("own visibility requires checkout authority"))?,
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
        // Connector-managed source graphs are read-only projections accepted
        // by the source projection store, not checkout state, so they are
        // visible under every visibility policy rather than gated by a
        // provisional opt-in.
        entries.extend(views.list_connector(&project_id));
        Ok(entries.into_iter().map(summary).collect())
    }

    pub(crate) fn project_graph_describe_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphDescription>> {
        let entries = self.graph_entries(project, graph_id, provisional)?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                let summary = summary(entry.clone());
                GraphDescription {
                    summary,
                    descriptor: entry.graph().map(|graph| graph.descriptor.clone()),
                    schema: entry.graph().map(|graph| graph.schema.clone()),
                    generation: entry.generation,
                }
            })
            .collect())
    }

    pub(crate) fn project_graph_validate_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphValidation>> {
        Ok(self
            .graph_entries(project, graph_id, provisional)?
            .into_iter()
            .map(|entry| {
                let (valid, errors) = match entry.validity.clone() {
                    ProjectGraphValidity::Valid => (true, Vec::new()),
                    ProjectGraphValidity::Invalid { errors } => (false, errors),
                };
                let source = source_label(&entry);
                GraphValidation {
                    graph_id: entry.graph_id.clone(),
                    valid,
                    source,
                    checkout_id: entry
                        .generation
                        .workspace_id
                        .as_ref()
                        .map(ToString::to_string),
                    errors,
                    generation: entry.generation,
                }
            })
            .collect())
    }

    /// Snapshot of which graph lanes the word-search authority admits
    /// (unified-retrieval design 5.1). Taken under ONE read lock of the view
    /// catalog before a search starts, so the answer cannot change mid-query
    /// and the lock is never held across the Tantivy call. Lanes whose policy
    /// disables text retrieval, and never-indexable local-scratch sources,
    /// land in `disabled_graph_lanes`; per-lane excluded vertex types ride
    /// along as the query-time re-check of the index-time gate.
    pub(crate) fn graph_word_policy_snapshot(
        &self,
    ) -> bbox_indexing::index::GraphWordPolicySnapshot {
        use bbox_project_graph::GraphSource;

        let mut snapshot = bbox_indexing::index::GraphWordPolicySnapshot::default();
        let views = self.state.project_graph_views.read();
        for (project_id, view) in views.iter_published() {
            for entry in view.graphs.values() {
                let Some(graph) = entry.graph() else {
                    continue;
                };
                let lane = (project_id.as_str().to_string(), entry.graph_id.clone());
                let never_indexable = matches!(graph.key.source, GraphSource::LocalScratch);
                if never_indexable || !graph.schema.index_policy.text_retrieval_enabled {
                    snapshot.disabled_graph_lanes.insert(lane);
                } else if !graph
                    .schema
                    .index_policy
                    .retrieval_excluded_types
                    .is_empty()
                {
                    snapshot.excluded_vertex_types.insert(
                        lane,
                        graph.schema.index_policy.retrieval_excluded_types.clone(),
                    );
                }
            }
        }
        snapshot
    }

    pub(crate) fn resolve_project_graph_vertex(
        &self,
        entity_ref: &EntityRef,
        provisional: Option<&str>,
    ) -> Result<ResolvedGraphVertex> {
        match entity_ref {
            EntityRef::ProjectGraphVertex {
                project_id,
                graph_id,
                vertex_id,
            } => self.resolve_published_form_vertex(project_id, graph_id, vertex_id, provisional),
            EntityRef::ProvisionalProjectGraphVertex {
                scope_hash,
                checkout_id,
                graph_id,
                vertex_id,
            } => self.resolve_compound_vertex(scope_hash, checkout_id, graph_id, vertex_id),
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
        // A project can hold connector-managed graphs before it has ever
        // published one, so the published scope is optional here. It is only
        // needed to render a provisional compound ref, and a connector graph
        // never has one.
        let scope_hash = views
            .published_view(&project_id)
            .map(|view| bbox_code_source::scope_hash(&view.scope));
        let mut candidates = Vec::new();
        match mode {
            ProvisionalMode::Published => {
                push_read_candidate(&mut candidates, views.load_published(&project_id, graph_id))?
            }
            ProvisionalMode::Own => push_read_candidate(
                &mut candidates,
                views.load_own(&project_id, own.as_ref().unwrap(), graph_id),
            )?,
            ProvisionalMode::All => {
                push_read_candidate(&mut candidates, views.load_published(&project_id, graph_id))?;
                for overlay in views.provisional_for_project(&project_id) {
                    if let Some(value) = overlay.graphs.get(graph_id)
                        && let bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(
                            entry,
                        ) = value
                    {
                        push_read_candidate(&mut candidates, read_entry(entry.clone()))?;
                    }
                }
            }
        }
        if candidates.is_empty() {
            push_read_candidate(&mut candidates, views.load_connector(&project_id, graph_id))?;
        }
        if candidates.is_empty() && scope_hash.is_none() {
            bail!("error.not_found: project has no accepted graph generation");
        }
        let scope_hash = scope_hash.unwrap_or_default();
        let mut resolved = candidates
            .into_iter()
            .filter_map(|entry| resolve_vertex(&project_id, &scope_hash, entry, vertex_id))
            .collect::<Vec<_>>();
        if resolved.is_empty() {
            bail!(
                "error.not_found: project graph vertex was not found in {} visibility",
                mode_name(mode)
            );
        }
        if mode == ProvisionalMode::All && resolved.len() > 1 {
            let refs = resolved
                .iter()
                .map(|item| item.canonical_ref.render())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "error.project_graph_ambiguous: all visibility matched multiple generations: {refs}"
            );
        }
        Ok(resolved.remove(0))
    }

    fn resolve_compound_vertex(
        &self,
        scope_hash: &str,
        checkout_id: &str,
        graph_id: &str,
        vertex_id: &str,
    ) -> Result<ResolvedGraphVertex> {
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
            let Some(overlay) = views.provisional_overlay(&parsed, &workspace_id) else {
                continue;
            };
            if bbox_code_source::scope_hash(&overlay.scope) != scope_hash {
                continue;
            }
            let Some(value) = overlay.graphs.get(graph_id) else {
                bail!("error.not_found: provisional graph is not live");
            };
            let entry = match value {
                bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry) => {
                    entry.clone()
                }
                bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Tombstone {
                    ..
                } => bail!("error.not_found: provisional graph is tombstoned"),
            };
            return resolve_vertex(&parsed, scope_hash, entry, vertex_id)
                .ok_or_else(|| anyhow!("error.not_found: provisional graph vertex is not live"));
        }
        bail!("error.not_found: provisional graph scope or checkout is not live")
    }

    fn graph_entries(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<ProjectGraphViewEntry>> {
        let (project_id, mode, own) = self.graph_read_context(Some(project), provisional)?;
        let views = self.state.project_graph_views.read();
        let mut entries = Vec::new();
        match mode {
            ProvisionalMode::Published => {
                push_read_candidate(&mut entries, views.load_published(&project_id, graph_id))?
            }
            ProvisionalMode::Own => push_read_candidate(
                &mut entries,
                views.load_own(&project_id, own.as_ref().unwrap(), graph_id),
            )?,
            ProvisionalMode::All => {
                push_read_candidate(&mut entries, views.load_published(&project_id, graph_id))?;
                for overlay in views.provisional_for_project(&project_id) {
                    if let Some(
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(entry),
                    ) = overlay.graphs.get(graph_id)
                    {
                        entries.push(entry.clone());
                    }
                }
            }
        }
        if entries.is_empty() {
            push_read_candidate(&mut entries, views.load_connector(&project_id, graph_id))?;
        }
        if entries.is_empty() {
            bail!(
                "error.not_found: graph `{graph_id}` was not found in {} visibility",
                mode_name(mode)
            );
        }
        Ok(entries)
    }

    /// Which project's binding set governs a ref.
    ///
    /// A project-scoped ref names its own project. Anything else (a knowledge
    /// entry, a thread, a commit) has no project of its own, so it borrows the
    /// caller's session project; without one there is no authorized scope and
    /// therefore no bindings to show.
    fn evidence_project_for(
        &self,
        entity: &EntityRef,
        provisional: Option<&str>,
    ) -> Option<ProjectId> {
        if let Some(project) = bbox_project_graph::entity_project_scope(entity) {
            return ProjectId::parse(project.to_string()).ok();
        }
        self.graph_read_context(None, provisional)
            .ok()
            .map(|(project_id, _, _)| project_id)
    }

    /// The evidence edges touching `entity`, split by direction.
    ///
    /// Endpoints this layer can observe (project graph vertices, and any ref
    /// outside the binding's authorized scope) are scored here. Everything
    /// else is left `unresolved` for the read plane to refine, because
    /// deciding whether a project file or knowledge entry is live needs the
    /// provider registry, which lives a layer up.
    fn evidence_neighborhood(
        &self,
        project_id: &ProjectId,
        entity: &EntityRef,
        provisional: Option<&str>,
    ) -> (Vec<Edge>, Vec<Edge>) {
        // The read context is resolved BEFORE the view guard is taken and then
        // threaded down. Re-deriving it under the lock would re-enter the same
        // RwLock through validate_project_selection, which deadlocks the
        // moment a writer is queued between the two acquisitions.
        let Ok((_, mode, own)) = self.graph_read_context(Some(project_id.as_str()), provisional)
        else {
            return (Vec::new(), Vec::new());
        };
        let views = self.state.project_graph_views.read();
        let sets = match mode {
            ProvisionalMode::Published => vec![views.evidence_published(project_id)],
            ProvisionalMode::Own => match own.as_ref() {
                Some(workspace) => vec![views.evidence_own(project_id, workspace)],
                None => vec![views.evidence_published(project_id)],
            },
            ProvisionalMode::All => views.evidence_all(project_id),
        };
        let mut forward = Vec::new();
        let mut reverse = Vec::new();
        let mut seen = BTreeSet::new();
        for set in &sets {
            for binding in set.forward(entity) {
                if seen.insert((binding.binding_id.clone(), true)) {
                    forward.push(evidence_edge(binding, &views, mode, own.as_ref()));
                }
            }
            for binding in set.reverse(entity) {
                if seen.insert((binding.binding_id.clone(), false)) {
                    reverse.push(evidence_edge(binding, &views, mode, own.as_ref()));
                }
            }
        }
        (forward, reverse)
    }

    fn graph_read_context(
        &self,
        project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<(ProjectId, ProvisionalMode, Option<WorkspaceId>)> {
        let checkout = self.authoritative_session_checkout();
        let binding = self.authoritative_session_workspace_binding();
        let mode = ProvisionalMode::parse(provisional, checkout.is_some() || binding.is_some())?;
        let selected = match project {
            Some(raw) => self.validate_project_selection(raw)?,
            None => checkout
                .as_ref()
                .map(|item| item.project_id.clone())
                .or_else(|| binding.as_ref().map(|item| item.project_id.clone()))
                .ok_or_else(|| anyhow!("project is required without session checkout authority"))?,
        };
        let project_id = ProjectId::parse(selected)?;
        let own = binding
            .as_ref()
            .filter(|item| item.project_id == project_id.as_str())
            .map(|item| item.workspace_id.clone())
            .or_else(|| {
                checkout
                    .as_ref()
                    .filter(|item| item.project_id == project_id.as_str())
                    .and_then(|item| WorkspaceId::parse(item.checkout_id.clone()).ok())
            });
        if mode == ProvisionalMode::Own && own.is_none() {
            bail!(
                "own visibility requires authoritative checkout authority for the selected project"
            );
        }
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
            (
                "content_hash".into(),
                resolved.generation.content_hash.clone(),
            ),
            ("source".into(), resolved_source_label(&resolved).into()),
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
        // Evidence edges are a separate family on the same neighborhood: they
        // are tenant assertions, not graph facts, so they never enter
        // graph_neighborhood and never round-trip through the graph documents.
        // Bindings reference the LOGICAL vertex ref, so a provisional caller
        // is matched on logical_ref while the edges it gets back are stated in
        // whatever canonical form it asked with.
        let (evidence_forward, evidence_reverse) =
            self.evidence_neighborhood(&resolved.project_id, &resolved.logical_ref, provisional);
        view.neighborhood
            .forward
            .extend(evidence_forward.into_iter().map(|mut edge| {
                edge.source = canonical.clone();
                edge
            }));
        view.neighborhood
            .reverse
            .extend(evidence_reverse.into_iter().map(|mut edge| {
                edge.target = canonical.clone();
                edge
            }));
        Ok(view)
    }

    fn evidence_edges(&self, r: &EntityRef, provisional: Option<&str>) -> Vec<Edge> {
        let Some(project_id) = self.evidence_project_for(r, provisional) else {
            return Vec::new();
        };
        let (forward, reverse) = self.evidence_neighborhood(&project_id, r, provisional);
        forward.into_iter().chain(reverse).collect()
    }
}

/// Builds one evidence edge with both endpoints scored.
///
/// A free function, not a method: it runs while the view guard is held, and a
/// method could reach back through `self` into the same lock.
fn evidence_edge(
    binding: &EvidenceBinding,
    views: &bbox_indexing::project_graph_view::ProjectGraphViewCatalog,
    mode: ProvisionalMode,
    own: Option<&WorkspaceId>,
) -> Edge {
    let source_status = observe_endpoint(
        binding,
        &binding.source,
        binding.source_generation,
        views,
        mode,
        own,
    );
    let target_status = observe_endpoint(
        binding,
        &binding.target,
        binding.target_generation,
        views,
        mode,
        own,
    );
    Edge {
        source: binding.source.clone(),
        kind: binding.kind.clone(),
        target: binding.target.clone(),
        provenance: EdgeProvenance::Explicit,
        confidence: EdgeConfidence::Exact,
        metadata: bbox_project_graph::binding_metadata(binding, source_status, target_status),
        project_id: Some(binding.project_id.clone()),
    }
}

/// Scores one endpoint against the generation the binding recorded.
///
/// Only project graph vertices are observable here. Anything else is left
/// `unresolved` for the read plane, which holds the provider registry and can
/// settle it by trying to load the entity.
fn observe_endpoint(
    binding: &EvidenceBinding,
    endpoint: &EntityRef,
    expected_generation: Option<u64>,
    views: &bbox_indexing::project_graph_view::ProjectGraphViewCatalog,
    mode: ProvisionalMode,
    own: Option<&WorkspaceId>,
) -> EvidenceEndpointStatus {
    // Scope first: an endpoint in another project is unauthorized whatever its
    // liveness, and path traversal must never cross it.
    if bbox_project_graph::entity_project_scope(endpoint)
        .is_some_and(|scope| scope != binding.project_id)
    {
        return bbox_project_graph::resolve_endpoint_status(
            EvidenceEndpointObservation::OutOfScope,
            expected_generation,
        );
    }
    let EntityRef::ProjectGraphVertex {
        project_id,
        graph_id,
        vertex_id,
    } = endpoint
    else {
        return bbox_project_graph::resolve_endpoint_status(
            EvidenceEndpointObservation::Unresolvable,
            expected_generation,
        );
    };
    let Ok(parsed) = ProjectId::parse(project_id.clone()) else {
        return bbox_project_graph::resolve_endpoint_status(
            EvidenceEndpointObservation::Absent,
            expected_generation,
        );
    };
    let read = match (mode, own) {
        (ProvisionalMode::Own, Some(workspace)) => views.load_own(&parsed, workspace, graph_id),
        _ => views.load_published(&parsed, graph_id),
    };
    let observation = match read {
        ProjectGraphRead::Valid(entry) | ProjectGraphRead::Invalid(entry) => match entry.graph() {
            Some(graph) if graph.vertices.contains_key(vertex_id) => {
                EvidenceEndpointObservation::Present {
                    generation: Some(graph.descriptor.generation),
                }
            }
            _ => EvidenceEndpointObservation::Absent,
        },
        ProjectGraphRead::Missing | ProjectGraphRead::Tombstoned(_) => {
            EvidenceEndpointObservation::Absent
        }
    };
    bbox_project_graph::resolve_endpoint_status(observation, expected_generation)
}

fn read_entry(entry: ProjectGraphViewEntry) -> ProjectGraphRead {
    match entry.validity {
        ProjectGraphValidity::Valid => ProjectGraphRead::Valid(entry),
        ProjectGraphValidity::Invalid { .. } => ProjectGraphRead::Invalid(entry),
    }
}

fn push_read_candidate(
    entries: &mut Vec<ProjectGraphViewEntry>,
    read: ProjectGraphRead,
) -> Result<()> {
    match read {
        ProjectGraphRead::Missing | ProjectGraphRead::Tombstoned(_) => Ok(()),
        ProjectGraphRead::Valid(entry) | ProjectGraphRead::Invalid(entry) => {
            entries.push(entry);
            Ok(())
        }
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
    let logical_ref = EntityRef::ProjectGraphVertex {
        project_id: project_id.to_string(),
        graph_id: entry.graph_id.clone(),
        vertex_id: vertex_id.to_string(),
    };
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
    Some(ResolvedGraphVertex {
        canonical_ref,
        logical_ref,
        project_id: project_id.clone(),
        graph_id: entry.graph_id,
        vertex,
        generation: entry.generation,
        provisional: checkout_id.is_some(),
        checkout_id,
        graph,
    })
}

pub(crate) fn graph_neighborhood(
    resolved: &ResolvedGraphVertex,
) -> (Vec<(String, EntityRef)>, Vec<(String, EntityRef)>) {
    let make_ref = |id: &str| match &resolved.canonical_ref {
        EntityRef::ProvisionalProjectGraphVertex {
            scope_hash,
            checkout_id,
            graph_id,
            ..
        } => EntityRef::ProvisionalProjectGraphVertex {
            scope_hash: scope_hash.clone(),
            checkout_id: checkout_id.clone(),
            graph_id: graph_id.clone(),
            vertex_id: id.to_string(),
        },
        _ => EntityRef::ProjectGraphVertex {
            project_id: resolved.project_id.to_string(),
            graph_id: resolved.graph_id.clone(),
            vertex_id: id.to_string(),
        },
    };
    let forward = resolved
        .graph
        .edges
        .iter()
        .filter(|edge| edge.from == resolved.vertex.id)
        .map(|edge| (edge.type_name.clone(), make_ref(&edge.to)))
        .collect();
    let reverse = resolved
        .graph
        .edges
        .iter()
        .filter(|edge| edge.to == resolved.vertex.id)
        .map(|edge| (edge.type_name.clone(), make_ref(&edge.from)))
        .collect();
    (forward, reverse)
}

fn summary(entry: ProjectGraphViewEntry) -> GraphSummary {
    let source = source_label(&entry);
    GraphSummary {
        graph_id: entry.graph_id.clone(),
        status: match entry.validity {
            ProjectGraphValidity::Valid => "valid",
            ProjectGraphValidity::Invalid { .. } => "invalid",
        },
        source,
        checkout_id: entry
            .generation
            .workspace_id
            .as_ref()
            .map(ToString::to_string),
        vertex_count: entry.graph().map(|graph| graph.vertices.len()).unwrap_or(0),
        edge_count: entry.graph().map(|graph| graph.edges.len()).unwrap_or(0),
        authored_vertex_count: entry
            .graph()
            .map(|graph| graph.authored_vertex_count)
            .unwrap_or(0),
        authored_edge_count: entry
            .graph()
            .map(|graph| graph.authored_edge_count)
            .unwrap_or(0),
        content_hash: entry.generation.content_hash,
    }
}

/// The read-plane authority label for one graph. Three values, one per
/// authority plane: `published` for accepted project-authored facts,
/// `provisional` for a checkout's own uncommitted graph, and `connector` for a
/// connector-managed source projection, which no checkout lane can author.
fn source_label(entry: &ProjectGraphViewEntry) -> &'static str {
    match entry.graph() {
        Some(graph) if graph.descriptor.authority == GraphAuthority::Connector => "connector",
        _ if entry.generation.workspace_id.is_some() => "provisional",
        _ => "published",
    }
}

fn resolved_source_label(resolved: &ResolvedGraphVertex) -> &'static str {
    if resolved.graph.descriptor.authority == GraphAuthority::Connector {
        "connector"
    } else if resolved.provisional {
        "provisional"
    } else {
        "published"
    }
}

fn mode_name(mode: ProvisionalMode) -> &'static str {
    match mode {
        ProvisionalMode::Published => "published",
        ProvisionalMode::Own => "own",
        ProvisionalMode::All => "all",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity;
    use bbox_project_graph::{
        DESCRIPTOR_VERSION, GraphDescriptor, GraphKey, GraphSchema, GraphScope, GraphSource,
        RetentionPolicy, build_generation,
    };

    fn schema() -> GraphSchema {
        serde_json::from_str(
            r#"{"version":1,"namespace":"dataset","vertex_types":{"dataset:Asset":{"required":["remote_id"],"properties":{"remote_id":"string"}}},"edge_types":[]}"#,
        )
        .unwrap()
    }

    fn generation(authority: GraphAuthority, source: GraphSource) -> GraphGeneration {
        let connector = authority == GraphAuthority::Connector;
        build_generation(
            GraphKey {
                scope_id: "scope".into(),
                graph_id: "source-assets".into(),
                source,
            },
            GraphDescriptor {
                descriptor_version: DESCRIPTOR_VERSION,
                scope: GraphScope::Project,
                graph_id: "source-assets".into(),
                authority,
                schema_id: "dataset:schema".into(),
                schema_version: 1,
                projection_version: connector.then(|| "dataset-v1".to_string()),
                source_connector: connector.then(|| "synthetic-api".to_string()),
                retention_policy: if connector {
                    RetentionPolicy::ConnectorManaged
                } else {
                    RetentionPolicy::ProjectOwned
                },
                generation: 1,
            },
            schema(),
            Vec::new(),
            Vec::new(),
            "d".repeat(64),
            std::path::PathBuf::from("/store"),
        )
    }

    fn identity(workspace: Option<&str>) -> ProjectGraphGenerationIdentity {
        ProjectGraphGenerationIdentity {
            accepted_generation: "1".into(),
            accepted_commit: String::new(),
            source_generation: None,
            workspace_id: workspace.map(|id| WorkspaceId::parse(id.to_string()).unwrap()),
            content_hash: "d".repeat(64),
        }
    }

    /// The read plane names three authority planes, and connector is distinct
    /// from both project-authored ones.
    #[test]
    fn the_source_label_names_three_distinct_authority_planes() {
        let connector = ProjectGraphViewEntry::valid(
            "source-assets".into(),
            identity(None),
            generation(GraphAuthority::Connector, GraphSource::ConnectorManaged),
        );
        assert_eq!(source_label(&connector), "connector");
        assert_eq!(summary(connector.clone()).source, "connector");
        assert!(
            summary(connector).checkout_id.is_none(),
            "a connector projection is never checkout scoped"
        );

        let published = ProjectGraphViewEntry::valid(
            "records".into(),
            identity(None),
            generation(GraphAuthority::Project, GraphSource::Committed),
        );
        assert_eq!(source_label(&published), "published");

        let provisional = ProjectGraphViewEntry::valid(
            "records".into(),
            identity(Some("0123456789abcdef0123456789abcdef")),
            generation(GraphAuthority::Project, GraphSource::Committed),
        );
        assert_eq!(source_label(&provisional), "provisional");
    }
}
