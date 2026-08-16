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
    /// A connector-managed projection of remote state. Connector authority is
    /// only legal for graphs held in the dedicated source projection store;
    /// it can never be authored through a checkout graph root.
    Connector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPolicy {
    ProjectOwned,
    LocalScratch,
    ConnectorManaged,
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
    /// Per-graph retrieval policy. Schema authors annotate individual
    /// properties for text indexing and embedding participation; this policy
    /// is the per-graph gate those annotations sit under. Structural only in
    /// M2: nothing indexes or embeds from it yet.
    #[serde(default)]
    pub index_policy: GraphIndexPolicy,
}

/// Per-graph retrieval policy. Conservative by construction: with the default
/// policy no property is embedded, whatever a property annotation asks for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GraphIndexPolicy {
    /// Whether any property in this graph may participate in embeddings.
    /// A property that opts into embedding while this is false is a schema
    /// error, not a silent downgrade.
    #[serde(default)]
    pub embeddings_enabled: bool,

    /// Whether this graph participates in text retrieval at all. Default
    /// true: the conservative default lives in the per-property annotations,
    /// not here, and a graph whose properties are all unannotated already
    /// contributes only labels. The field exists to be turned OFF.
    #[serde(default = "default_text_retrieval_enabled")]
    pub text_retrieval_enabled: bool,

    /// Vertex types excluded from retrieval regardless of annotation. The
    /// operator escape hatch for a shipped connector schema the tenant does
    /// not own. Policy only ever subtracts: it can widen participation past
    /// nothing that a property annotation declined to request.
    #[serde(default)]
    pub retrieval_excluded_types: BTreeSet<String>,
}

impl Default for GraphIndexPolicy {
    /// Text retrieval defaults ON because the conservative default already
    /// lives in the per-property annotations: with no annotations a graph
    /// contributes labels and nothing else. A default-OFF gate would make
    /// every schema author opt in twice to get the documented default.
    fn default() -> Self {
        Self {
            embeddings_enabled: false,
            text_retrieval_enabled: default_text_retrieval_enabled(),
            retrieval_excluded_types: BTreeSet::new(),
        }
    }
}

fn default_text_retrieval_enabled() -> bool {
    true
}

/// How a property participates in text retrieval. Vertex labels are word
/// indexed by default; every property is `none` unless annotated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyIndexMode {
    #[default]
    None,
    Word,
    Text,
}

impl PropertyIndexMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "word" => Some(Self::Word),
            "text" => Some(Self::Text),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Word => "word",
            Self::Text => "text",
        }
    }
}

/// The retrieval annotations carried by one schema property term.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PropertyAnnotations {
    pub index: PropertyIndexMode,
    pub embed: bool,
}

/// The reserved keys of an annotated property term.
pub const ANNOTATION_TYPE_KEY: &str = "type";
pub const ANNOTATION_INDEX_KEY: &str = "index";
pub const ANNOTATION_EMBED_KEY: &str = "embed";

/// Whether a property term is an ANNOTATED term rather than a nested object
/// term.
///
/// The discriminator is deliberately narrow so no existing schema changes
/// meaning: an annotated term is an object whose keys are a subset of
/// `{type, index, embed}`, that carries `type`, and that carries at least one
/// of `index` / `embed`. A bare `{"type": "string"}` therefore keeps its
/// existing meaning (an object with one field named `type`), and only an
/// author who actually writes an annotation opts into the new form.
pub fn is_annotated_property_term(term: &Value) -> bool {
    let Some(fields) = term.as_object() else {
        return false;
    };
    fields.contains_key(ANNOTATION_TYPE_KEY)
        && (fields.contains_key(ANNOTATION_INDEX_KEY) || fields.contains_key(ANNOTATION_EMBED_KEY))
        && fields.keys().all(|key| {
            matches!(
                key.as_str(),
                ANNOTATION_TYPE_KEY | ANNOTATION_INDEX_KEY | ANNOTATION_EMBED_KEY
            )
        })
}

/// The structural term inside a property term: the annotated `type` body when
/// the term is annotated, the term itself otherwise. Value validation always
/// runs against this, so annotating a property never changes what values it
/// accepts.
pub fn property_term_body(term: &Value) -> &Value {
    if is_annotated_property_term(term) {
        &term[ANNOTATION_TYPE_KEY]
    } else {
        term
    }
}

/// The annotations a property term declares. Unannotated terms carry the
/// conservative default (`index: none`, `embed: false`).
pub fn property_annotations(term: &Value) -> PropertyAnnotations {
    if !is_annotated_property_term(term) {
        return PropertyAnnotations::default();
    }
    PropertyAnnotations {
        index: term
            .get(ANNOTATION_INDEX_KEY)
            .and_then(Value::as_str)
            .and_then(PropertyIndexMode::parse)
            .unwrap_or_default(),
        embed: term
            .get(ANNOTATION_EMBED_KEY)
            .and_then(Value::as_bool)
            .unwrap_or_default(),
    }
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
    /// Held by the dedicated source projection store, outside every checkout
    /// graph root. Connector-managed graphs have no relative root by
    /// construction, which is what keeps them unreachable from the checkout
    /// authoring lane.
    ConnectorManaged,
}

impl GraphSource {
    pub fn retention_policy(self) -> RetentionPolicy {
        match self {
            Self::Committed => RetentionPolicy::ProjectOwned,
            Self::LocalScratch => RetentionPolicy::LocalScratch,
            Self::ConnectorManaged => RetentionPolicy::ConnectorManaged,
        }
    }

    /// The checkout-relative directory that holds this source's graphs, or
    /// `None` for sources that are not file-backed by a checkout. Callers that
    /// discover or locate graphs on disk must treat `None` as "no such graph
    /// here" rather than defaulting to a project root.
    pub fn relative_root(self) -> Option<&'static str> {
        match self {
            Self::Committed => Some(".bbox/graphs"),
            Self::LocalScratch => Some(".bbox/local/graphs"),
            Self::ConnectorManaged => None,
        }
    }

    /// The AUTHORITY-plane label for this source, used in diagnostics.
    ///
    /// Deliberately not the read-surface vocabulary: the read plane labels an
    /// accepted checkout graph `published` (and a workspace's own uncommitted
    /// one `provisional`), which is a visibility distinction this enum does
    /// not carry. Do not wire this into tool output.
    pub fn authority_label(self) -> &'static str {
        match self {
            Self::Committed => "project",
            Self::LocalScratch => "provisional",
            Self::ConnectorManaged => "connector",
        }
    }

    /// Whether a graph from this source may be authored or mutated through a
    /// checkout lane.
    pub fn is_checkout_authored(self) -> bool {
        matches!(self, Self::Committed | Self::LocalScratch)
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
    /// Count of vertices read directly from vertices.jsonl, before the
    /// schema-as-data meta vertices (vertex/edge type definitions and the
    /// fixed meta floor) are projected in. This is the number an author
    /// comparing against their source files expects to see.
    pub authored_vertex_count: usize,
    /// Count of edges read directly from edges.jsonl, before the
    /// schema-as-data meta edges (`meta:INSTANCE_OF`, `meta:FROM_TYPE`,
    /// `meta:TO_TYPE`) are projected in. This is the number an author
    /// comparing against their source files expects to see.
    pub authored_edge_count: usize,
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

/// Reflect a validated descriptor, schema, and fact set into an accepted
/// generation: the schema is projected in as meta vertices and edges on top of
/// the authored facts. Shared by the checkout loader and the source projection
/// store so both authority planes reflect identically.
pub fn build_generation(
    key: GraphKey,
    descriptor: GraphDescriptor,
    schema: GraphSchema,
    fact_vertices: Vec<ProjectGraphVertex>,
    fact_edges: Vec<ProjectGraphEdge>,
    fingerprint: String,
    source_root: PathBuf,
) -> GraphGeneration {
    let authored_vertex_count = fact_vertices.len();
    let authored_edge_count = fact_edges.len();
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
        authored_vertex_count,
        authored_edge_count,
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
        // One graph id in one scope has exactly one authority. A connector
        // refresh must never be able to shadow or replace a project-authored
        // graph, and a checkout must never be able to shadow a connector
        // projection; both directions fail closed here.
        if let Some(existing) = self.entries.keys().find(|key| {
            key.scope_id == candidate.key.scope_id
                && key.graph_id == candidate.key.graph_id
                && key.source != candidate.key.source
        }) {
            return Err(CatalogPublishError {
                code: "graph.ambiguous_source".into(),
                message: format!(
                    "graph `{}` already has an accepted generation from the {} source; \
                     one graph id cannot hold two authorities",
                    candidate.key.graph_id,
                    existing.source.authority_label()
                ),
            });
        }
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
        self.entries
            .get(&committed)
            .cloned()
            .or_else(|| {
                // Connector-managed graphs are always visible: they are
                // read-only, so they need no local opt-in.
                self.entries
                    .get(&GraphKey {
                        scope_id: scope_id.to_string(),
                        graph_id: graph_id.to_string(),
                        source: GraphSource::ConnectorManaged,
                    })
                    .cloned()
            })
            .or_else(|| {
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
            .filter(|generation| {
                include_local || generation.key.source != GraphSource::LocalScratch
            })
            .map(|generation| generation.vertices.len())
            .sum()
    }

    /// Accepted generations, optionally including local scratch graphs.
    /// Connector-managed generations are always included.
    pub fn generations(&self, include_local: bool) -> Vec<Arc<GraphGeneration>> {
        self.entries
            .values()
            .filter(|generation| {
                include_local || generation.key.source != GraphSource::LocalScratch
            })
            .cloned()
            .collect()
    }
}
