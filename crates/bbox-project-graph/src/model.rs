use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};

pub const DESCRIPTOR_VERSION: u32 = 1;
pub const META_VERTEX_TYPE: &str = "meta:VertexType";
pub const META_EDGE_TYPE: &str = "meta:EdgeType";
pub const META_INSTANCE_OF: &str = "meta:INSTANCE_OF";
pub const META_FROM_TYPE: &str = "meta:FROM_TYPE";
pub const META_TO_TYPE: &str = "meta:TO_TYPE";
pub const FIXED_META_VERTICES: [&str; 5] = [
    META_VERTEX_TYPE,
    META_EDGE_TYPE,
    META_INSTANCE_OF,
    META_FROM_TYPE,
    META_TO_TYPE,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphScope {
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphAuthority {
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    ProjectOwned,
    LocalScratch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphDescriptor {
    pub descriptor_version: u32,
    pub scope: GraphScope,
    pub graph_id: String,
    pub authority: GraphAuthority,
    pub schema_id: String,
    pub schema_version: u64,
    #[serde(default)]
    pub projection_version: Option<String>,
    #[serde(default)]
    pub source_connector: Option<String>,
    pub retention_policy: RetentionPolicy,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GraphSchema {
    pub version: u64,
    pub namespace: String,
    pub vertex_types: BTreeMap<String, VertexTypeDefinition>,
    pub edge_types: Vec<EdgeTypeDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VertexTypeDefinition {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EdgeTypeDefinition {
    #[serde(rename = "type")]
    pub type_name: String,
    pub endpoints: Vec<EdgeEndpointDefinition>,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct EdgeEndpointDefinition {
    #[serde(rename = "from")]
    pub from_type: String,
    #[serde(rename = "to")]
    pub to_type: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeTypeDefinitionWire {
    #[serde(rename = "type")]
    type_name: String,
    #[serde(default)]
    from_type: Option<String>,
    #[serde(default)]
    to_type: Option<String>,
    #[serde(default)]
    endpoints: Option<Vec<EdgeEndpointDefinition>>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    properties: BTreeMap<String, Value>,
}

impl<'de> Deserialize<'de> for EdgeTypeDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = EdgeTypeDefinitionWire::deserialize(deserializer)?;
        let endpoints = match (wire.from_type, wire.to_type, wire.endpoints) {
            (Some(from_type), Some(to_type), None) => {
                vec![EdgeEndpointDefinition { from_type, to_type }]
            }
            (None, None, Some(endpoints)) => endpoints,
            _ => {
                return Err(de::Error::custom(
                    "edge type must declare either from_type/to_type or endpoints",
                ));
            }
        };
        Ok(Self {
            type_name: wire.type_name,
            endpoints,
            required: wire.required,
            properties: wire.properties,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectGraphVertex {
    pub id: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub label: String,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProjectGraphEdge {
    pub from: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub to: String,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphSource {
    Committed,
    LocalScratch,
}

impl GraphSource {
    pub fn retention_policy(self) -> RetentionPolicy {
        match self {
            Self::Committed => RetentionPolicy::ProjectOwned,
            Self::LocalScratch => RetentionPolicy::LocalScratch,
        }
    }

    pub fn relative_root(self) -> &'static str {
        match self {
            Self::Committed => ".bbox/graphs",
            Self::LocalScratch => ".bbox/local/graphs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphKey {
    pub scope_id: String,
    pub graph_id: String,
    pub source: GraphSource,
}

#[derive(Debug, Clone)]
pub struct GraphGeneration {
    pub key: GraphKey,
    pub descriptor: GraphDescriptor,
    pub schema: GraphSchema,
    pub vertices: BTreeMap<String, ProjectGraphVertex>,
    pub edges: Vec<ProjectGraphEdge>,
    pub fingerprint: String,
    pub source_root: PathBuf,
}

impl GraphGeneration {
    pub fn projected_vertex(&self, vertex_id: &str) -> Option<&ProjectGraphVertex> {
        self.vertices.get(vertex_id)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetaSchemaFloor {
    pub vertex_types: [&'static str; 2],
    pub edge_types: [&'static str; 3],
    pub endpoint_contracts: BTreeMap<&'static str, MetaEndpointContract>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetaEndpointContract {
    pub from_type: &'static str,
    pub to_type: &'static str,
}

pub fn meta_schema_floor() -> MetaSchemaFloor {
    MetaSchemaFloor {
        vertex_types: [META_VERTEX_TYPE, META_EDGE_TYPE],
        edge_types: [META_INSTANCE_OF, META_FROM_TYPE, META_TO_TYPE],
        endpoint_contracts: BTreeMap::from([
            (
                META_INSTANCE_OF,
                MetaEndpointContract {
                    from_type: "any_vertex",
                    to_type: META_VERTEX_TYPE,
                },
            ),
            (
                META_FROM_TYPE,
                MetaEndpointContract {
                    from_type: META_EDGE_TYPE,
                    to_type: META_VERTEX_TYPE,
                },
            ),
            (
                META_TO_TYPE,
                MetaEndpointContract {
                    from_type: META_EDGE_TYPE,
                    to_type: META_VERTEX_TYPE,
                },
            ),
        ]),
    }
}

pub(crate) fn project_generation(
    key: GraphKey,
    descriptor: GraphDescriptor,
    schema: GraphSchema,
    fact_vertices: Vec<ProjectGraphVertex>,
    fact_edges: Vec<ProjectGraphEdge>,
    fingerprint: String,
    source_root: PathBuf,
) -> GraphGeneration {
    let mut vertices = fixed_meta_vertices();
    for (type_name, definition) in &schema.vertex_types {
        vertices.insert(
            type_name.clone(),
            ProjectGraphVertex {
                id: type_name.clone(),
                type_name: META_VERTEX_TYPE.to_string(),
                label: type_name.clone(),
                properties: BTreeMap::from([
                    ("required".into(), json!(definition.required)),
                    ("properties".into(), json!(definition.properties)),
                    ("schema_definition".into(), Value::Bool(true)),
                ]),
            },
        );
    }
    for definition in &schema.edge_types {
        let mut properties = BTreeMap::from([
            ("endpoints".into(), json!(definition.endpoints)),
            ("required".into(), json!(definition.required)),
            ("properties".into(), json!(definition.properties)),
            ("schema_definition".into(), Value::Bool(true)),
        ]);
        if let [endpoint] = definition.endpoints.as_slice() {
            properties.insert("from_type".into(), json!(endpoint.from_type));
            properties.insert("to_type".into(), json!(endpoint.to_type));
        }
        vertices.insert(
            definition.type_name.clone(),
            ProjectGraphVertex {
                id: definition.type_name.clone(),
                type_name: META_EDGE_TYPE.to_string(),
                label: definition.type_name.clone(),
                properties,
            },
        );
    }
    for vertex in fact_vertices {
        vertices.insert(vertex.id.clone(), vertex);
    }

    let mut all_edges = fixed_meta_edges();
    for type_name in schema.vertex_types.keys() {
        all_edges.push(ProjectGraphEdge {
            from: type_name.clone(),
            type_name: META_INSTANCE_OF.to_string(),
            to: META_VERTEX_TYPE.to_string(),
            properties: BTreeMap::new(),
        });
    }
    for edge_type in &schema.edge_types {
        all_edges.push(ProjectGraphEdge {
            from: edge_type.type_name.clone(),
            type_name: META_INSTANCE_OF.to_string(),
            to: META_EDGE_TYPE.to_string(),
            properties: BTreeMap::new(),
        });
        for endpoint in &edge_type.endpoints {
            all_edges.extend([
                ProjectGraphEdge {
                    from: edge_type.type_name.clone(),
                    type_name: META_FROM_TYPE.to_string(),
                    to: endpoint.from_type.clone(),
                    properties: BTreeMap::new(),
                },
                ProjectGraphEdge {
                    from: edge_type.type_name.clone(),
                    type_name: META_TO_TYPE.to_string(),
                    to: endpoint.to_type.clone(),
                    properties: BTreeMap::new(),
                },
            ]);
        }
    }
    for vertex in vertices.values() {
        if FIXED_META_VERTICES.contains(&vertex.id.as_str())
            || vertex.type_name == META_VERTEX_TYPE
            || vertex.type_name == META_EDGE_TYPE
        {
            continue;
        }
        all_edges.push(ProjectGraphEdge {
            from: vertex.id.clone(),
            type_name: META_INSTANCE_OF.to_string(),
            to: vertex.type_name.clone(),
            properties: BTreeMap::new(),
        });
    }
    all_edges.extend(fact_edges.iter().cloned());

    GraphGeneration {
        key,
        descriptor,
        schema,
        vertices,
        edges: all_edges,
        fingerprint,
        source_root,
    }
}

fn fixed_meta_vertices() -> BTreeMap<String, ProjectGraphVertex> {
    FIXED_META_VERTICES
        .into_iter()
        .map(|id| {
            let type_name = if matches!(id, META_VERTEX_TYPE | META_EDGE_TYPE) {
                META_VERTEX_TYPE
            } else {
                META_EDGE_TYPE
            };
            (
                id.to_string(),
                ProjectGraphVertex {
                    id: id.to_string(),
                    type_name: type_name.to_string(),
                    label: id.to_string(),
                    properties: BTreeMap::from([("fixed_floor".into(), Value::Bool(true))]),
                },
            )
        })
        .collect()
}

fn fixed_meta_edges() -> Vec<ProjectGraphEdge> {
    let mut edges = FIXED_META_VERTICES
        .into_iter()
        .map(|id| ProjectGraphEdge {
            from: id.to_string(),
            type_name: META_INSTANCE_OF.to_string(),
            to: if matches!(id, META_VERTEX_TYPE | META_EDGE_TYPE) {
                META_VERTEX_TYPE.to_string()
            } else {
                META_EDGE_TYPE.to_string()
            },
            properties: BTreeMap::new(),
        })
        .collect::<Vec<_>>();
    for edge_type in [META_FROM_TYPE, META_TO_TYPE] {
        edges.extend([
            ProjectGraphEdge {
                from: edge_type.to_string(),
                type_name: META_FROM_TYPE.to_string(),
                to: META_EDGE_TYPE.to_string(),
                properties: BTreeMap::new(),
            },
            ProjectGraphEdge {
                from: edge_type.to_string(),
                type_name: META_TO_TYPE.to_string(),
                to: META_VERTEX_TYPE.to_string(),
                properties: BTreeMap::new(),
            },
        ]);
    }
    edges
}

pub fn property_value_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

pub fn vertex_properties(
    generation: &GraphGeneration,
    vertex: &ProjectGraphVertex,
) -> BTreeMap<String, String> {
    let mut properties = BTreeMap::from([
        ("id".into(), vertex.id.clone()),
        ("type".into(), vertex.type_name.clone()),
        ("label".into(), vertex.label.clone()),
        ("scope_id".into(), generation.key.scope_id.clone()),
        ("graph_id".into(), generation.key.graph_id.clone()),
        (
            "generation".into(),
            generation.descriptor.generation.to_string(),
        ),
        ("namespace".into(), generation.schema.namespace.clone()),
        (
            "authority".into(),
            serde_json::to_value(generation.descriptor.authority)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "project".into()),
        ),
    ]);
    properties.extend(
        vertex
            .properties
            .iter()
            .map(|(name, value)| (format!("property.{name}"), property_value_string(value))),
    );
    properties
}

pub fn graph_defined_edge_types(generation: &GraphGeneration) -> BTreeSet<String> {
    generation
        .schema
        .edge_types
        .iter()
        .map(|definition| definition.type_name.clone())
        .collect()
}

#[derive(Debug, Default)]
pub struct ProjectGraphCatalog {
    entries: BTreeMap<GraphKey, Arc<GraphGeneration>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogPublishError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for CatalogPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CatalogPublishError {}

impl ProjectGraphCatalog {
    pub fn publish(
        &mut self,
        candidate: GraphGeneration,
    ) -> Result<Arc<GraphGeneration>, CatalogPublishError> {
        if let Some(current) = self.entries.get(&candidate.key) {
            if candidate.descriptor.generation < current.descriptor.generation {
                return Err(CatalogPublishError {
                    code: "generation.rollback".into(),
                    message: format!(
                        "graph generation {} is older than accepted generation {}",
                        candidate.descriptor.generation, current.descriptor.generation
                    ),
                });
            }
            if candidate.descriptor.generation == current.descriptor.generation {
                if candidate.fingerprint != current.fingerprint {
                    return Err(CatalogPublishError {
                        code: "generation.conflict".into(),
                        message: format!(
                            "graph generation {} changed content without advancing generation",
                            candidate.descriptor.generation
                        ),
                    });
                }
                return Ok(current.clone());
            }
        }
        let key = candidate.key.clone();
        let accepted = Arc::new(candidate);
        self.entries.insert(key, accepted.clone());
        Ok(accepted)
    }

    pub fn get(
        &self,
        scope_id: &str,
        graph_id: &str,
        include_local: bool,
    ) -> Option<Arc<GraphGeneration>> {
        let committed = GraphKey {
            scope_id: scope_id.to_string(),
            graph_id: graph_id.to_string(),
            source: GraphSource::Committed,
        };
        self.entries.get(&committed).cloned().or_else(|| {
            include_local.then(|| {
                self.entries
                    .get(&GraphKey {
                        scope_id: scope_id.to_string(),
                        graph_id: graph_id.to_string(),
                        source: GraphSource::LocalScratch,
                    })
                    .cloned()
            })?
        })
    }

    pub fn remove(&mut self, key: &GraphKey) {
        self.entries.remove(key);
    }

    pub fn remove_graph(&mut self, scope_id: &str, graph_id: &str, source: GraphSource) {
        self.entries.remove(&GraphKey {
            scope_id: scope_id.to_string(),
            graph_id: graph_id.to_string(),
            source,
        });
    }

    pub fn reconcile_source(
        &mut self,
        scope_id: &str,
        source: GraphSource,
        present_graph_ids: &BTreeSet<String>,
    ) {
        self.entries.retain(|key, _| {
            key.scope_id != scope_id
                || key.source != source
                || present_graph_ids.contains(&key.graph_id)
        });
    }

    pub fn vertex_count(&self, include_local: bool) -> usize {
        self.entries
            .values()
            .filter(|generation| include_local || generation.key.source == GraphSource::Committed)
            .map(|generation| generation.vertices.len())
            .sum()
    }
}
