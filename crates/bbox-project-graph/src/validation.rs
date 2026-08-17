use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::{
    ANNOTATION_EMBED_KEY, ANNOTATION_INDEX_KEY, EdgeTypeDefinition, GraphAuthority,
    GraphDescriptor, GraphSchema, GraphSource, HintDirection, ProjectGraphEdge, ProjectGraphVertex,
    PropertyIndexMode, RetentionPolicy, VertexTypeDefinition, is_annotated_property_term,
    property_term_body,
};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationError {
    pub code: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

impl ValidationError {
    pub fn new(
        code: impl Into<String>,
        file: impl Into<String>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            file: file.into(),
            line,
            message: message.into(),
        }
    }
}

pub fn validate_graph(
    directory_graph_id: &str,
    source: GraphSource,
    descriptor: &GraphDescriptor,
    schema: &GraphSchema,
    vertices: &[(usize, ProjectGraphVertex)],
    edges: &[(usize, ProjectGraphEdge)],
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    validate_descriptor(directory_graph_id, source, descriptor, schema, &mut errors);
    validate_schema(schema, &mut errors);
    validate_facts(schema, vertices, edges, &mut errors);
    errors
}

pub fn validate_graph_id(graph_id: &str) -> Result<(), String> {
    if graph_id.is_empty() {
        return Err("graph_id must not be empty".into());
    }
    if graph_id.contains(':') {
        return Err("graph_id must not contain ':'".into());
    }
    if graph_id == "."
        || graph_id == ".."
        || graph_id.contains('/')
        || graph_id.contains('\\')
        || graph_id.chars().any(char::is_control)
    {
        return Err("graph_id must be one safe path segment".into());
    }
    Ok(())
}

fn validate_descriptor(
    directory_graph_id: &str,
    source: GraphSource,
    descriptor: &GraphDescriptor,
    schema: &GraphSchema,
    errors: &mut Vec<ValidationError>,
) {
    if descriptor.descriptor_version != crate::DESCRIPTOR_VERSION {
        errors.push(ValidationError::new(
            "descriptor.unsupported_version",
            "graph.json",
            None,
            format!(
                "descriptor_version {} is unsupported; expected {}",
                descriptor.descriptor_version,
                crate::DESCRIPTOR_VERSION
            ),
        ));
    }
    if let Err(message) = validate_graph_id(&descriptor.graph_id) {
        errors.push(ValidationError::new(
            "descriptor.invalid_graph_id",
            "graph.json",
            None,
            message,
        ));
    }
    if descriptor.graph_id != directory_graph_id {
        errors.push(ValidationError::new(
            "descriptor.graph_id_mismatch",
            "graph.json",
            None,
            format!(
                "descriptor graph_id `{}` does not match directory `{directory_graph_id}`",
                descriptor.graph_id
            ),
        ));
    }
    if descriptor.schema_id.trim().is_empty() {
        errors.push(ValidationError::new(
            "descriptor.empty_schema_id",
            "graph.json",
            None,
            "schema_id must not be empty",
        ));
    }
    if descriptor.schema_version == 0 || descriptor.schema_version != schema.version {
        errors.push(ValidationError::new(
            "descriptor.schema_version_mismatch",
            "graph.json",
            None,
            format!(
                "descriptor schema_version {} must equal schema version {} and be positive",
                descriptor.schema_version, schema.version
            ),
        ));
    }
    if descriptor.generation == 0 {
        errors.push(ValidationError::new(
            "descriptor.invalid_generation",
            "graph.json",
            None,
            "generation must be positive",
        ));
    }
    // Authority and storage source are one decision, checked in both
    // directions. A project-authored graph can never claim connector
    // authority, and a connector projection can never be stored in a checkout
    // graph root, so a connector refresh has no path to a project-authored
    // graph and a checkout has no path to a connector projection.
    match descriptor.authority {
        GraphAuthority::Project => {
            if source == GraphSource::ConnectorManaged {
                errors.push(ValidationError::new(
                    "descriptor.authority_source_mismatch",
                    "graph.json",
                    None,
                    "project authority cannot use the connector-managed source projection store",
                ));
            }
            if descriptor.projection_version.is_some() || descriptor.source_connector.is_some() {
                errors.push(ValidationError::new(
                    "descriptor.project_projection_fields",
                    "graph.json",
                    None,
                    "project-authored graphs cannot declare projection_version or source_connector",
                ));
            }
        }
        GraphAuthority::Connector => {
            if source != GraphSource::ConnectorManaged {
                errors.push(ValidationError::new(
                    "descriptor.authority_source_mismatch",
                    "graph.json",
                    None,
                    "connector authority requires the connector-managed source projection store; \
                     connector graphs are not authorable through a checkout",
                ));
            }
            if descriptor
                .projection_version
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                errors.push(ValidationError::new(
                    "descriptor.missing_projection_version",
                    "graph.json",
                    None,
                    "connector-managed graphs require a non-empty projection_version",
                ));
            }
            if descriptor
                .source_connector
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            {
                errors.push(ValidationError::new(
                    "descriptor.missing_source_connector",
                    "graph.json",
                    None,
                    "connector-managed graphs require a non-empty source_connector",
                ));
            }
        }
    }
    let expected_retention = source.retention_policy();
    if descriptor.retention_policy != expected_retention {
        errors.push(ValidationError::new(
            "descriptor.retention_mismatch",
            "graph.json",
            None,
            format!(
                "retention_policy {:?} does not match graph location {:?}",
                descriptor.retention_policy, source
            )
            .to_ascii_lowercase(),
        ));
    }
    if source == GraphSource::Committed
        && descriptor.retention_policy != RetentionPolicy::ProjectOwned
    {
        errors.push(ValidationError::new(
            "descriptor.committed_custody",
            "graph.json",
            None,
            "committed graphs must use project_owned retention",
        ));
    }
}

fn validate_schema(schema: &GraphSchema, errors: &mut Vec<ValidationError>) {
    if schema.version == 0 {
        errors.push(ValidationError::new(
            "schema.invalid_version",
            "schema.json",
            None,
            "schema version must be positive",
        ));
    }
    if !valid_namespace(&schema.namespace) {
        errors.push(ValidationError::new(
            "schema.invalid_namespace",
            "schema.json",
            None,
            "namespace must start with an ASCII letter, contain only ASCII letters, digits, '_' or '-', and must not be meta or bbox",
        ));
    }

    let mut schema_vertex_ids = BTreeSet::new();
    for (type_name, definition) in &schema.vertex_types {
        validate_type_name(type_name, &schema.namespace, "vertex", errors);
        if !schema_vertex_ids.insert(type_name.clone()) {
            errors.push(ValidationError::new(
                "schema.duplicate_vertex_type",
                "schema.json",
                None,
                format!("duplicate vertex type `{type_name}`"),
            ));
        }
        validate_property_definition(
            &format!("vertex type `{type_name}`"),
            definition,
            &schema.index_policy,
            errors,
        );
    }

    let mut edge_type_names = HashSet::new();
    for definition in &schema.edge_types {
        validate_type_name(&definition.type_name, &schema.namespace, "edge", errors);
        if !edge_type_names.insert(definition.type_name.clone()) {
            errors.push(ValidationError::new(
                "schema.duplicate_edge_type",
                "schema.json",
                None,
                format!("duplicate edge type `{}`", definition.type_name),
            ));
        }
        if schema_vertex_ids.contains(&definition.type_name) {
            errors.push(ValidationError::new(
                "schema.type_id_collision",
                "schema.json",
                None,
                format!(
                    "`{}` cannot be both a vertex type and an edge type",
                    definition.type_name
                ),
            ));
        }
        if definition.endpoints.is_empty() {
            errors.push(ValidationError::new(
                "schema.empty_endpoint_pairs",
                "schema.json",
                None,
                format!(
                    "edge type `{}` must declare at least one endpoint pair",
                    definition.type_name
                ),
            ));
        }
        let mut endpoint_pairs = BTreeSet::new();
        for endpoint in &definition.endpoints {
            if !endpoint_pairs.insert((&endpoint.from_type, &endpoint.to_type)) {
                errors.push(ValidationError::new(
                    "schema.duplicate_endpoint_pair",
                    "schema.json",
                    None,
                    format!(
                        "edge type `{}` repeats endpoint pair (`{}`, `{}`)",
                        definition.type_name, endpoint.from_type, endpoint.to_type
                    ),
                ));
            }
            for (role, endpoint_type) in [("from", &endpoint.from_type), ("to", &endpoint.to_type)]
            {
                if !schema.vertex_types.contains_key(endpoint_type) {
                    errors.push(ValidationError::new(
                        "schema.missing_endpoint_type",
                        "schema.json",
                        None,
                        format!(
                            "edge type `{}` endpoint pair (`{}`, `{}`) {role} type `{endpoint_type}` is not a declared vertex type",
                            definition.type_name, endpoint.from_type, endpoint.to_type
                        ),
                    ));
                }
            }
        }
        validate_edge_property_definition(definition, &schema.index_policy, errors);
    }

    // Hints are validated against the WHOLE edge-type set, so this runs after
    // the edge loop rather than inside the vertex loop above.
    validate_next_hop_hints(schema, errors);

    let declared_vertex_types = schema_vertex_ids;
    validate_retrieval_policy(schema, &declared_vertex_types, errors);
}

/// Validate the schema-declared next-hop hints on every vertex type.
///
/// A hint is a retrieval instruction, so a wrong one sends every consumer one
/// hop into nothing. All three failures are schema errors, not silent drops:
/// an edge type this schema never declared, a direction no declared endpoint
/// pair supports, and the same (edge type, direction) hop declared twice on
/// one vertex type.
fn validate_next_hop_hints(schema: &GraphSchema, errors: &mut Vec<ValidationError>) {
    for (type_name, definition) in &schema.vertex_types {
        let mut seen_hops = BTreeSet::new();
        for hint in &definition.hints {
            if !seen_hops.insert((&hint.edge_type, hint.direction)) {
                errors.push(ValidationError::new(
                    "schema.duplicate_hint",
                    "schema.json",
                    None,
                    format!(
                        "vertex type `{type_name}` repeats next-hop hint (`{}`, {})",
                        hint.edge_type,
                        hint.direction.as_str()
                    ),
                ));
                continue;
            }
            let Some(edge_type) = schema
                .edge_types
                .iter()
                .find(|candidate| candidate.type_name == hint.edge_type)
            else {
                errors.push(ValidationError::new(
                    "schema.hint_unknown_edge_type",
                    "schema.json",
                    None,
                    format!(
                        "vertex type `{type_name}` next-hop hint references edge type `{}` which is not declared in this schema",
                        hint.edge_type
                    ),
                ));
                continue;
            };
            let supported = edge_type.endpoints.iter().any(|endpoint| {
                let endpoint_type = match hint.direction {
                    HintDirection::Out => &endpoint.from_type,
                    HintDirection::In => &endpoint.to_type,
                };
                endpoint_type == type_name
            });
            if !supported {
                errors.push(ValidationError::new(
                    "schema.hint_direction_mismatch",
                    "schema.json",
                    None,
                    format!(
                        "vertex type `{type_name}` next-hop hint declares edge type `{}` in direction {}, but no declared endpoint pair puts `{type_name}` on the `{}` side",
                        hint.edge_type,
                        hint.direction.as_str(),
                        hint.direction.endpoint_role()
                    ),
                ));
            }
        }
    }
}

/// Validate the per-graph retrieval policy block (unified-retrieval design
/// section 3.2).
///
/// A type-malformed block (a string where a bool belongs, an unknown key) is
/// refused at parse time as `json.malformed`, exactly like every other typed
/// schema field; the codes here cover what a parseable block can still get
/// wrong. An exclusion naming a type the schema does not declare is an error
/// rather than a no-op: a silently ignored exclusion is a policy the operator
/// believes is in force and is not.
fn validate_retrieval_policy(
    schema: &GraphSchema,
    declared_vertex_types: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    for excluded in &schema.index_policy.retrieval_excluded_types {
        if excluded.is_empty() || excluded.chars().any(char::is_whitespace) {
            errors.push(ValidationError::new(
                "schema.invalid_retrieval_policy",
                "schema.json",
                None,
                format!("index_policy exclusion `{excluded}` is not a well-formed type name"),
            ));
            continue;
        }
        if !declared_vertex_types.contains(excluded) {
            errors.push(ValidationError::new(
                "schema.unknown_excluded_type",
                "schema.json",
                None,
                format!(
                    "index_policy excludes vertex type `{excluded}`, which the schema does not declare"
                ),
            ));
        }
    }
}

fn validate_facts(
    schema: &GraphSchema,
    vertices: &[(usize, ProjectGraphVertex)],
    edges: &[(usize, ProjectGraphEdge)],
    errors: &mut Vec<ValidationError>,
) {
    let schema_ids = schema
        .vertex_types
        .keys()
        .chain(
            schema
                .edge_types
                .iter()
                .map(|definition| &definition.type_name),
        )
        .cloned()
        .collect::<HashSet<_>>();
    let mut by_id = HashMap::<String, (usize, &ProjectGraphVertex)>::new();
    for (line, vertex) in vertices {
        if vertex.id.trim().is_empty() || vertex.id.chars().any(char::is_control) {
            errors.push(ValidationError::new(
                "vertex.invalid_id",
                "vertices.jsonl",
                Some(*line),
                "vertex id must be non-empty and contain no control characters",
            ));
        }
        if uses_reserved_namespace(&vertex.id) {
            errors.push(ValidationError::new(
                "vertex.reserved_namespace",
                "vertices.jsonl",
                Some(*line),
                format!("fact vertex id `{}` uses a reserved namespace", vertex.id),
            ));
        }
        if schema_ids.contains(&vertex.id) {
            errors.push(ValidationError::new(
                "vertex.schema_id_collision",
                "vertices.jsonl",
                Some(*line),
                format!("fact vertex id `{}` is reserved by the schema", vertex.id),
            ));
        }
        if by_id.insert(vertex.id.clone(), (*line, vertex)).is_some() {
            errors.push(ValidationError::new(
                "vertex.duplicate_id",
                "vertices.jsonl",
                Some(*line),
                format!("duplicate vertex id `{}`", vertex.id),
            ));
        }
        if vertex.label.trim().is_empty() {
            errors.push(ValidationError::new(
                "vertex.empty_label",
                "vertices.jsonl",
                Some(*line),
                format!("vertex `{}` must have a non-empty label", vertex.id),
            ));
        }
        let Some(definition) = schema.vertex_types.get(&vertex.type_name) else {
            errors.push(ValidationError::new(
                "vertex.undeclared_type",
                "vertices.jsonl",
                Some(*line),
                format!(
                    "vertex `{}` uses undeclared type `{}`",
                    vertex.id, vertex.type_name
                ),
            ));
            continue;
        };
        validate_property_values(
            &format!("vertex `{}`", vertex.id),
            &vertex.properties,
            &definition.required,
            &definition.properties,
            "vertices.jsonl",
            *line,
            errors,
        );
    }

    let edge_types = schema
        .edge_types
        .iter()
        .map(|definition| (definition.type_name.as_str(), definition))
        .collect::<HashMap<_, _>>();
    let mut edge_keys = HashSet::new();
    for (line, edge) in edges {
        let key = (&edge.from, &edge.type_name, &edge.to);
        if !edge_keys.insert(key) {
            errors.push(ValidationError::new(
                "edge.duplicate_key",
                "edges.jsonl",
                Some(*line),
                format!(
                    "duplicate edge (`{}`, `{}`, `{}`)",
                    edge.from, edge.type_name, edge.to
                ),
            ));
        }
        let Some(definition) = edge_types.get(edge.type_name.as_str()) else {
            errors.push(ValidationError::new(
                "edge.undeclared_type",
                "edges.jsonl",
                Some(*line),
                format!("edge uses undeclared type `{}`", edge.type_name),
            ));
            continue;
        };
        let Some((_, from)) = by_id.get(&edge.from) else {
            errors.push(ValidationError::new(
                "edge.missing_source",
                "edges.jsonl",
                Some(*line),
                format!("edge source `{}` is missing", edge.from),
            ));
            continue;
        };
        let Some((_, to)) = by_id.get(&edge.to) else {
            errors.push(ValidationError::new(
                "edge.missing_target",
                "edges.jsonl",
                Some(*line),
                format!("edge target `{}` is missing", edge.to),
            ));
            continue;
        };
        if !definition.endpoints.iter().any(|endpoint| {
            from.type_name == endpoint.from_type && to.type_name == endpoint.to_type
        }) {
            errors.push(ValidationError::new(
                "edge.endpoint_type_mismatch",
                "edges.jsonl",
                Some(*line),
                format!(
                    "edge type `{}` does not allow endpoint types (`{}`, `{}`); declared pairs: {}",
                    edge.type_name,
                    from.type_name,
                    to.type_name,
                    format_endpoint_pairs(&definition.endpoints)
                ),
            ));
        }
        validate_property_values(
            &format!(
                "edge (`{}`, `{}`, `{}`)",
                edge.from, edge.type_name, edge.to
            ),
            &edge.properties,
            &definition.required,
            &definition.properties,
            "edges.jsonl",
            *line,
            errors,
        );
    }
}

fn format_endpoint_pairs(endpoints: &[crate::EdgeEndpointDefinition]) -> String {
    endpoints
        .iter()
        .map(|endpoint| format!("(`{}`, `{}`)", endpoint.from_type, endpoint.to_type))
        .collect::<Vec<_>>()
        .join(", ")
}

fn valid_namespace(namespace: &str) -> bool {
    let mut chars = namespace.chars();
    chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        && !matches!(namespace, "meta" | "bbox")
}

fn validate_type_name(
    type_name: &str,
    namespace: &str,
    kind: &str,
    errors: &mut Vec<ValidationError>,
) {
    let prefix = format!("{namespace}:");
    if !type_name.starts_with(&prefix) || type_name.len() == prefix.len() {
        errors.push(ValidationError::new(
            format!("schema.invalid_{kind}_type_name"),
            "schema.json",
            None,
            format!("{kind} type `{type_name}` must use namespace prefix `{prefix}`"),
        ));
    }
    if type_name.chars().any(char::is_control) {
        errors.push(ValidationError::new(
            format!("schema.invalid_{kind}_type_name"),
            "schema.json",
            None,
            format!("{kind} type `{type_name}` contains a control character"),
        ));
    }
}

fn validate_property_definition(
    owner: &str,
    definition: &VertexTypeDefinition,
    policy: &crate::GraphIndexPolicy,
    errors: &mut Vec<ValidationError>,
) {
    validate_definition_parts(
        owner,
        &definition.required,
        &definition.properties,
        policy,
        errors,
    );
}

fn validate_edge_property_definition(
    definition: &EdgeTypeDefinition,
    policy: &crate::GraphIndexPolicy,
    errors: &mut Vec<ValidationError>,
) {
    validate_definition_parts(
        &format!("edge type `{}`", definition.type_name),
        &definition.required,
        &definition.properties,
        policy,
        errors,
    );
}

fn validate_definition_parts(
    owner: &str,
    required: &[String],
    properties: &BTreeMap<String, Value>,
    policy: &crate::GraphIndexPolicy,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen = HashSet::new();
    for name in required {
        if !seen.insert(name) {
            errors.push(ValidationError::new(
                "schema.duplicate_required_property",
                "schema.json",
                None,
                format!("{owner} repeats required property `{name}`"),
            ));
        }
        if !properties.contains_key(name) {
            errors.push(ValidationError::new(
                "schema.undeclared_required_property",
                "schema.json",
                None,
                format!("{owner} requires undeclared property `{name}`"),
            ));
        }
    }
    for (name, term) in properties {
        validate_property_term(owner, name, term, policy, errors);
    }
}

/// Validate one property annotation and its per-graph gate. The annotation is
/// structural in M2: it is accepted, checked, and preserved, but nothing reads
/// it for indexing or embedding yet.
fn validate_property_annotations(
    owner: &str,
    path: &str,
    term: &Value,
    policy: &crate::GraphIndexPolicy,
    errors: &mut Vec<ValidationError>,
) {
    if let Some(index) = term.get(ANNOTATION_INDEX_KEY) {
        let valid = index
            .as_str()
            .is_some_and(|value| PropertyIndexMode::parse(value).is_some());
        if !valid {
            errors.push(ValidationError::new(
                "schema.invalid_index_annotation",
                "schema.json",
                None,
                format!(
                    "{owner} property `{path}` index annotation must be one of `none`, `word`, or `text`"
                ),
            ));
        }
    }
    match term.get(ANNOTATION_EMBED_KEY) {
        None => {}
        Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            if !policy.embeddings_enabled {
                errors.push(ValidationError::new(
                    "schema.embedding_not_enabled",
                    "schema.json",
                    None,
                    format!(
                        "{owner} property `{path}` opts into embedding, but this graph's index_policy does not enable embeddings"
                    ),
                ));
            }
        }
        Some(_) => errors.push(ValidationError::new(
            "schema.invalid_embed_annotation",
            "schema.json",
            None,
            format!("{owner} property `{path}` embed annotation must be a boolean"),
        )),
    }
}

fn validate_property_term(
    owner: &str,
    path: &str,
    term: &Value,
    policy: &crate::GraphIndexPolicy,
    errors: &mut Vec<ValidationError>,
) {
    if is_annotated_property_term(term) {
        validate_property_annotations(owner, path, term, policy, errors);
        validate_property_term(owner, path, property_term_body(term), policy, errors);
        return;
    }
    match term {
        Value::String(name) if matches!(name.as_str(), "string" | "number" | "boolean") => {}
        Value::Object(fields) if fields.len() == 1 && fields.contains_key("enum") => {
            let Some(members) = fields["enum"].as_array() else {
                errors.push(ValidationError::new(
                    "schema.invalid_enum_property_term",
                    "schema.json",
                    None,
                    format!(
                        "{owner} property `{path}` enum term must contain a non-empty array of unique strings"
                    ),
                ));
                return;
            };
            if members.is_empty() || members.iter().any(|member| !member.is_string()) {
                errors.push(ValidationError::new(
                    "schema.invalid_enum_property_term",
                    "schema.json",
                    None,
                    format!(
                        "{owner} property `{path}` enum term must contain a non-empty array of unique strings"
                    ),
                ));
                return;
            }
            let mut unique = BTreeSet::new();
            for member in members {
                let member = member.as_str().expect("enum members checked as strings");
                if !unique.insert(member) {
                    errors.push(ValidationError::new(
                        "schema.duplicate_enum_member",
                        "schema.json",
                        None,
                        format!(
                            "{owner} property `{path}` enum term repeats member `{member}`"
                        ),
                    ));
                }
            }
        }
        Value::Object(fields) if !fields.is_empty() => {
            for (name, nested) in fields {
                validate_property_term(owner, &format!("{path}.{name}"), nested, policy, errors);
            }
        }
        Value::Array(items) if items.len() == 1 => {
            validate_property_term(owner, &format!("{path}[]"), &items[0], policy, errors);
        }
        _ => errors.push(ValidationError::new(
            "schema.invalid_property_term",
            "schema.json",
            None,
            format!(
                "{owner} property `{path}` must be string, number, boolean, a non-empty object, or a one-term array"
            ),
        )),
    }
}

fn validate_property_values(
    owner: &str,
    values: &BTreeMap<String, Value>,
    required: &[String],
    definitions: &BTreeMap<String, Value>,
    file: &str,
    line: usize,
    errors: &mut Vec<ValidationError>,
) {
    for name in required {
        if !values.contains_key(name) {
            errors.push(ValidationError::new(
                "property.missing_required",
                file,
                Some(line),
                format!("{owner} is missing required property `{name}`"),
            ));
        }
    }
    for (name, value) in values {
        let Some(term) = definitions.get(name) else {
            errors.push(ValidationError::new(
                "property.undeclared",
                file,
                Some(line),
                format!("{owner} has undeclared property `{name}`"),
            ));
            continue;
        };
        validate_property_shape(owner, name, value, term, file, line, errors);
    }
}

fn validate_property_shape(
    owner: &str,
    path: &str,
    value: &Value,
    term: &Value,
    file: &str,
    line: usize,
    errors: &mut Vec<ValidationError>,
) {
    // Annotations are retrieval metadata, not shape. Values are always checked
    // against the annotated term's body, so annotating a property never
    // changes which values it accepts.
    let term = property_term_body(term);
    let matches = match term {
        Value::String(kind) => match kind.as_str() {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            _ => false,
        },
        Value::Object(fields) if fields.len() == 1 && fields.contains_key("enum") => {
            let Some(candidate) = value.as_str() else {
                return push_property_shape_mismatch(owner, path, file, line, errors);
            };
            let allowed = fields["enum"]
                .as_array()
                .is_some_and(|members| members.iter().any(|member| member == candidate));
            if !allowed {
                let declared = fields["enum"]
                    .as_array()
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|member| format!("`{member}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                errors.push(ValidationError::new(
                    "property.enum_violation",
                    file,
                    Some(line),
                    format!(
                        "{owner} property `{path}` value `{candidate}` is not one of: {declared}"
                    ),
                ));
            }
            return;
        }
        Value::Object(fields) => match value.as_object() {
            Some(object) => {
                for field in object.keys() {
                    if !fields.contains_key(field) {
                        errors.push(ValidationError::new(
                            "property.undeclared_nested",
                            file,
                            Some(line),
                            format!("{owner} property `{path}.{field}` is undeclared"),
                        ));
                    }
                }
                for (field, nested_term) in fields {
                    match object.get(field) {
                        Some(nested_value) => validate_property_shape(
                            owner,
                            &format!("{path}.{field}"),
                            nested_value,
                            nested_term,
                            file,
                            line,
                            errors,
                        ),
                        None => errors.push(ValidationError::new(
                            "property.missing_nested",
                            file,
                            Some(line),
                            format!("{owner} property `{path}` is missing nested field `{field}`"),
                        )),
                    }
                }
                true
            }
            None => false,
        },
        Value::Array(items) if items.len() == 1 => match value.as_array() {
            Some(values) => {
                for (index, nested_value) in values.iter().enumerate() {
                    validate_property_shape(
                        owner,
                        &format!("{path}[{index}]"),
                        nested_value,
                        &items[0],
                        file,
                        line,
                        errors,
                    );
                }
                true
            }
            None => false,
        },
        _ => false,
    };
    if !matches {
        push_property_shape_mismatch(owner, path, file, line, errors);
    }
}

fn push_property_shape_mismatch(
    owner: &str,
    path: &str,
    file: &str,
    line: usize,
    errors: &mut Vec<ValidationError>,
) {
    errors.push(ValidationError::new(
        "property.shape_mismatch",
        file,
        Some(line),
        format!("{owner} property `{path}` does not match its declared JSON shape"),
    ));
}

fn uses_reserved_namespace(id: &str) -> bool {
    id.starts_with("meta:") || id.starts_with("bbox:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphAuthority, GraphScope, NextHopHint};
    use serde_json::json;

    fn descriptor() -> GraphDescriptor {
        GraphDescriptor {
            descriptor_version: 1,
            scope: GraphScope::Project,
            graph_id: "repo".into(),
            authority: GraphAuthority::Project,
            schema_id: "repo-schema".into(),
            schema_version: 1,
            projection_version: None,
            source_connector: None,
            retention_policy: RetentionPolicy::ProjectOwned,
            generation: 1,
        }
    }

    fn schema() -> GraphSchema {
        GraphSchema {
            version: 1,
            namespace: "repo".into(),
            vertex_types: BTreeMap::from([(
                "repo:Claim".into(),
                VertexTypeDefinition {
                    required: vec!["source".into()],
                    properties: BTreeMap::from([(
                        "source".into(),
                        json!({"path": "string", "tags": ["string"]}),
                    )]),
                    hints: Vec::new(),
                },
            )]),
            edge_types: Vec::new(),
            index_policy: crate::GraphIndexPolicy::default(),
        }
    }

    #[test]
    fn nested_objects_and_arrays_validate_structurally() {
        let vertices = vec![(
            1,
            ProjectGraphVertex {
                id: "claim-1".into(),
                type_name: "repo:Claim".into(),
                label: "claim".into(),
                properties: BTreeMap::from([(
                    "source".into(),
                    json!({"path": "PROJECT.md", "tags": ["design", "graph"]}),
                )]),
            },
        )];
        assert!(
            validate_graph(
                "repo",
                GraphSource::Committed,
                &descriptor(),
                &schema(),
                &vertices,
                &[]
            )
            .is_empty()
        );
    }

    #[test]
    fn nested_shape_mismatch_is_rejected() {
        let vertices = vec![(
            4,
            ProjectGraphVertex {
                id: "claim-1".into(),
                type_name: "repo:Claim".into(),
                label: "claim".into(),
                properties: BTreeMap::from([(
                    "source".into(),
                    json!({"path": 42, "tags": ["design", false]}),
                )]),
            },
        )];
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema(),
            &vertices,
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "property.shape_mismatch" && error.line == Some(4))
        );
    }

    #[test]
    fn invalid_enum_terms_are_rejected() {
        for (term, expected_code) in [
            (json!({"enum": []}), "schema.invalid_enum_property_term"),
            (
                json!({"enum": ["draft", 2]}),
                "schema.invalid_enum_property_term",
            ),
            (
                json!({"enum": ["draft", "draft"]}),
                "schema.duplicate_enum_member",
            ),
        ] {
            let mut schema = schema();
            schema
                .vertex_types
                .get_mut("repo:Claim")
                .unwrap()
                .properties
                .insert("status".into(), term);
            let errors = validate_graph(
                "repo",
                GraphSource::Committed,
                &descriptor(),
                &schema,
                &[],
                &[],
            );
            assert!(
                errors.iter().any(|error| error.code == expected_code),
                "{errors:?}"
            );
        }
    }

    fn connector_descriptor() -> GraphDescriptor {
        GraphDescriptor {
            authority: GraphAuthority::Connector,
            projection_version: Some("dataset-v1".into()),
            source_connector: Some("synthetic-api".into()),
            retention_policy: RetentionPolicy::ConnectorManaged,
            ..descriptor()
        }
    }

    /// A connector-authored descriptor dropped into a checkout graph root is
    /// refused. This is the checkout-lane half of the authority split: there is
    /// no way to author a connector graph by committing files.
    #[test]
    fn connector_authority_is_refused_in_a_checkout_graph_root() {
        for source in [GraphSource::Committed, GraphSource::LocalScratch] {
            let errors =
                validate_graph("repo", source, &connector_descriptor(), &schema(), &[], &[]);
            assert!(
                errors
                    .iter()
                    .any(|error| error.code == "descriptor.authority_source_mismatch"),
                "{source:?}: {errors:?}"
            );
        }
    }

    /// The mirror refusal: a connector refresh cannot install a
    /// project-authored descriptor into the source projection store, so a
    /// refresh has no path to project-authored facts.
    #[test]
    fn project_authority_is_refused_in_the_source_projection_store() {
        let errors = validate_graph(
            "repo",
            GraphSource::ConnectorManaged,
            &descriptor(),
            &schema(),
            &[],
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "descriptor.authority_source_mismatch"),
            "{errors:?}"
        );
    }

    #[test]
    fn connector_descriptors_require_projection_version_and_source_connector() {
        let mut descriptor = connector_descriptor();
        descriptor.projection_version = Some("   ".into());
        descriptor.source_connector = None;
        let errors = validate_graph(
            "repo",
            GraphSource::ConnectorManaged,
            &descriptor,
            &schema(),
            &[],
            &[],
        );
        for expected in [
            "descriptor.missing_projection_version",
            "descriptor.missing_source_connector",
        ] {
            assert!(
                errors.iter().any(|error| error.code == expected),
                "{expected} missing from {errors:?}"
            );
        }
    }

    /// A well-formed connector descriptor validates in its own store.
    #[test]
    fn connector_descriptors_validate_in_the_source_projection_store() {
        let errors = validate_graph(
            "repo",
            GraphSource::ConnectorManaged,
            &connector_descriptor(),
            &schema(),
            &[],
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// An annotated property term is accepted, its body still governs values,
    /// and the annotation round trips structurally (M9 reads it; M2 only
    /// preserves it).
    #[test]
    fn annotated_property_terms_validate_their_body_and_preserve_annotations() {
        let mut schema = schema();
        schema.index_policy.embeddings_enabled = true;
        schema
            .vertex_types
            .get_mut("repo:Claim")
            .unwrap()
            .properties
            .insert(
                "summary".into(),
                json!({"type": "string", "index": "text", "embed": true}),
            );
        let vertices = vec![(
            1,
            ProjectGraphVertex {
                id: "claim-1".into(),
                type_name: "repo:Claim".into(),
                label: "claim".into(),
                properties: BTreeMap::from([
                    ("source".into(), json!({"path": "PROJECT.md", "tags": []})),
                    ("summary".into(), json!("a summary")),
                ]),
            },
        )];
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &vertices,
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");

        let term = &schema.vertex_types["repo:Claim"].properties["summary"];
        assert!(crate::is_annotated_property_term(term));
        assert_eq!(crate::property_term_body(term), &json!("string"));
        let annotations = crate::property_annotations(term);
        assert_eq!(annotations.index, crate::PropertyIndexMode::Text);
        assert!(annotations.embed);

        // The annotated term still rejects a value of the wrong shape.
        let mismatched = vec![(
            2,
            ProjectGraphVertex {
                id: "claim-2".into(),
                type_name: "repo:Claim".into(),
                label: "claim".into(),
                properties: BTreeMap::from([
                    ("source".into(), json!({"path": "PROJECT.md", "tags": []})),
                    ("summary".into(), json!(7)),
                ]),
            },
        )];
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &mismatched,
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "property.shape_mismatch"),
            "{errors:?}"
        );
    }

    /// A bare `{"type": ...}` object keeps its pre-annotation meaning, so no
    /// existing schema changes meaning when annotations land.
    #[test]
    fn a_bare_type_field_is_still_a_nested_object_term() {
        let term = json!({"type": "string"});
        assert!(!crate::is_annotated_property_term(&term));
        assert_eq!(crate::property_term_body(&term), &term);
        assert_eq!(
            crate::property_annotations(&term),
            crate::PropertyAnnotations::default()
        );
    }

    /// Embedding is per-kind opt-in UNDER a per-graph policy. Opting a
    /// property in without enabling the graph policy is an error, never a
    /// silent downgrade.
    #[test]
    fn embedding_opt_in_requires_the_graph_index_policy() {
        let mut schema = schema();
        schema
            .vertex_types
            .get_mut("repo:Claim")
            .unwrap()
            .properties
            .insert("summary".into(), json!({"type": "string", "embed": true}));
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "schema.embedding_not_enabled"),
            "{errors:?}"
        );

        schema.index_policy.embeddings_enabled = true;
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn invalid_annotation_values_are_rejected() {
        for (term, expected) in [
            (
                json!({"type": "string", "index": "everything"}),
                "schema.invalid_index_annotation",
            ),
            (
                json!({"type": "string", "index": 3}),
                "schema.invalid_index_annotation",
            ),
            (
                json!({"type": "string", "embed": "yes"}),
                "schema.invalid_embed_annotation",
            ),
        ] {
            let mut schema = schema();
            schema
                .vertex_types
                .get_mut("repo:Claim")
                .unwrap()
                .properties
                .insert("summary".into(), term);
            let errors = validate_graph(
                "repo",
                GraphSource::Committed,
                &descriptor(),
                &schema,
                &[],
                &[],
            );
            assert!(
                errors.iter().any(|error| error.code == expected),
                "{expected} missing from {errors:?}"
            );
        }
    }

    #[test]
    fn empty_endpoint_list_is_rejected() {
        let mut schema = schema();
        schema.edge_types.push(EdgeTypeDefinition {
            type_name: "repo:CITES".into(),
            endpoints: Vec::new(),
            required: Vec::new(),
            properties: BTreeMap::new(),
        });
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "schema.empty_endpoint_pairs")
        );
    }

    /// The retrieval policy extension stays additive: a schema written before
    /// the new fields existed keeps its meaning, and text retrieval defaults
    /// ON because the conservative default already lives in the per-property
    /// annotations (unified-retrieval design section 3.2).
    #[test]
    fn retrieval_policy_defaults_are_additive() {
        let schema: GraphSchema = serde_json::from_value(json!({
            "version": 1,
            "namespace": "repo",
            "vertex_types": {
                "repo:Claim": {
                    "required": [],
                    "properties": {}
                }
            },
            "edge_types": []
        }))
        .unwrap();
        assert!(!schema.index_policy.embeddings_enabled);
        assert!(schema.index_policy.text_retrieval_enabled);
        assert!(schema.index_policy.retrieval_excluded_types.is_empty());

        let wire = serde_json::to_value(&schema.index_policy).unwrap();
        assert_eq!(
            wire,
            json!({
                "embeddings_enabled": false,
                "text_retrieval_enabled": true,
                "retrieval_excluded_types": []
            })
        );
        let parsed: crate::GraphIndexPolicy = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, schema.index_policy);
    }

    #[test]
    fn retrieval_exclusion_naming_an_undeclared_type_is_an_error() {
        let mut schema = schema();
        schema
            .index_policy
            .retrieval_excluded_types
            .insert("repo:Secret".into());
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "schema.unknown_excluded_type"),
            "{errors:?}"
        );

        schema.index_policy.retrieval_excluded_types.clear();
        schema
            .index_policy
            .retrieval_excluded_types
            .insert("repo:Claim".into());
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn malformed_retrieval_exclusions_are_rejected() {
        for malformed in ["", "repo: Claim", "repo:Claim\n"] {
            let mut schema = schema();
            schema
                .index_policy
                .retrieval_excluded_types
                .insert(malformed.to_string());
            let errors = validate_graph(
                "repo",
                GraphSource::Committed,
                &descriptor(),
                &schema,
                &[],
                &[],
            );
            assert!(
                errors
                    .iter()
                    .any(|error| error.code == "schema.invalid_retrieval_policy"),
                "{malformed:?} missing code in {errors:?}"
            );
        }
    }

    /// A schema carrying next-hop hints round-trips and validates clean when
    /// every hint names a declared edge type in a direction its endpoints
    /// support.
    #[test]
    fn authored_hints_round_trip_and_validate() {
        let raw = r#"{
            "version": 1,
            "namespace": "repo",
            "vertex_types": {
                "repo:Claim": {
                    "hints": [
                        {"edge_type": "repo:CITES", "direction": "out", "label": "cited basis"},
                        {"edge_type": "repo:CITES", "direction": "in", "label": "citing claims"}
                    ]
                }
            },
            "edge_types": [
                {"type": "repo:CITES", "from_type": "repo:Claim", "to_type": "repo:Claim"}
            ]
        }"#;
        let parsed: GraphSchema = serde_json::from_str(raw).unwrap();
        let hints = &parsed.vertex_types["repo:Claim"].hints;
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].edge_type, "repo:CITES");
        assert_eq!(hints[0].direction, HintDirection::Out);
        assert_eq!(hints[0].label, "cited basis");
        assert_eq!(hints[1].direction, HintDirection::In);

        // Array order is priority order, so it must survive serialization.
        let reserialized: GraphSchema =
            serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
        assert_eq!(reserialized.vertex_types["repo:Claim"].hints, *hints);

        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &parsed,
            &[],
            &[],
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn hint_naming_an_undeclared_edge_type_is_rejected() {
        let mut schema = schema();
        schema
            .vertex_types
            .get_mut("repo:Claim")
            .unwrap()
            .hints
            .push(NextHopHint {
                edge_type: "repo:GHOST".into(),
                direction: HintDirection::Out,
                label: "nowhere".into(),
            });
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        let error = errors
            .iter()
            .find(|error| error.code == "schema.hint_unknown_edge_type")
            .unwrap_or_else(|| panic!("{errors:?}"));
        assert!(error.message.contains("repo:GHOST"), "{}", error.message);
    }

    #[test]
    fn hint_direction_no_endpoint_pair_supports_is_rejected() {
        let mut schema = schema();
        schema.edge_types.push(EdgeTypeDefinition {
            type_name: "repo:CITES".into(),
            endpoints: vec![crate::EdgeEndpointDefinition {
                from_type: "repo:Claim".into(),
                to_type: "repo:Claim".into(),
            }],
            required: Vec::new(),
            properties: BTreeMap::new(),
        });
        schema.vertex_types.insert(
            "repo:Subject".into(),
            VertexTypeDefinition {
                required: Vec::new(),
                properties: BTreeMap::new(),
                // repo:CITES never touches repo:Subject on either end.
                hints: vec![NextHopHint {
                    edge_type: "repo:CITES".into(),
                    direction: HintDirection::In,
                    label: "citing claims".into(),
                }],
            },
        );
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        let error = errors
            .iter()
            .find(|error| error.code == "schema.hint_direction_mismatch")
            .unwrap_or_else(|| panic!("{errors:?}"));
        assert!(error.message.contains("`to` side"), "{}", error.message);
    }

    #[test]
    fn the_same_hop_declared_twice_on_one_vertex_type_is_rejected() {
        let mut schema = schema();
        schema.edge_types.push(EdgeTypeDefinition {
            type_name: "repo:CITES".into(),
            endpoints: vec![crate::EdgeEndpointDefinition {
                from_type: "repo:Claim".into(),
                to_type: "repo:Claim".into(),
            }],
            required: Vec::new(),
            properties: BTreeMap::new(),
        });
        let hints = &mut schema.vertex_types.get_mut("repo:Claim").unwrap().hints;
        for label in ["cited basis", "cited basis again"] {
            hints.push(NextHopHint {
                edge_type: "repo:CITES".into(),
                direction: HintDirection::Out,
                label: label.into(),
            });
        }
        let errors = validate_graph(
            "repo",
            GraphSource::Committed,
            &descriptor(),
            &schema,
            &[],
            &[],
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == "schema.duplicate_hint"),
            "{errors:?}"
        );
    }
}
