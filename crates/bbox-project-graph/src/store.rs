use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    GraphDescriptor, GraphGeneration, GraphKey, GraphSchema, GraphSource, ProjectGraphEdge,
    ProjectGraphVertex, ValidationError, build_generation, validate_graph, validate_graph_id,
};

const GRAPH_FILE: &str = "graph.json";
const SCHEMA_FILE: &str = "schema.json";
const VERTICES_FILE: &str = "vertices.jsonl";
const EDGES_FILE: &str = "edges.jsonl";
const REQUIRED_SOURCE_FILES: [&str; 3] = [SCHEMA_FILE, VERTICES_FILE, EDGES_FILE];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphParseLimits {
    pub max_vertices: usize,
    pub max_edges: usize,
}

impl Default for GraphParseLimits {
    fn default() -> Self {
        Self {
            max_vertices: usize::MAX,
            max_edges: usize::MAX,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct GraphDocumentBytes<'a> {
    pub descriptor: Option<&'a [u8]>,
    pub schema: &'a [u8],
    pub vertices: &'a [u8],
    pub edges: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedGraphDocuments {
    descriptor: Option<Vec<u8>>,
    schema: Vec<u8>,
    vertices: Vec<u8>,
    edges: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GraphLocation {
    pub graph_id: String,
    pub source: GraphSource,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub scope_id: String,
    pub graph_id: String,
    pub source: GraphSource,
    pub valid: bool,
    pub errors: Vec<ValidationError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<GraphDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub fact_vertex_count: usize,
    pub fact_edge_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Debug)]
pub struct GraphLoad {
    pub report: ValidationReport,
    pub generation: Option<GraphGeneration>,
}

pub fn discover_graphs(project_root: &Path, include_local: bool) -> Vec<GraphLocation> {
    let mut locations = discover_under(project_root, GraphSource::Committed);
    if include_local {
        locations.extend(discover_under(project_root, GraphSource::LocalScratch));
    }
    locations.sort_by(|a, b| a.graph_id.cmp(&b.graph_id).then(a.source.cmp(&b.source)));
    locations
}

pub fn locate_graph(
    project_root: &Path,
    graph_id: &str,
    include_local: bool,
) -> Result<GraphLocation, ValidationError> {
    if let Err(message) = validate_graph_id(graph_id) {
        return Err(ValidationError::new(
            "descriptor.invalid_graph_id",
            GRAPH_FILE,
            None,
            message,
        ));
    }
    let committed = graph_directory(project_root, GraphSource::Committed, graph_id)
        .filter(|directory| directory.is_dir());
    let local = graph_directory(project_root, GraphSource::LocalScratch, graph_id)
        .filter(|directory| directory.is_dir());
    if committed.is_some() && include_local && local.is_some() {
        return Err(ValidationError::new(
            "graph.ambiguous_source",
            GRAPH_FILE,
            None,
            format!("graph `{graph_id}` exists in both .bbox/graphs and .bbox/local/graphs"),
        ));
    }
    if let Some(directory) = committed {
        return Ok(GraphLocation {
            graph_id: graph_id.to_string(),
            source: GraphSource::Committed,
            directory,
        });
    }
    if include_local && let Some(directory) = local.clone() {
        return Ok(GraphLocation {
            graph_id: graph_id.to_string(),
            source: GraphSource::LocalScratch,
            directory,
        });
    }
    Err(ValidationError::new(
        "graph.not_found",
        GRAPH_FILE,
        None,
        if local.is_some() {
            format!(
                "graph `{graph_id}` exists only under .bbox/local/graphs; pass include_local=true to opt in"
            )
        } else {
            format!("graph `{graph_id}` was not found")
        },
    ))
}

pub fn load_graph(scope_id: &str, project_root: &Path, location: &GraphLocation) -> GraphLoad {
    let mut errors = Vec::new();
    if let Err(message) = validate_graph_id(&location.graph_id) {
        errors.push(ValidationError::new(
            "descriptor.invalid_graph_id",
            GRAPH_FILE,
            None,
            message,
        ));
    }
    let documents = match read_stable_documents(&location.directory) {
        Ok(documents) => documents,
        Err(mut read_errors) => {
            errors.append(&mut read_errors);
            return GraphLoad {
                report: report(scope_id, location, errors, None, None, 0, 0, None),
                generation: None,
            };
        }
    };

    let schema = parse_json::<GraphSchema>(&documents.schema, SCHEMA_FILE, &mut errors);
    let descriptor = descriptor_from_documents(
        &location.graph_id,
        location.source,
        documents.descriptor.as_deref(),
        schema.as_ref(),
        &mut errors,
    );
    let vertices = parse_jsonl::<ProjectGraphVertex>(
        &documents.vertices,
        VERTICES_FILE,
        usize::MAX,
        &mut errors,
    );
    let edges =
        parse_jsonl::<ProjectGraphEdge>(&documents.edges, EDGES_FILE, usize::MAX, &mut errors);
    if let (Some(descriptor), Some(schema)) = (&descriptor, &schema) {
        errors.extend(validate_graph(
            &location.graph_id,
            location.source,
            descriptor,
            schema,
            &vertices,
            &edges,
        ));
    }
    let fingerprint = fingerprint_documents(&documents);
    let report = report(
        scope_id,
        location,
        errors,
        descriptor.clone(),
        schema.as_ref().map(|schema| schema.namespace.clone()),
        vertices.len(),
        edges.len(),
        Some(fingerprint.clone()),
    );
    let generation = if report.valid {
        Some(build_generation(
            GraphKey {
                scope_id: scope_id.to_string(),
                graph_id: location.graph_id.clone(),
                source: location.source,
            },
            descriptor.expect("valid report requires descriptor"),
            schema.expect("valid report requires schema"),
            vertices.into_iter().map(|(_, vertex)| vertex).collect(),
            edges.into_iter().map(|(_, edge)| edge).collect(),
            fingerprint,
            project_root.to_path_buf(),
        ))
    } else {
        None
    };
    GraphLoad { report, generation }
}

pub fn load_graph_documents(
    scope_id: &str,
    graph_id: &str,
    documents: GraphDocumentBytes<'_>,
    limits: GraphParseLimits,
    source_root: PathBuf,
) -> GraphLoad {
    let location = GraphLocation {
        graph_id: graph_id.to_string(),
        source: GraphSource::Committed,
        directory: source_root.clone(),
    };
    let mut errors = Vec::new();
    if let Err(message) = validate_graph_id(graph_id) {
        errors.push(ValidationError::new(
            "descriptor.invalid_graph_id",
            GRAPH_FILE,
            None,
            message,
        ));
    }
    let schema = parse_json::<GraphSchema>(documents.schema, SCHEMA_FILE, &mut errors);
    let vertices = parse_jsonl::<ProjectGraphVertex>(
        documents.vertices,
        VERTICES_FILE,
        limits.max_vertices,
        &mut errors,
    );
    let edges =
        parse_jsonl::<ProjectGraphEdge>(documents.edges, EDGES_FILE, limits.max_edges, &mut errors);
    let descriptor = descriptor_from_documents(
        graph_id,
        GraphSource::Committed,
        documents.descriptor,
        schema.as_ref(),
        &mut errors,
    );
    if let (Some(descriptor), Some(schema)) = (&descriptor, &schema) {
        errors.extend(validate_graph(
            graph_id,
            GraphSource::Committed,
            descriptor,
            schema,
            &vertices,
            &edges,
        ));
    }
    let fingerprint = fingerprint_transport_documents(documents);
    let report = report(
        scope_id,
        &location,
        errors,
        descriptor.clone(),
        schema.as_ref().map(|schema| schema.namespace.clone()),
        vertices.len(),
        edges.len(),
        Some(fingerprint.clone()),
    );
    let generation = if report.valid {
        Some(build_generation(
            GraphKey {
                scope_id: scope_id.to_string(),
                graph_id: graph_id.to_string(),
                source: GraphSource::Committed,
            },
            descriptor.expect("valid report requires descriptor"),
            schema.expect("valid report requires schema"),
            vertices.into_iter().map(|(_, vertex)| vertex).collect(),
            edges.into_iter().map(|(_, edge)| edge).collect(),
            fingerprint,
            source_root,
        ))
    } else {
        None
    };
    GraphLoad { report, generation }
}

fn discover_under(project_root: &Path, source: GraphSource) -> Vec<GraphLocation> {
    // A source with no checkout-relative root (connector-managed) is never
    // discoverable on a checkout, however the caller asks.
    let Some(relative_root) = source.relative_root() else {
        return Vec::new();
    };
    let root = project_root.join(relative_root);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let graph_id = entry.file_name().to_string_lossy().into_owned();
            if graph_id.starts_with("._") || !entry.file_type().ok()?.is_dir() {
                return None;
            }
            Some(GraphLocation {
                graph_id,
                source,
                directory: entry.path(),
            })
        })
        .collect()
}

/// The checkout directory that would hold this graph, or `None` when the
/// source is not file-backed by a checkout (connector-managed).
fn graph_directory(project_root: &Path, source: GraphSource, graph_id: &str) -> Option<PathBuf> {
    Some(project_root.join(source.relative_root()?).join(graph_id))
}

fn read_stable_documents(directory: &Path) -> Result<OwnedGraphDocuments, Vec<ValidationError>> {
    let first = read_documents(directory)?;
    let second = read_documents(directory)?;
    if first != second {
        return Err(vec![ValidationError::new(
            "generation.concurrent_update",
            GRAPH_FILE,
            None,
            "graph documents changed while the generation was being read; retry after the file update completes",
        )]);
    }
    Ok(first)
}

fn read_documents(directory: &Path) -> Result<OwnedGraphDocuments, Vec<ValidationError>> {
    let mut errors = Vec::new();
    let descriptor = match fs::read(directory.join(GRAPH_FILE)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            errors.push(ValidationError::new(
                "file.read_failed",
                GRAPH_FILE,
                None,
                format!("failed to read {GRAPH_FILE}: {error}"),
            ));
            None
        }
    };
    let mut required = Vec::with_capacity(REQUIRED_SOURCE_FILES.len());
    for file in REQUIRED_SOURCE_FILES {
        let path = directory.join(file);
        match fs::read(&path) {
            Ok(bytes) => required.push(bytes),
            Err(error) => errors.push(ValidationError::new(
                "file.read_failed",
                file,
                None,
                format!("failed to read {file}: {error}"),
            )),
        }
    }
    if errors.is_empty() {
        let [schema, vertices, edges]: [Vec<u8>; 3] = required
            .try_into()
            .expect("all required graph documents were read");
        Ok(OwnedGraphDocuments {
            descriptor,
            schema,
            vertices,
            edges,
        })
    } else {
        Err(errors)
    }
}

fn descriptor_from_documents(
    graph_id: &str,
    source: GraphSource,
    descriptor_bytes: Option<&[u8]>,
    schema: Option<&GraphSchema>,
    errors: &mut Vec<ValidationError>,
) -> Option<GraphDescriptor> {
    if let Some(bytes) = descriptor_bytes {
        return parse_json::<GraphDescriptor>(bytes, GRAPH_FILE, errors);
    }
    schema.map(|schema| GraphDescriptor {
        descriptor_version: crate::DESCRIPTOR_VERSION,
        scope: crate::GraphScope::Project,
        graph_id: graph_id.to_string(),
        authority: crate::GraphAuthority::Project,
        schema_id: format!("{}:schema", schema.namespace),
        schema_version: schema.version,
        projection_version: None,
        source_connector: None,
        retention_policy: match source {
            GraphSource::Committed => crate::RetentionPolicy::ProjectOwned,
            GraphSource::LocalScratch => crate::RetentionPolicy::LocalScratch,
        },
        generation: 1,
    })
}

fn parse_json<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    file: &str,
    errors: &mut Vec<ValidationError>,
) -> Option<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(ValidationError::new(
                "json.malformed",
                file,
                Some(error.line()),
                error.to_string(),
            ));
            None
        }
    }
}

fn parse_jsonl<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    file: &str,
    max_rows: usize,
    errors: &mut Vec<ValidationError>,
) -> Vec<(usize, T)> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            errors.push(ValidationError::new(
                "json.invalid_utf8",
                file,
                None,
                error.to_string(),
            ));
            return Vec::new();
        }
    };
    let mut rows = Vec::new();
    let mut decoded_rows = 0_usize;
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        decoded_rows = decoded_rows.saturating_add(1);
        if decoded_rows > max_rows {
            errors.push(ValidationError::new(
                "jsonl.row_limit_exceeded",
                file,
                Some(line_number),
                format!("{file} exceeds the decoded row maximum of {max_rows}"),
            ));
            break;
        }
        match serde_json::from_str(line) {
            Ok(value) => rows.push((line_number, value)),
            Err(error) => errors.push(ValidationError::new(
                "jsonl.malformed",
                file,
                Some(line_number),
                error.to_string(),
            )),
        }
    }
    rows
}

fn fingerprint_documents(documents: &OwnedGraphDocuments) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in documents
        .descriptor
        .as_deref()
        .map(|bytes| (GRAPH_FILE, bytes))
        .into_iter()
        .chain([
            (SCHEMA_FILE, documents.schema.as_slice()),
            (VERTICES_FILE, documents.vertices.as_slice()),
            (EDGES_FILE, documents.edges.as_slice()),
        ])
    {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

fn fingerprint_transport_documents(documents: GraphDocumentBytes<'_>) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in documents
        .descriptor
        .map(|bytes| (GRAPH_FILE, bytes))
        .into_iter()
        .chain([
            (SCHEMA_FILE, documents.schema),
            (VERTICES_FILE, documents.vertices),
            (EDGES_FILE, documents.edges),
        ])
    {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

#[allow(clippy::too_many_arguments)]
fn report(
    scope_id: &str,
    location: &GraphLocation,
    errors: Vec<ValidationError>,
    descriptor: Option<GraphDescriptor>,
    namespace: Option<String>,
    fact_vertex_count: usize,
    fact_edge_count: usize,
    fingerprint: Option<String>,
) -> ValidationReport {
    ValidationReport {
        scope_id: scope_id.to_string(),
        graph_id: location.graph_id.clone(),
        source: location.source,
        valid: errors.is_empty(),
        errors,
        descriptor,
        namespace,
        fact_vertex_count,
        fact_edge_count,
        fingerprint,
    }
}

pub fn duplicate_graph_ids(locations: &[GraphLocation]) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for location in locations {
        if !seen.insert(location.graph_id.clone()) {
            duplicates.insert(location.graph_id.clone());
        }
    }
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectGraphCatalog;
    use std::fs;

    fn write_graph(root: &Path, graph_id: &str, generation: u64, local: bool) {
        let source = if local {
            GraphSource::LocalScratch
        } else {
            GraphSource::Committed
        };
        let dir = graph_directory(root, source, graph_id);
        fs::create_dir_all(&dir).unwrap();
        let retention = if local {
            "local_scratch"
        } else {
            "project_owned"
        };
        fs::write(
            dir.join(GRAPH_FILE),
            format!(
                r#"{{"descriptor_version":1,"scope":"project","graph_id":"{graph_id}","authority":"project","schema_id":"repo-schema","schema_version":1,"retention_policy":"{retention}","generation":{generation}}}"#
            ),
        )
        .unwrap();
        fs::write(
            dir.join(SCHEMA_FILE),
            r#"{"version":1,"namespace":"repo","vertex_types":{"repo:Module":{"required":["path","source"],"properties":{"path":"string","source":{"file":"string","tags":["string"]}}},"repo:Invariant":{"required":["claim"],"properties":{"claim":"string"}}},"edge_types":[{"type":"repo:CONSTRAINED_BY","from_type":"repo:Module","to_type":"repo:Invariant","required":["confidence"],"properties":{"confidence":"number"}}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join(VERTICES_FILE),
            concat!(
                r#"{"id":"src/tools/graph.rs","type":"repo:Module","label":"graph tools","properties":{"path":"src/tools/graph.rs","source":{"file":"PROJECT.md","tags":["graph","tools"]}}}"#,
                "\n",
                r#"{"id":"canonical-refs","type":"repo:Invariant","label":"canonical refs","properties":{"claim":"refs round trip"}}"#,
                "\n"
            ),
        )
        .unwrap();
        fs::write(
            dir.join(EDGES_FILE),
            concat!(
                r#"{"from":"src/tools/graph.rs","type":"repo:CONSTRAINED_BY","to":"canonical-refs","properties":{"confidence":1}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn valid_graph_loads_and_materializes_fixed_floor() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        let location = locate_graph(&root, "repo", false).unwrap();
        let loaded = load_graph("scope123", &root, &location);
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        let generation = loaded.generation.unwrap();
        for id in crate::FIXED_META_VERTICES {
            assert!(generation.vertices.contains_key(id));
        }
        assert!(
            generation
                .edges
                .iter()
                .any(|edge| edge.type_name == "repo:CONSTRAINED_BY")
        );
        assert!(generation.edges.iter().any(|edge| {
            edge.from == "repo:CONSTRAINED_BY"
                && edge.type_name == crate::META_FROM_TYPE
                && edge.to == "repo:Module"
        }));
    }

    #[test]
    fn authored_counts_match_source_rows_and_stay_below_reflected_totals() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        let location = locate_graph(&root, "repo", false).unwrap();
        let loaded = load_graph("scope123", &root, &location);
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);

        // write_graph() authors exactly 2 vertices (repo:Module,
        // repo:Invariant) and 1 edge (repo:CONSTRAINED_BY) in
        // vertices.jsonl / edges.jsonl.
        assert_eq!(loaded.report.fact_vertex_count, 2);
        assert_eq!(loaded.report.fact_edge_count, 1);

        let generation = loaded.generation.unwrap();
        assert_eq!(generation.authored_vertex_count, 2);
        assert_eq!(generation.authored_edge_count, 1);

        // The reflected graph also carries the fixed meta floor plus
        // schema-as-data vertex/edge type definitions and meta:INSTANCE_OF
        // edges, so the totals on GraphGeneration must exceed the authored
        // counts.
        assert!(generation.vertices.len() > generation.authored_vertex_count);
        assert!(generation.edges.len() > generation.authored_edge_count);
    }

    #[test]
    fn graph_without_optional_descriptor_loads_from_the_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        fs::remove_file(graph_directory(&root, GraphSource::Committed, "repo").join(GRAPH_FILE))
            .unwrap();

        let location = locate_graph(&root, "repo", false).unwrap();
        let loaded = load_graph("scope123", &root, &location);

        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        let descriptor = loaded.report.descriptor.unwrap();
        assert_eq!(descriptor.graph_id, "repo");
        assert_eq!(descriptor.schema_id, "repo:schema");
        assert_eq!(descriptor.generation, 1);
    }

    /// Connector-managed graphs have no checkout-relative root, so no amount
    /// of asking discovers or locates one on a checkout. This is the storage
    /// half of "connector graphs are not writable through a checkout lane".
    #[test]
    fn connector_managed_graphs_are_not_discoverable_on_a_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        assert!(GraphSource::ConnectorManaged.relative_root().is_none());
        assert!(!GraphSource::ConnectorManaged.is_checkout_authored());
        assert!(graph_directory(&root, GraphSource::ConnectorManaged, "repo").is_none());
        assert!(discover_under(&root, GraphSource::ConnectorManaged).is_empty());
        assert!(
            discover_graphs(&root, true)
                .iter()
                .all(|location| location.source != GraphSource::ConnectorManaged)
        );
    }

    /// One graph id in one scope holds exactly one authority. A connector
    /// generation cannot shadow or replace a project-authored graph, and the
    /// refusal is symmetric.
    #[test]
    fn a_connector_generation_cannot_shadow_a_project_authored_graph() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        let location = locate_graph(&root, "repo", false).unwrap();
        let project = load_graph("scope123", &root, &location).generation.unwrap();

        let schema: GraphSchema = serde_json::from_str(
            r#"{"version":1,"namespace":"dataset","vertex_types":{"dataset:Asset":{"required":["remote_id"],"properties":{"remote_id":"string"}}},"edge_types":[]}"#,
        )
        .unwrap();
        let connector = crate::build_generation(
            GraphKey {
                scope_id: "scope123".into(),
                graph_id: "repo".into(),
                source: GraphSource::ConnectorManaged,
            },
            GraphDescriptor {
                descriptor_version: crate::DESCRIPTOR_VERSION,
                scope: crate::GraphScope::Project,
                graph_id: "repo".into(),
                authority: crate::GraphAuthority::Connector,
                schema_id: "dataset:schema".into(),
                schema_version: 1,
                projection_version: Some("dataset-v1".into()),
                source_connector: Some("synthetic-api".into()),
                retention_policy: crate::RetentionPolicy::ConnectorManaged,
                generation: 9,
            },
            schema,
            Vec::new(),
            Vec::new(),
            "0".repeat(64),
            root.clone(),
        );

        let mut catalog = ProjectGraphCatalog::default();
        catalog.publish(project).unwrap();
        let error = catalog.publish(connector).unwrap_err();
        assert_eq!(error.code, "graph.ambiguous_source");
        assert!(error.message.contains("project"), "{}", error.message);
        assert_eq!(
            catalog
                .get("scope123", "repo", false)
                .unwrap()
                .descriptor
                .authority,
            crate::GraphAuthority::Project,
            "the project-authored generation stays accepted"
        );
    }

    #[test]
    fn local_graphs_are_excluded_until_explicitly_included() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "scratch", 1, true);
        assert!(discover_graphs(&root, false).is_empty());
        assert_eq!(discover_graphs(&root, true).len(), 1);
        let error = locate_graph(&root, "scratch", false).unwrap_err();
        assert_eq!(error.code, "graph.not_found");
        assert!(error.message.contains("include_local=true"));
    }

    #[test]
    fn accepted_generation_is_atomic_and_monotonic() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        write_graph(&root, "repo", 1, false);
        let location = locate_graph(&root, "repo", false).unwrap();
        let mut catalog = ProjectGraphCatalog::default();
        let first = load_graph("scope123", &root, &location).generation.unwrap();
        let accepted = catalog.publish(first).unwrap();
        let accepted_fingerprint = accepted.fingerprint.clone();

        fs::write(
            location.directory.join(EDGES_FILE),
            concat!(
                r#"{"from":"src/tools/graph.rs","type":"repo:CONSTRAINED_BY","to":"canonical-refs","properties":{"confidence":0.5}}"#,
                "\n"
            ),
        )
        .unwrap();
        let divergent = load_graph("scope123", &root, &location).generation.unwrap();
        let error = catalog.publish(divergent).unwrap_err();
        assert_eq!(error.code, "generation.conflict");
        assert_eq!(
            catalog.get("scope123", "repo", false).unwrap().fingerprint,
            accepted_fingerprint
        );

        write_graph(&root, "repo", 2, false);
        let second = load_graph("scope123", &root, &location).generation.unwrap();
        catalog.publish(second).unwrap();
        assert_eq!(
            catalog
                .get("scope123", "repo", false)
                .unwrap()
                .descriptor
                .generation,
            2
        );

        fs::remove_dir_all(&location.directory).unwrap();
        catalog.reconcile_source("scope123", GraphSource::Committed, &BTreeSet::new());
        assert!(catalog.get("scope123", "repo", false).is_none());
    }
}
