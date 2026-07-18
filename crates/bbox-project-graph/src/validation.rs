use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use crate::{
    EdgeTypeDefinition, GraphAuthority, GraphDescriptor, GraphSchema, GraphSource,
    ProjectGraphEdge, ProjectGraphVertex, RetentionPolicy, VertexTypeDefinition,
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
    match descriptor.authority {
        GraphAuthority::Project => {
            if source == GraphSource::ConnectorManaged {
                errors.push(ValidationError::new(
                    "descriptor.authority_source_mismatch",
                    "graph.json",
                    None,
                    "project authority cannot use connector-managed storage",
                ));
            }
            if descriptor.projection_version.is_some() || descriptor.source_connector.is_some() {
                errors.push(ValidationError::new(
                    "descriptor.project_projection_fields",
                    "graph.json",
                    None,
                    "project-owned graphs cannot declare projection_version or source_connector",
                ));
            }
        }
        GraphAuthority::Connector => {
            if source != GraphSource::ConnectorManaged {
                errors.push(ValidationError::new(
                    "descriptor.authority_source_mismatch",
                    "graph.json",
                    None,
                    "connector authority requires connector-managed storage",
                ));
            }
            if descriptor
                .projection_version
                .as_deref()
                .is_none_or(str::is_empty)
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
                .is_none_or(str::is_empty)
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
        validate_property_definition(&format!("vertex type `{type_name}`"), definition, errors);
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
        for (role, endpoint) in [
            ("from_type", &definition.from_type),
            ("to_type", &definition.to_type),
        ] {
            if !schema.vertex_types.contains_key(endpoint) {
                errors.push(ValidationError::new(
                    "schema.missing_endpoint_type",
                    "schema.json",
                    None,
                    format!(
                        "edge type `{}` {role} `{endpoint}` is not a declared vertex type",
                        definition.type_name
                    ),
                ));
            }
        }
        validate_edge_property_definition(definition, errors);
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
        if from.type_name != definition.from_type {
            errors.push(ValidationError::new(
                "edge.source_type_mismatch",
                "edges.jsonl",
                Some(*line),
                format!(
                    "edge source `{}` has type `{}`; `{}` requires `{}`",
                    edge.from, from.type_name, edge.type_name, definition.from_type
                ),
            ));
        }
        if to.type_name != definition.to_type {
            errors.push(ValidationError::new(
                "edge.target_type_mismatch",
                "edges.jsonl",
                Some(*line),
                format!(
                    "edge target `{}` has type `{}`; `{}` requires `{}`",
                    edge.to, to.type_name, edge.type_name, definition.to_type
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
    errors: &mut Vec<ValidationError>,
) {
    validate_definition_parts(owner, &definition.required, &definition.properties, errors);
}

fn validate_edge_property_definition(
    definition: &EdgeTypeDefinition,
    errors: &mut Vec<ValidationError>,
) {
    validate_definition_parts(
        &format!("edge type `{}`", definition.type_name),
        &definition.required,
        &definition.properties,
        errors,
    );
}

fn validate_definition_parts(
    owner: &str,
    required: &[String],
    properties: &BTreeMap<String, Value>,
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
        validate_property_term(owner, name, term, errors);
    }
}

fn validate_property_term(
    owner: &str,
    path: &str,
    term: &Value,
    errors: &mut Vec<ValidationError>,
) {
    match term {
        Value::String(name) if matches!(name.as_str(), "string" | "number" | "boolean") => {}
        Value::Object(fields) if !fields.is_empty() => {
            for (name, nested) in fields {
                validate_property_term(owner, &format!("{path}.{name}"), nested, errors);
            }
        }
        Value::Array(items) if items.len() == 1 => {
            validate_property_term(owner, &format!("{path}[]"), &items[0], errors);
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
    let matches = match term {
        Value::String(kind) => match kind.as_str() {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            _ => false,
        },
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
        errors.push(ValidationError::new(
            "property.shape_mismatch",
            file,
            Some(line),
            format!("{owner} property `{path}` does not match its declared JSON shape"),
        ));
    }
}

fn uses_reserved_namespace(id: &str) -> bool {
    id.starts_with("meta:") || id.starts_with("bbox:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphAuthority, GraphScope};
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
                },
            )]),
            edge_types: Vec::new(),
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
}
