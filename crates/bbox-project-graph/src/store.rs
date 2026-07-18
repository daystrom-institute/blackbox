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
const SOURCE_FILES: [&str; 4] = [GRAPH_FILE, SCHEMA_FILE, VERTICES_FILE, EDGES_FILE];

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
    let committed = graph_directory(project_root, GraphSource::Committed, graph_id);
    let local = graph_directory(project_root, GraphSource::LocalScratch, graph_id);
    if committed.is_dir() && include_local && local.is_dir() {
        return Err(ValidationError::new(
            "graph.ambiguous_source",
            GRAPH_FILE,
            None,
            format!("graph `{graph_id}` exists in both .bbox/graphs and .bbox/local/graphs"),
        ));
    }
    if committed.is_dir() {
        return Ok(GraphLocation {
            graph_id: graph_id.to_string(),
            source: GraphSource::Committed,
            directory: committed,
        });
    }
    if include_local && local.is_dir() {
        return Ok(GraphLocation {
            graph_id: graph_id.to_string(),
            source: GraphSource::LocalScratch,
            directory: local,
        });
    }
    Err(ValidationError::new(
        "graph.not_found",
        GRAPH_FILE,
        None,
        if local.is_dir() {
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

    let descriptor = parse_json::<GraphDescriptor>(&documents[0], GRAPH_FILE, &mut errors);
    let schema = parse_json::<GraphSchema>(&documents[1], SCHEMA_FILE, &mut errors);
    let vertices = parse_jsonl::<ProjectGraphVertex>(&documents[2], VERTICES_FILE, &mut errors);
    let edges = parse_jsonl::<ProjectGraphEdge>(&documents[3], EDGES_FILE, &mut errors);
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

fn discover_under(project_root: &Path, source: GraphSource) -> Vec<GraphLocation> {
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

fn graph_directory(project_root: &Path, source: GraphSource, graph_id: &str) -> PathBuf {
    project_root
        .join(
            source
                .relative_root()
                .expect("file-backed graph source must have a relative root"),
        )
        .join(graph_id)
}

fn read_stable_documents(directory: &Path) -> Result<Vec<Vec<u8>>, Vec<ValidationError>> {
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

fn read_documents(directory: &Path) -> Result<Vec<Vec<u8>>, Vec<ValidationError>> {
    let mut documents = Vec::with_capacity(SOURCE_FILES.len());
    let mut errors = Vec::new();
    for file in SOURCE_FILES {
        let path = directory.join(file);
        match fs::read(&path) {
            Ok(bytes) => documents.push(bytes),
            Err(error) => errors.push(ValidationError::new(
                "file.read_failed",
                file,
                None,
                format!("failed to read {file}: {error}"),
            )),
        }
    }
    if errors.is_empty() {
        Ok(documents)
    } else {
        Err(errors)
    }
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
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line_number = index + 1;
            if line.trim().is_empty() {
                return None;
            }
            match serde_json::from_str(line) {
                Ok(value) => Some((line_number, value)),
                Err(error) => {
                    errors.push(ValidationError::new(
                        "jsonl.malformed",
                        file,
                        Some(line_number),
                        error.to_string(),
                    ));
                    None
                }
            }
        })
        .collect()
}

fn fingerprint_documents(documents: &[Vec<u8>]) -> String {
    let mut hasher = Sha256::new();
    for (name, bytes) in SOURCE_FILES.iter().zip(documents) {
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
    fn valid_graph_loads_and_projects_fixed_floor() {
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
                .projected_edges
                .iter()
                .any(|edge| edge.kind == "repo:CONSTRAINED_BY")
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
