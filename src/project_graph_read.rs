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
    GraphGeneration, HintDirection, ProjectGraphVertex,
};
use bbox_providers::providers::{
    EntityView, Neighborhood, NextHopDirection, NextHopHint as ProviderNextHopHint,
    ProjectGraphEntityResolver, empty_neighborhood_view,
};
use bro_core::WorkspaceId;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::server::BlackboxServer;

/// Detail selector for `bbox_project_graph_describe`. `summary` is the
/// compact default; `schema` and `descriptor` name exact body reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphDescribeDetail {
    Summary,
    Schema,
    Descriptor,
}

impl GraphDescribeDetail {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("summary") => Ok(Self::Summary),
            Some("schema") => Ok(Self::Schema),
            Some("descriptor") => Ok(Self::Descriptor),
            Some(other) => bail!(
                "error.bad_input: invalid detail {other:?}; expected summary, schema, or descriptor"
            ),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Schema => "schema",
            Self::Descriptor => "descriptor",
        }
    }
}

/// Detail selector for `bbox_project_graph_validate`. `summary` pages the
/// error rows; `errors` recovers the complete array as exact JSON bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphValidateDetail {
    Summary,
    Errors,
}

impl GraphValidateDetail {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self> {
        match raw {
            None | Some("summary") => Ok(Self::Summary),
            Some("errors") => Ok(Self::Errors),
            Some(other) => {
                bail!("error.bad_input: invalid detail {other:?}; expected summary or errors")
            }
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Errors => "errors",
        }
    }
}

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
    /// Compact schema identity. `None` when the entry was accepted invalid
    /// and carries no parsed graph payload; exact bytes live behind the
    /// detail reads instead of this summary.
    pub schema: Option<GraphSchemaSummary>,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
    pub retrieval: GraphRetrievalParticipation,
}

/// Compact schema identity for the default describe summary: enough to name
/// and count the schema without embedding it. Exact schema and descriptor
/// JSON stay recoverable through the detail body reads.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphSchemaSummary {
    pub schema_id: String,
    pub schema_version: u64,
    pub vertex_type_count: usize,
    pub edge_type_count: usize,
}

/// Retrieval participation for one graph lane (unified-retrieval 6.5): the
/// surface that answers "why is my graph not showing up in search" without
/// reading a schema artifact. Word-lane counts come from the index; the
/// vector-lane counts come from the embed projection against the vector
/// store (design 4.4). M9a indexes the published plane only.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphRetrievalParticipation {
    /// Authored policy flag: the per-graph kill switch for text retrieval.
    pub text_retrieval_enabled: bool,
    /// Effective indexability: the policy flag AND a source that may be
    /// indexed at all (local-scratch never participates).
    pub indexable: bool,
    /// Vertex types the policy excludes from word retrieval, sorted.
    pub excluded_vertex_types: Vec<String>,
    /// Documents this lane currently holds in the word index.
    pub indexed_vertex_count: usize,
    /// Generation stamp on the indexed documents, if any. Compare against
    /// `accepted_generation`: an empty count or a stale stamp means the lane
    /// is waiting for (or was dropped by) an activation.
    pub indexed_generation: Option<String>,
    /// Generation of the currently accepted view, for a one-place comparison.
    pub accepted_generation: String,
    /// Authored policy flag: the per-graph gate over every `embed: true`
    /// property annotation (`index_policy.embeddings_enabled`).
    pub embeddings_enabled: bool,
    /// Vertices of the accepted generation whose composed embed projection is
    /// non-empty under the three-way gate (policy on, `embed: true` property
    /// present, type not excluded). Zero with `embeddings_enabled` true means
    /// no property opted in or none carries a value.
    pub embed_eligible_vertex_count: usize,
    /// Eligible vertices whose vector is active under the CURRENT envelope
    /// hash. Less than `embed_eligible_vertex_count` means the embed queue is
    /// still draining (or the route is unavailable: see `bbox_embed_status`).
    /// `None` when no embed queue / vector store is installed to ask.
    pub embedded_vertex_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraphValidation {
    pub graph_id: String,
    pub valid: bool,
    pub source: &'static str,
    pub checkout_id: Option<String>,
    /// Bounded page of validation error rows. The complete array stays
    /// recoverable through the `detail=errors` exact body read.
    pub errors: Vec<Value>,
    pub errors_total: usize,
    pub errors_offset: usize,
    pub errors_limit: usize,
    pub next_error_offset: Option<usize>,
    /// Content-bound stamp over this generation's current error set. A
    /// changed set refuses error-page continuation.
    pub error_stamp: String,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
}

/// One exact detail body plus the identity every page response preserves:
/// selection scope, compact summary, and generation. The adapter pages
/// `body` through the shared transport-only body-page helper.
#[derive(Debug, Clone)]
pub(crate) struct GraphDetailRead {
    pub project_id: String,
    pub provisional_mode: &'static str,
    pub summary: GraphSummary,
    pub generation: bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity,
    pub body: Value,
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
        Ok(self.project_graph_inventory_domain(project, provisional)?.1)
    }

    /// The complete visible inventory for one selection, deterministically
    /// ordered, plus a content-bound stamp over that inventory. The stamp
    /// lets list continuation refuse when the live view changed between
    /// pages instead of silently skipping or duplicating entries.
    pub(crate) fn project_graph_inventory_domain(
        &self,
        project: Option<&str>,
        provisional: Option<&str>,
    ) -> Result<(ProvisionalMode, Vec<GraphSummary>, String)> {
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
        let mut summaries: Vec<GraphSummary> = entries.into_iter().map(summary).collect();
        summaries.sort_by(|a, b| {
            (
                &a.graph_id,
                a.source,
                &a.checkout_id,
                a.status,
                &a.content_hash,
            )
                .cmp(&(
                    &b.graph_id,
                    b.source,
                    &b.checkout_id,
                    b.status,
                    &b.content_hash,
                ))
        });
        let stamp = graph_view_stamp(&project_id, mode, &summaries);
        Ok((mode, summaries, stamp))
    }

    pub(crate) fn project_graph_describe_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<Vec<GraphDescription>> {
        let (project_id, _mode, _own) = self.graph_read_context(Some(project), provisional)?;
        let index = self.state.idx.read();
        let descriptions = self
            .graph_entries(project, graph_id, provisional)?
            .into_iter()
            .map(|entry| {
                let summary = summary(entry.clone());
                let retrieval = graph_retrieval_participation(&entry, &*index, &project_id)?;
                Ok(GraphDescription {
                    summary,
                    schema: entry.graph().map(schema_summary),
                    generation: entry.generation,
                    retrieval,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(descriptions)
    }

    /// Exactly one entry for an exact detail read. Under `all` visibility a
    /// graph id can name several live generations (published plus overlays);
    /// an exact body needs one generation, so ambiguity refuses instead of
    /// silently paging a different graph than the caller selected.
    fn single_graph_entry(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<(ProjectId, ProvisionalMode, ProjectGraphViewEntry)> {
        let (project_id, mode, _own) = self.graph_read_context(Some(project), provisional)?;
        let mut entries = self.graph_entries(project, graph_id, provisional)?;
        if entries.len() > 1 {
            let hashes = entries
                .iter()
                .map(|entry| entry.generation.content_hash.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "error.project_graph_ambiguous: exact detail matched {} graph generations ({hashes}); narrow provisional to published or own",
                entries.len()
            );
        }
        Ok((project_id, mode, entries.remove(0)))
    }

    /// Exact schema/descriptor body for one graph generation. The adapter
    /// turns `body` into bounded content-bound pages.
    pub(crate) fn project_graph_detail_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
        detail: GraphDescribeDetail,
    ) -> Result<GraphDetailRead> {
        let (project_id, mode, entry) = self.single_graph_entry(project, graph_id, provisional)?;
        let summary = summary(entry.clone());
        let Some(graph) = entry.graph().cloned() else {
            bail!(
                "error.graph_payload_unavailable: graph `{graph_id}` carries no parsed {} because it was accepted invalid; use bbox_project_graph_validate for diagnostics",
                detail.as_str()
            )
        };
        let generation = entry.generation;
        let body = match detail {
            GraphDescribeDetail::Schema => serde_json::to_value(&graph.schema)?,
            GraphDescribeDetail::Descriptor => serde_json::to_value(&graph.descriptor)?,
            GraphDescribeDetail::Summary => {
                unreachable!("summary detail reads the compact list, not an exact body")
            }
        };
        Ok(GraphDetailRead {
            project_id: project_id.to_string(),
            provisional_mode: mode_name(mode),
            summary,
            generation,
            body,
        })
    }

    pub(crate) fn project_graph_validate_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
        error_offset: usize,
        error_limit: usize,
    ) -> Result<Vec<GraphValidation>> {
        let (project_id, mode, _own) = self.graph_read_context(Some(project), provisional)?;
        self.graph_entries(project, graph_id, provisional)?
            .into_iter()
            .map(|entry| {
                let (valid, errors) = match entry.validity.clone() {
                    ProjectGraphValidity::Valid => (true, Vec::new()),
                    ProjectGraphValidity::Invalid { errors } => (false, errors),
                };
                let source = source_label(&entry);
                let error_values = errors
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<_>>>()?;
                let errors_total = error_values.len();
                let error_stamp = graph_error_stamp(
                    &project_id,
                    mode,
                    &entry.graph_id,
                    &entry.generation.content_hash,
                    &error_values,
                )?;
                let errors = error_values
                    .iter()
                    .skip(error_offset)
                    .take(error_limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let next_error_offset = (error_offset.saturating_add(errors.len()) < errors_total)
                    .then_some(error_offset + errors.len());
                Ok(GraphValidation {
                    graph_id: entry.graph_id.clone(),
                    valid,
                    source,
                    checkout_id: entry
                        .generation
                        .workspace_id
                        .as_ref()
                        .map(ToString::to_string),
                    errors,
                    errors_total,
                    errors_offset: error_offset,
                    errors_limit: error_limit,
                    next_error_offset,
                    error_stamp,
                    generation: entry.generation,
                })
            })
            .collect()
    }

    /// The complete validation error array for one graph generation, as an
    /// exact JSON body the adapter pages through the shared body reader.
    pub(crate) fn project_graph_validation_errors_domain(
        &self,
        project: &str,
        graph_id: &str,
        provisional: Option<&str>,
    ) -> Result<GraphDetailRead> {
        let (project_id, mode, entry) = self.single_graph_entry(project, graph_id, provisional)?;
        let summary = summary(entry.clone());
        let errors = match entry.validity.clone() {
            ProjectGraphValidity::Valid => Vec::new(),
            ProjectGraphValidity::Invalid { errors } => errors,
        };
        let generation = entry.generation;
        Ok(GraphDetailRead {
            project_id: project_id.to_string(),
            provisional_mode: mode_name(mode),
            summary,
            generation,
            body: serde_json::to_value(&errors)?,
        })
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
                // The vector half (design 4.4): pin the accepted generation
                // of every lane that embeds, so the per-hit re-check answers
                // from the same generation the word lane ranked.
                if bbox_project_graph::graph_embeds(graph) {
                    snapshot.embed_lanes.insert(lane.clone(), graph.clone());
                }
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
    /// Live graph-selection gate for traversal expansion (unified-retrieval
    /// 5.2). The gate owns GRAPH lanes only: a non-graph ref (a project file,
    /// a knowledge entry) is admitted untouched, because its readability is
    /// enforced by its own provider and the evidence-status algebra. A graph
    /// hop is admitted only when the destination lane resolves under the
    /// caller's active plane AND its policy leaves text retrieval on AND its
    /// source is not the never-indexable local-scratch plane. Resolution
    /// failure means the lane is absent for this caller, which is the same
    /// answer the entity loader would give one step later; refusing here
    /// keeps the vertex out of the frontier instead of leaking a truncated
    /// path that implies it exists.
    fn traversal_admits(&self, r: &EntityRef, provisional: Option<&str>) -> bool {
        use bbox_project_graph::GraphSource;

        if !matches!(
            r.entity_type(),
            bbox_corpus_core::entity_ref::EntityType::ProjectGraphVertex
                | bbox_corpus_core::entity_ref::EntityType::ProvisionalProjectGraphVertex
        ) {
            return true;
        }
        let Ok(resolved) = self.resolve_project_graph_vertex(r, provisional) else {
            return false;
        };
        !matches!(resolved.graph.key.source, GraphSource::LocalScratch)
            && resolved.graph.schema.index_policy.text_retrieval_enabled
    }

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
        // Schema-declared retrieval hints for this vertex's TYPE, projected
        // into the provider vocabulary. The schema is the substrate's answer to
        // "which hop is worth taking from here"; without it a consumer only
        // sees direction-blind family counts and lands one hop short.
        view.next_hop_hints = resolved
            .graph
            .schema
            .next_hop_hints_for(&resolved.vertex.type_name)
            .into_iter()
            .map(|hint| ProviderNextHopHint {
                edge_family_name: hint.edge_type,
                direction: match hint.direction {
                    HintDirection::Out => NextHopDirection::Out,
                    HintDirection::In => NextHopDirection::In,
                },
                label: hint.label,
                authored: hint.authored,
            })
            .collect();
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

fn graph_retrieval_participation(
    entry: &ProjectGraphViewEntry,
    index: &bbox_indexing::index::TranscriptIndex,
    project_id: &ProjectId,
) -> Result<GraphRetrievalParticipation> {
    use bbox_project_graph::GraphSource;

    let stats =
        index.graph_lane_stats(project_id.as_str(), &entry.graph_id, source_label(entry))?;
    let graph = entry.graph();
    let text_retrieval_enabled = graph
        .map(|graph| graph.schema.index_policy.text_retrieval_enabled)
        .unwrap_or(false);
    let never_indexable = graph
        .map(|graph| matches!(graph.key.source, GraphSource::LocalScratch))
        .unwrap_or(true);
    let embeddings_enabled = graph
        .map(|graph| graph.schema.index_policy.embeddings_enabled)
        .unwrap_or(false);
    // Published plane only: provisional overlays never embed in this
    // milestone, so a provisional entry reports zero eligible rather than
    // a phantom backlog.
    let projections = graph
        .filter(|_| entry.generation.workspace_id.is_none())
        .map(|graph| bbox_project_graph::graph_embed_projections(graph))
        .unwrap_or_default();
    let mut embedded_vertex_count = Some(0usize);
    for projection in &projections {
        let entity_id = EntityRef::ProjectGraphVertex {
            project_id: project_id.as_str().to_string(),
            graph_id: entry.graph_id.clone(),
            vertex_id: projection.vertex_id.clone(),
        }
        .to_string();
        match crate::embed_queue::graph_vertex_vector_is_active(
            project_id.as_str(),
            &entity_id,
            &projection.content_hash(),
        ) {
            Some(true) => {
                embedded_vertex_count = embedded_vertex_count.map(|count| count + 1);
            }
            Some(false) => {}
            None => {
                embedded_vertex_count = None;
                break;
            }
        }
    }
    Ok(GraphRetrievalParticipation {
        text_retrieval_enabled,
        indexable: text_retrieval_enabled && !never_indexable,
        excluded_vertex_types: graph
            .map(|graph| {
                graph
                    .schema
                    .index_policy
                    .retrieval_excluded_types
                    .iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        indexed_vertex_count: stats.indexed_vertex_count,
        indexed_generation: stats.indexed_generation,
        accepted_generation: entry.generation.content_hash.clone(),
        embeddings_enabled,
        embed_eligible_vertex_count: projections.len(),
        embedded_vertex_count,
    })
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

/// Compact schema identity: name and counts only, never the schema body.
fn schema_summary(graph: &GraphGeneration) -> GraphSchemaSummary {
    GraphSchemaSummary {
        schema_id: graph.descriptor.schema_id.clone(),
        schema_version: graph.descriptor.schema_version,
        vertex_type_count: graph.schema.vertex_types.len(),
        edge_type_count: graph.schema.edge_types.len(),
    }
}

/// Content-bound stamp over one visible inventory. Any entry added,
/// removed, republished, or flipped valid/invalid changes the stamp, so a
/// nonzero list offset carried across a view change refuses instead of
/// paging a silently different inventory.
fn graph_view_stamp(
    project_id: &ProjectId,
    mode: ProvisionalMode,
    summaries: &[GraphSummary],
) -> String {
    let mut hash = Sha256::new();
    hash.update(project_id.as_str().as_bytes());
    hash.update([0]);
    hash.update(mode_name(mode).as_bytes());
    hash.update([0]);
    for item in summaries {
        hash.update(item.graph_id.as_bytes());
        hash.update([0]);
        hash.update(item.source.as_bytes());
        hash.update([0]);
        hash.update(item.checkout_id.as_deref().unwrap_or("").as_bytes());
        hash.update([0]);
        hash.update(item.status.as_bytes());
        hash.update([0]);
        hash.update(item.content_hash.as_bytes());
        hash.update(b"\n");
    }
    format!("{:x}", hash.finalize())
}

/// Content-bound stamp over one generation's validation error set, so error
/// pages refuse continuation when the underlying graph or its errors
/// changed. The generation content hash alone is not enough: it names the
/// accepted bytes, while this stamp also commits to the exact error rows a
/// page walk is sampling.
fn graph_error_stamp(
    project_id: &ProjectId,
    mode: ProvisionalMode,
    graph_id: &str,
    content_hash: &str,
    errors: &[Value],
) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(project_id.as_str().as_bytes());
    hash.update([0]);
    hash.update(mode_name(mode).as_bytes());
    hash.update([0]);
    hash.update(graph_id.as_bytes());
    hash.update([0]);
    hash.update(content_hash.as_bytes());
    hash.update([0]);
    for error in errors {
        hash.update(serde_json::to_vec(error)?);
        hash.update(b"\n");
    }
    Ok(format!("{:x}", hash.finalize()))
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

    /// The compact describe summary names and counts the schema without
    /// carrying its body.
    #[test]
    fn schema_summary_counts_types_without_the_body() {
        let connector = generation(GraphAuthority::Connector, GraphSource::ConnectorManaged);
        let summary = schema_summary(&connector);
        assert_eq!(summary.schema_id, "dataset:schema");
        assert_eq!(summary.schema_version, 1);
        assert_eq!(summary.vertex_type_count, 1);
        assert_eq!(summary.edge_type_count, 0);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(serialized.contains("dataset:schema"));
        assert!(!serialized.contains("remote_id"), "{serialized}");
    }

    /// The list stamp binds project, mode, membership, and per-entry
    /// content: any of those changing must refuse a carried offset.
    #[test]
    fn view_stamp_binds_selection_membership_and_content() {
        let project = ProjectId::parse("p_graphstamp").unwrap();
        let entry = |graph_id: &str, hash: &str| GraphSummary {
            graph_id: graph_id.into(),
            status: "valid",
            source: "published",
            checkout_id: None,
            vertex_count: 1,
            edge_count: 0,
            authored_vertex_count: 1,
            authored_edge_count: 0,
            content_hash: hash.into(),
        };
        let one = "1".repeat(64);
        let two = "2".repeat(64);
        let base = vec![entry("a", &one), entry("b", &two)];
        let stamp = graph_view_stamp(&project, ProvisionalMode::Published, &base);
        assert_eq!(
            stamp,
            graph_view_stamp(&project, ProvisionalMode::Published, &base)
        );
        assert_ne!(
            stamp,
            graph_view_stamp(&project, ProvisionalMode::All, &base)
        );
        let other_project = ProjectId::parse("p_other").unwrap();
        assert_ne!(
            stamp,
            graph_view_stamp(&other_project, ProvisionalMode::Published, &base)
        );
        let changed_content = vec![entry("a", &"3".repeat(64)), entry("b", &two)];
        assert_ne!(
            stamp,
            graph_view_stamp(&project, ProvisionalMode::Published, &changed_content)
        );
        let changed_membership = vec![entry("a", &one)];
        assert_ne!(
            stamp,
            graph_view_stamp(&project, ProvisionalMode::Published, &changed_membership)
        );
    }

    /// The error stamp commits to the exact error rows, not just the graph.
    #[test]
    fn error_stamp_commits_to_the_error_rows() {
        let project = ProjectId::parse("p_errorstamp").unwrap();
        let errors = vec![
            serde_json::json!({"code": "edge.missing_vertex", "file": "edges.jsonl", "line": 7, "message": "target is missing"}),
        ];
        let stamp = graph_error_stamp(
            &project,
            ProvisionalMode::Own,
            "g",
            &"h".repeat(64),
            &errors,
        )
        .unwrap();
        assert_eq!(
            stamp,
            graph_error_stamp(
                &project,
                ProvisionalMode::Own,
                "g",
                &"h".repeat(64),
                &errors
            )
            .unwrap()
        );
        let changed = vec![serde_json::json!({
            "code": "edge.missing_vertex",
            "file": "edges.jsonl",
            "line": 8,
            "message": "target is missing",
        })];
        assert_ne!(
            stamp,
            graph_error_stamp(
                &project,
                ProvisionalMode::Own,
                "g",
                &"h".repeat(64),
                &changed
            )
            .unwrap()
        );
        assert_ne!(
            stamp,
            graph_error_stamp(
                &project,
                ProvisionalMode::Own,
                "g2",
                &"h".repeat(64),
                &errors
            )
            .unwrap()
        );
    }
}
