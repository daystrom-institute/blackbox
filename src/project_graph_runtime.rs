use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::Edge;
use bbox_project_graph::{
    CatalogPublishError, GraphGeneration, GraphLoad, GraphSource, ProjectGraphCatalog,
    ValidationError, ValidationReport, discover_graphs, duplicate_graph_ids,
    load_evidence_document, load_graph, locate_graph, meta_schema_floor, vertex_properties,
};
use serde_json::json;

use crate::providers::ProjectGraphAccess;
use crate::server::state::SharedState;

#[derive(Debug)]
pub(crate) struct RefreshResult {
    pub(crate) report: ValidationReport,
    pub(crate) accepted: Option<Arc<GraphGeneration>>,
    pub(crate) publish_error: Option<CatalogPublishError>,
    pub(crate) evidence_binding_count: usize,
    pub(crate) evidence_error: Option<String>,
}

impl SharedState {
    pub(crate) fn refresh_project_graph(
        &self,
        scope_id: &str,
        project_root: &Path,
        graph_id: &str,
        include_local: bool,
    ) -> RefreshResult {
        let location = match locate_graph(project_root, graph_id, include_local) {
            Ok(location) => location,
            Err(error) => {
                if error.code == "graph.not_found" {
                    let mut catalog = self.project_graphs.write();
                    catalog.remove_graph(scope_id, graph_id, GraphSource::Committed);
                    if include_local {
                        catalog.remove_graph(scope_id, graph_id, GraphSource::LocalScratch);
                    }
                }
                let source = if include_local {
                    GraphSource::LocalScratch
                } else {
                    GraphSource::Committed
                };
                return RefreshResult {
                    report: ValidationReport {
                        scope_id: scope_id.to_string(),
                        graph_id: graph_id.to_string(),
                        source,
                        valid: false,
                        errors: vec![error],
                        descriptor: None,
                        namespace: None,
                        fact_vertex_count: 0,
                        fact_edge_count: 0,
                        fingerprint: None,
                    },
                    accepted: None,
                    publish_error: None,
                    evidence_binding_count: 0,
                    evidence_error: None,
                };
            }
        };
        let GraphLoad { report, generation } = load_graph(scope_id, project_root, &location);
        let Some(generation) = generation else {
            return RefreshResult {
                report,
                accepted: None,
                publish_error: None,
                evidence_binding_count: 0,
                evidence_error: None,
            };
        };
        let mut result = match self.project_graphs.write().publish(generation) {
            Ok(accepted) => RefreshResult {
                report,
                accepted: Some(accepted),
                publish_error: None,
                evidence_binding_count: 0,
                evidence_error: None,
            },
            Err(error) => RefreshResult {
                report,
                accepted: None,
                publish_error: Some(error),
                evidence_binding_count: 0,
                evidence_error: None,
            },
        };
        if result.accepted.is_some() {
            let (count, error) = self.refresh_project_evidence(scope_id, project_root);
            result.evidence_binding_count = count;
            result.evidence_error = error;
        }
        result
    }

    fn refresh_project_evidence(
        &self,
        scope_id: &str,
        project_root: &Path,
    ) -> (usize, Option<String>) {
        match load_evidence_document(scope_id, project_root) {
            Ok(Some(document)) => {
                let count = document.bindings.len();
                match self
                    .project_graphs
                    .write()
                    .replace_evidence_scope(scope_id, document.bindings)
                {
                    Ok(()) => (count, None),
                    Err(errors) => (
                        self.project_graphs.read().evidence_bindings(scope_id).len(),
                        Some(format!(
                            "evidence bindings failed catalog validation: {}",
                            serde_json::to_string(&errors)
                                .unwrap_or_else(|_| "validation error".into())
                        )),
                    ),
                }
            }
            Ok(None) => {
                self.project_graphs.write().remove_evidence_scope(scope_id);
                (0, None)
            }
            Err(error) => (
                self.project_graphs.read().evidence_bindings(scope_id).len(),
                Some(error.to_string()),
            ),
        }
    }

    pub(crate) fn refresh_project_graph_ref(
        &self,
        entity: &EntityRef,
        include_local: bool,
    ) -> Option<RefreshResult> {
        let EntityRef::ProjectGraphVertex {
            scope_id, graph_id, ..
        } = entity
        else {
            return None;
        };
        let current = {
            self.project_graphs
                .read()
                .get(scope_id, graph_id, include_local)
        };
        if let Some(current) = current {
            let source_root = current.source_root.clone();
            return Some(self.refresh_project_graph(
                scope_id,
                &source_root,
                graph_id,
                include_local,
            ));
        }
        let project = self
            .projects
            .read()
            .list()
            .into_iter()
            .find(|project| project.project_id == *scope_id);
        match project {
            Some(project) => Some(self.refresh_project_graph(
                scope_id,
                Path::new(&project.canonical_path),
                graph_id,
                include_local,
            )),
            None => Some(RefreshResult {
                report: ValidationReport {
                    scope_id: scope_id.clone(),
                    graph_id: graph_id.clone(),
                    source: GraphSource::Committed,
                    valid: false,
                    errors: vec![ValidationError::new(
                        "graph.scope_not_registered",
                        "graph.json",
                        None,
                        format!("project graph scope `{scope_id}` is not registered"),
                    )],
                    descriptor: None,
                    namespace: None,
                    fact_vertex_count: 0,
                    fact_edge_count: 0,
                    fingerprint: None,
                },
                accepted: None,
                publish_error: None,
                evidence_binding_count: 0,
                evidence_error: None,
            }),
        }
    }
}

impl ProjectGraphAccess for SharedState {
    fn entity_properties(
        &self,
        entity: &EntityRef,
        include_local: bool,
    ) -> Result<Option<BTreeMap<String, String>>> {
        let EntityRef::ProjectGraphVertex {
            scope_id,
            graph_id,
            vertex_id,
        } = entity
        else {
            return Ok(None);
        };
        let generation = self
            .project_graphs
            .read()
            .get(scope_id, graph_id, include_local);
        Ok(generation.and_then(|generation| {
            generation
                .projected_vertex(vertex_id)
                .map(|vertex| vertex_properties(&generation, vertex))
        }))
    }

    fn forward_edges(&self, entity: &EntityRef, include_local: bool) -> Vec<Edge> {
        self.project_graphs
            .read()
            .forward_edges(entity, include_local)
    }

    fn reverse_edges(&self, entity: &EntityRef, include_local: bool) -> Vec<Edge> {
        self.project_graphs
            .read()
            .reverse_edges(entity, include_local)
    }

    fn known_refs(&self, include_local: bool) -> Vec<EntityRef> {
        self.project_graphs.read().known_refs(include_local)
    }
}

pub(crate) fn publish_loaded(catalog: &mut ProjectGraphCatalog, load: GraphLoad) -> RefreshResult {
    let report = load.report;
    let Some(generation) = load.generation else {
        return RefreshResult {
            report,
            accepted: None,
            publish_error: None,
            evidence_binding_count: 0,
            evidence_error: None,
        };
    };
    match catalog.publish(generation) {
        Ok(accepted) => RefreshResult {
            report,
            accepted: Some(accepted),
            publish_error: None,
            evidence_binding_count: 0,
            evidence_error: None,
        },
        Err(error) => RefreshResult {
            report,
            accepted: None,
            publish_error: Some(error),
            evidence_binding_count: 0,
            evidence_error: None,
        },
    }
}

pub(crate) fn list_graphs(
    state: &SharedState,
    projects: Vec<(String, PathBuf)>,
    include_local: bool,
) -> Result<String> {
    let mut graphs = Vec::new();
    let mut evidence = Vec::new();
    for (scope_id, root) in projects {
        let locations = discover_graphs(&root, include_local);
        let duplicates = duplicate_graph_ids(&locations);
        let committed_ids = locations
            .iter()
            .filter(|location| location.source == GraphSource::Committed)
            .map(|location| location.graph_id.clone())
            .collect();
        let local_ids = locations
            .iter()
            .filter(|location| location.source == GraphSource::LocalScratch)
            .map(|location| location.graph_id.clone())
            .collect();
        {
            let mut catalog = state.project_graphs.write();
            catalog.reconcile_source(&scope_id, GraphSource::Committed, &committed_ids);
            if include_local {
                catalog.reconcile_source(&scope_id, GraphSource::LocalScratch, &local_ids);
            }
        }
        for location in locations {
            let mut load = load_graph(&scope_id, &root, &location);
            if duplicates.contains(&location.graph_id) {
                load.report.valid = false;
                load.report.errors.push(ValidationError::new(
                    "graph.ambiguous_source",
                    "graph.json",
                    None,
                    format!(
                        "graph `{}` exists in both .bbox/graphs and .bbox/local/graphs",
                        location.graph_id
                    ),
                ));
                load.generation = None;
            }
            let refreshed = publish_loaded(&mut state.project_graphs.write(), load);
            graphs.push(json!({
                "project_id": scope_id,
                "project_root": root,
                "validation": refreshed.report,
                "accepted": refreshed.accepted.is_some(),
                "publish_error": refreshed.publish_error,
            }));
        }
        let (binding_count, error) = state.refresh_project_evidence(&scope_id, &root);
        evidence.push(json!({
            "project_id": scope_id,
            "project_root": root,
            "binding_count": binding_count,
            "error": error,
        }));
    }
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "include_local": include_local,
        "graphs": graphs,
        "evidence": evidence,
    }))?)
}

pub(crate) fn validate_graph(
    state: &SharedState,
    scope_id: &str,
    root: &Path,
    graph_id: &str,
    include_local: bool,
) -> Result<String> {
    let refreshed = state.refresh_project_graph(scope_id, root, graph_id, include_local);
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "validation": refreshed.report,
        "accepted": refreshed.accepted.is_some(),
        "publish_error": refreshed.publish_error,
        "evidence_binding_count": refreshed.evidence_binding_count,
        "evidence_error": refreshed.evidence_error,
    }))?)
}

pub(crate) fn describe_graph(
    state: &SharedState,
    scope_id: &str,
    root: &Path,
    graph_id: &str,
    include_local: bool,
) -> Result<String> {
    let refreshed = state.refresh_project_graph(scope_id, root, graph_id, include_local);
    let Some(generation) = refreshed.accepted else {
        return Ok(serde_json::to_string_pretty(&json!({
            "status": "error.invalid_project_graph",
            "validation": refreshed.report,
            "publish_error": refreshed.publish_error,
            "evidence_binding_count": refreshed.evidence_binding_count,
            "evidence_error": refreshed.evidence_error,
        }))?);
    };
    let schema_vertex_refs = generation
        .schema
        .vertex_types
        .keys()
        .chain(
            generation
                .schema
                .edge_types
                .iter()
                .map(|definition| &definition.type_name),
        )
        .map(|id| generation.vertex_ref(id).to_string())
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&json!({
        "status": "ok",
        "scope_id": scope_id,
        "graph_id": generation.key.graph_id,
        "source": generation.key.source,
        "descriptor": generation.descriptor,
        "schema": generation.schema,
        "meta_schema": meta_schema_floor(),
        "counts": {
            "fact_vertices": generation.vertices.len()
                .saturating_sub(bbox_project_graph::FIXED_META_VERTICES.len())
                .saturating_sub(schema_vertex_refs.len()),
            "projected_vertices": generation.vertices.len(),
            "fact_edges": generation.edges.len(),
            "projected_edges": generation.projected_edges.len(),
        },
        "schema_vertex_refs": schema_vertex_refs,
        "source_root": generation.source_root,
        "fingerprint": generation.fingerprint,
        "evidence_binding_count": refreshed.evidence_binding_count,
        "evidence_error": refreshed.evidence_error,
    }))?)
}

pub(crate) fn refresh_ref_error(
    state: &SharedState,
    entity: &EntityRef,
    include_local: bool,
) -> Option<String> {
    let refreshed = state.refresh_project_graph_ref(entity, include_local)?;
    if refreshed.accepted.is_some() {
        return None;
    }
    Some(
        serde_json::to_string_pretty(&json!({
            "status": "error.invalid_project_graph",
            "validation": refreshed.report,
            "publish_error": refreshed.publish_error,
            "evidence_binding_count": refreshed.evidence_binding_count,
            "evidence_error": refreshed.evidence_error,
        }))
        .expect("project graph refresh error serializes"),
    )
}
