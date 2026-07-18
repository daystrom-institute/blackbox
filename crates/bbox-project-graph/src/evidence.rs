use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bbox_chunker::{EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_edge_index::edge_index::Edge;
use serde::{Deserialize, Serialize};

use crate::{GraphSource, ProjectGraphCatalog};

pub const EVIDENCE_DOCUMENT_VERSION: u32 = 1;
pub const EVIDENCE_BINDINGS_PATH: &str = ".bbox/evidence/bindings.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAssertionAuthority {
    Project,
    Connector,
    Operator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBinding {
    pub binding_id: String,
    pub scope_id: String,
    pub source: EntityRef,
    pub kind: String,
    pub target: EntityRef,
    pub assertion_authority: EvidenceAssertionAuthority,
    #[serde(default)]
    pub observation_id: Option<String>,
    #[serde(default)]
    pub mapping_version: Option<String>,
    pub asserted_at: String,
    #[serde(default)]
    pub source_generation: Option<u64>,
    #[serde(default)]
    pub target_generation: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDocument {
    pub version: u32,
    pub scope_id: String,
    #[serde(default)]
    pub bindings: Vec<EvidenceBinding>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceEndpointStatus {
    Current,
    Stale,
    Missing,
    Unauthorized,
    Unresolved,
}

impl EvidenceEndpointStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Missing => "missing",
            Self::Unauthorized => "unauthorized",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EvidenceValidationError {
    pub code: String,
    pub binding_id: Option<String>,
    pub message: String,
}

impl EvidenceValidationError {
    fn document(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            binding_id: None,
            message: message.into(),
        }
    }

    fn binding(binding: &EvidenceBinding, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            binding_id: Some(binding.binding_id.clone()),
            message: message.into(),
        }
    }
}

pub fn evidence_document_path(project_root: &Path) -> PathBuf {
    project_root.join(EVIDENCE_BINDINGS_PATH)
}

pub fn load_evidence_document(
    expected_scope_id: &str,
    project_root: &Path,
) -> Result<Option<EvidenceDocument>> {
    let path = evidence_document_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    let first = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let second = fs::read(&path).with_context(|| format!("rereading {}", path.display()))?;
    if first != second {
        anyhow::bail!(
            "evidence bindings changed while being read; retry after the file update completes"
        );
    }
    let document: EvidenceDocument =
        serde_json::from_slice(&first).with_context(|| format!("parsing {}", path.display()))?;
    let errors = validate_evidence_document(expected_scope_id, &document);
    if !errors.is_empty() {
        anyhow::bail!(
            "evidence bindings failed validation: {}",
            serde_json::to_string(&errors)?
        );
    }
    Ok(Some(document))
}

pub fn validate_evidence_document(
    expected_scope_id: &str,
    document: &EvidenceDocument,
) -> Vec<EvidenceValidationError> {
    let mut errors = Vec::new();
    if document.version != EVIDENCE_DOCUMENT_VERSION {
        errors.push(EvidenceValidationError::document(
            "evidence.unsupported_version",
            format!(
                "evidence document version {} is unsupported; expected {}",
                document.version, EVIDENCE_DOCUMENT_VERSION
            ),
        ));
    }
    if document.scope_id.trim().is_empty() || document.scope_id != expected_scope_id {
        errors.push(EvidenceValidationError::document(
            "evidence.scope_mismatch",
            format!(
                "document scope_id `{}` must equal expected project scope `{expected_scope_id}`",
                document.scope_id
            ),
        ));
    }

    let mut binding_ids = BTreeSet::new();
    for binding in &document.bindings {
        if binding.binding_id.trim().is_empty()
            || binding
                .binding_id
                .bytes()
                .any(|byte| byte.is_ascii_control())
        {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.invalid_binding_id",
                "binding_id must be non-empty and contain no control characters",
            ));
        } else if !binding_ids.insert(&binding.binding_id) {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.duplicate_binding_id",
                format!("binding_id `{}` appears more than once", binding.binding_id),
            ));
        }
        if binding.scope_id != document.scope_id {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.binding_scope_mismatch",
                "binding scope_id must equal document scope_id",
            ));
        }
        if binding.kind.trim().is_empty()
            || binding.kind.bytes().any(|byte| byte.is_ascii_control())
        {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.invalid_kind",
                "evidence kind must be non-empty and contain no control characters",
            ));
        }
        if binding.source == binding.target {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.self_edge",
                "an evidence binding must connect two distinct entity refs",
            ));
        }
        if binding.asserted_at.trim().is_empty() {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.missing_asserted_at",
                "asserted_at must not be empty",
            ));
        }
        if binding.assertion_authority == EvidenceAssertionAuthority::Connector
            && binding
                .observation_id
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.connector_observation_required",
                "connector assertions require observation_id",
            ));
        }
        if matches!(
            binding.assertion_authority,
            EvidenceAssertionAuthority::Project | EvidenceAssertionAuthority::Operator
        ) && binding
            .mapping_version
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.mapping_version_required",
                "project and operator assertions require mapping_version",
            ));
        }
        if !matches!(
            (&binding.source, &binding.target),
            (EntityRef::ProjectGraphVertex { .. }, _) | (_, EntityRef::ProjectGraphVertex { .. })
        ) {
            errors.push(EvidenceValidationError::binding(
                binding,
                "evidence.graph_endpoint_required",
                "at least one evidence endpoint must be a project graph vertex",
            ));
        }
    }
    errors
}

pub(crate) fn binding_edge(
    binding: &EvidenceBinding,
    catalog: &ProjectGraphCatalog,
    include_local: bool,
) -> Edge {
    let source_status = endpoint_status(
        catalog,
        &binding.scope_id,
        &binding.source,
        binding.source_generation,
        include_local,
    );
    let target_status = endpoint_status(
        catalog,
        &binding.scope_id,
        &binding.target,
        binding.target_generation,
        include_local,
    );
    let authority = serde_json::to_value(binding.assertion_authority)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    let mut metadata = BTreeMap::from([
        ("evidence.binding_id".into(), binding.binding_id.clone()),
        ("evidence.scope_id".into(), binding.scope_id.clone()),
        ("evidence.assertion_authority".into(), authority),
        ("evidence.asserted_at".into(), binding.asserted_at.clone()),
        (
            "evidence.source_status".into(),
            source_status.as_str().into(),
        ),
        (
            "evidence.target_status".into(),
            target_status.as_str().into(),
        ),
        (
            "evidence.freshness".into(),
            aggregate_status(source_status, target_status)
                .as_str()
                .into(),
        ),
    ]);
    for (name, value) in [
        ("evidence.observation_id", binding.observation_id.as_ref()),
        ("evidence.mapping_version", binding.mapping_version.as_ref()),
    ] {
        if let Some(value) = value {
            metadata.insert(name.into(), value.clone());
        }
    }
    for (name, value) in [
        ("evidence.source_generation", binding.source_generation),
        ("evidence.target_generation", binding.target_generation),
    ] {
        if let Some(value) = value {
            metadata.insert(name.into(), value.to_string());
        }
    }
    Edge {
        source: binding.source.clone(),
        kind: binding.kind.clone(),
        target: binding.target.clone(),
        provenance: EdgeProvenance::Explicit,
        confidence: EdgeConfidence::Exact,
        metadata,
    }
}

fn endpoint_status(
    catalog: &ProjectGraphCatalog,
    scope_id: &str,
    entity: &EntityRef,
    expected_generation: Option<u64>,
    include_local: bool,
) -> EvidenceEndpointStatus {
    if entity_scope(entity).is_some_and(|entity_scope| entity_scope != scope_id) {
        return EvidenceEndpointStatus::Unauthorized;
    }
    let EntityRef::ProjectGraphVertex {
        scope_id,
        graph_id,
        vertex_id,
    } = entity
    else {
        return EvidenceEndpointStatus::Unresolved;
    };
    let Some(generation) = catalog.get(scope_id, graph_id, include_local) else {
        return if expected_generation.is_some() {
            EvidenceEndpointStatus::Stale
        } else {
            EvidenceEndpointStatus::Missing
        };
    };
    if expected_generation.is_some_and(|expected| expected != generation.descriptor.generation)
        || !generation.vertices.contains_key(vertex_id)
    {
        return EvidenceEndpointStatus::Stale;
    }
    EvidenceEndpointStatus::Current
}

fn aggregate_status(
    source: EvidenceEndpointStatus,
    target: EvidenceEndpointStatus,
) -> EvidenceEndpointStatus {
    for status in [
        EvidenceEndpointStatus::Unauthorized,
        EvidenceEndpointStatus::Stale,
        EvidenceEndpointStatus::Missing,
        EvidenceEndpointStatus::Unresolved,
    ] {
        if source == status || target == status {
            return status;
        }
    }
    EvidenceEndpointStatus::Current
}

pub fn entity_scope(entity: &EntityRef) -> Option<&str> {
    match entity {
        EntityRef::ProjectGraphVertex { scope_id, .. } => Some(scope_id),
        EntityRef::ProjectFile { project_id, .. }
        | EntityRef::ProjectFileV2 { project_id, .. }
        | EntityRef::Symbol { project_id, .. }
        | EntityRef::SymbolV2 { project_id, .. } => Some(project_id),
        _ => None,
    }
}

pub(crate) fn binding_hidden_by_local_policy(
    binding: &EvidenceBinding,
    catalog: &ProjectGraphCatalog,
    include_local: bool,
) -> bool {
    if include_local {
        return false;
    }
    [&binding.source, &binding.target]
        .into_iter()
        .filter_map(|entity| match entity {
            EntityRef::ProjectGraphVertex {
                scope_id, graph_id, ..
            } => Some((scope_id, graph_id)),
            _ => None,
        })
        .any(|(scope_id, graph_id)| {
            catalog.has_source(scope_id, graph_id, GraphSource::LocalScratch)
                && !catalog.has_source(scope_id, graph_id, GraphSource::Committed)
                && !catalog.has_source(scope_id, graph_id, GraphSource::ConnectorManaged)
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        DESCRIPTOR_VERSION, GraphAuthority, GraphDescriptor, GraphGeneration, GraphKey,
        GraphSchema, GraphScope, ProjectGraphVertex, RetentionPolicy, VertexTypeDefinition,
        build_generation,
    };

    fn generation(
        graph_id: &str,
        source: GraphSource,
        generation: u64,
        vertices: &[&str],
    ) -> GraphGeneration {
        let connector = source == GraphSource::ConnectorManaged;
        let schema = GraphSchema {
            version: 1,
            namespace: "fixture".into(),
            vertex_types: BTreeMap::from([(
                "fixture:Node".into(),
                VertexTypeDefinition {
                    required: Vec::new(),
                    properties: BTreeMap::new(),
                },
            )]),
            edge_types: Vec::new(),
        };
        build_generation(
            GraphKey {
                scope_id: "scope-a".into(),
                graph_id: graph_id.into(),
                source,
            },
            GraphDescriptor {
                descriptor_version: DESCRIPTOR_VERSION,
                scope: GraphScope::Project,
                graph_id: graph_id.into(),
                authority: if connector {
                    GraphAuthority::Connector
                } else {
                    GraphAuthority::Project
                },
                schema_id: format!("{graph_id}-schema"),
                schema_version: 1,
                projection_version: connector.then(|| "fixture-v1".into()),
                source_connector: connector.then(|| "synthetic-api".into()),
                retention_policy: source.retention_policy(),
                generation,
            },
            schema,
            vertices
                .iter()
                .map(|id| ProjectGraphVertex {
                    id: (*id).into(),
                    type_name: "fixture:Node".into(),
                    label: (*id).into(),
                    properties: BTreeMap::new(),
                })
                .collect(),
            Vec::new(),
            format!("{graph_id}-{generation}"),
            PathBuf::from("."),
        )
    }

    fn graph_ref(graph_id: &str, vertex_id: &str) -> EntityRef {
        EntityRef::ProjectGraphVertex {
            scope_id: "scope-a".into(),
            graph_id: graph_id.into(),
            vertex_id: vertex_id.into(),
        }
    }

    fn project_file() -> EntityRef {
        EntityRef::ProjectFile {
            project_id: "scope-a".into(),
            rel_path_hash: "path".into(),
            chunk_hash: "chunk".into(),
            occurrence_idx: 0,
        }
    }

    #[test]
    fn connector_assertions_require_observation_provenance() {
        let document = EvidenceDocument {
            version: EVIDENCE_DOCUMENT_VERSION,
            scope_id: "scope-a".into(),
            bindings: vec![EvidenceBinding {
                binding_id: "binding-1".into(),
                scope_id: "scope-a".into(),
                source: EntityRef::ProjectGraphVertex {
                    scope_id: "scope-a".into(),
                    graph_id: "source".into(),
                    vertex_id: "asset-1".into(),
                },
                kind: "dataset:EVIDENCED_BY".into(),
                target: EntityRef::ProjectFile {
                    project_id: "scope-a".into(),
                    rel_path_hash: "path".into(),
                    chunk_hash: "chunk".into(),
                    occurrence_idx: 0,
                },
                assertion_authority: EvidenceAssertionAuthority::Connector,
                observation_id: None,
                mapping_version: None,
                asserted_at: "2026-01-01T00:00:00Z".into(),
                source_generation: Some(1),
                target_generation: None,
            }],
        };
        assert!(
            validate_evidence_document("scope-a", &document)
                .iter()
                .any(|error| error.code == "evidence.connector_observation_required")
        );
    }

    #[test]
    fn bindings_cross_graphs_and_files_then_survive_source_deletion() {
        let mut catalog = ProjectGraphCatalog::default();
        catalog
            .publish(generation(
                "records",
                GraphSource::Committed,
                1,
                &["record-1"],
            ))
            .unwrap();
        catalog
            .publish(generation(
                "source",
                GraphSource::ConnectorManaged,
                1,
                &["asset-1"],
            ))
            .unwrap();
        let record = graph_ref("records", "record-1");
        let asset = graph_ref("source", "asset-1");
        let file = project_file();
        catalog
            .replace_evidence_scope(
                "scope-a",
                vec![
                    EvidenceBinding {
                        binding_id: "record-to-source".into(),
                        scope_id: "scope-a".into(),
                        source: record.clone(),
                        kind: "fixture:CORRESPONDS_TO".into(),
                        target: asset.clone(),
                        assertion_authority: EvidenceAssertionAuthority::Project,
                        observation_id: None,
                        mapping_version: Some("mapping-v1".into()),
                        asserted_at: "2026-01-01T00:00:00Z".into(),
                        source_generation: Some(1),
                        target_generation: Some(1),
                    },
                    EvidenceBinding {
                        binding_id: "source-to-file".into(),
                        scope_id: "scope-a".into(),
                        source: asset.clone(),
                        kind: "fixture:EVIDENCED_BY".into(),
                        target: file.clone(),
                        assertion_authority: EvidenceAssertionAuthority::Connector,
                        observation_id: Some("obs-file-1".into()),
                        mapping_version: None,
                        asserted_at: "2026-01-01T00:00:00Z".into(),
                        source_generation: Some(1),
                        target_generation: None,
                    },
                ],
            )
            .unwrap();

        let first_hop = catalog.forward_edges(&record, false);
        let first_hop = first_hop
            .iter()
            .find(|edge| edge.kind == "fixture:CORRESPONDS_TO")
            .unwrap();
        assert_eq!(first_hop.target, asset);
        assert_eq!(
            first_hop.metadata["evidence.freshness"],
            EvidenceEndpointStatus::Current.as_str()
        );
        assert_eq!(first_hop.metadata["evidence.mapping_version"], "mapping-v1");

        let reverse = catalog.reverse_edges(&file, false);
        assert_eq!(reverse.len(), 1);
        assert_eq!(reverse[0].source, asset);
        assert_eq!(reverse[0].metadata["evidence.observation_id"], "obs-file-1");
        assert_eq!(
            reverse[0].provenance,
            bbox_chunker::EdgeProvenance::Explicit
        );

        catalog
            .publish(generation("source", GraphSource::ConnectorManaged, 2, &[]))
            .unwrap();
        assert_eq!(catalog.evidence_bindings("scope-a").len(), 2);
        let stale = catalog.forward_edges(&record, false);
        let stale = stale
            .iter()
            .find(|edge| edge.kind == "fixture:CORRESPONDS_TO")
            .unwrap();
        assert_eq!(
            stale.metadata["evidence.target_status"],
            EvidenceEndpointStatus::Stale.as_str()
        );
        let reverse = catalog.reverse_edges(&file, false);
        assert_eq!(
            reverse[0].metadata["evidence.source_status"],
            EvidenceEndpointStatus::Stale.as_str()
        );
    }

    #[test]
    fn cross_scope_endpoint_is_retained_but_marked_unauthorized() {
        let mut catalog = ProjectGraphCatalog::default();
        catalog
            .publish(generation(
                "records",
                GraphSource::Committed,
                1,
                &["record-1"],
            ))
            .unwrap();
        let record = graph_ref("records", "record-1");
        let foreign = EntityRef::ProjectGraphVertex {
            scope_id: "scope-b".into(),
            graph_id: "source".into(),
            vertex_id: "asset-1".into(),
        };
        catalog
            .replace_evidence_scope(
                "scope-a",
                vec![EvidenceBinding {
                    binding_id: "unauthorized-target".into(),
                    scope_id: "scope-a".into(),
                    source: record.clone(),
                    kind: "fixture:CORRESPONDS_TO".into(),
                    target: foreign,
                    assertion_authority: EvidenceAssertionAuthority::Operator,
                    observation_id: None,
                    mapping_version: Some("mapping-v1".into()),
                    asserted_at: "2026-01-01T00:00:00Z".into(),
                    source_generation: Some(1),
                    target_generation: None,
                }],
            )
            .unwrap();
        let edges = catalog.forward_edges(&record, false);
        let edge = edges
            .iter()
            .find(|edge| edge.kind == "fixture:CORRESPONDS_TO")
            .unwrap();
        assert_eq!(
            edge.metadata["evidence.target_status"],
            EvidenceEndpointStatus::Unauthorized.as_str()
        );
    }
}
