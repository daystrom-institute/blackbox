use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use bbox_project_graph::{GraphGeneration, META_FROM_TYPE, META_TO_TYPE, load_graph, locate_graph};
use serde_json::Value;

const GRAPH_ID: &str = "governance-record";
const FIXTURE_FILES: [&str; 4] = ["graph.json", "schema.json", "vertices.jsonl", "edges.jsonl"];

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(GRAPH_ID)
}

fn copy_fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let target = root.join(".bbox").join("graphs").join(GRAPH_ID);
    fs::create_dir_all(&target).unwrap();
    for file in FIXTURE_FILES {
        fs::copy(fixture_root().join(file), target.join(file)).unwrap();
    }
    (temp, root)
}

fn load_fixture(root: &Path) -> bbox_project_graph::GraphLoad {
    let location = locate_graph(root, GRAPH_ID, false).unwrap();
    load_graph("fixture-scope", root, &location)
}

fn outgoing(generation: &GraphGeneration, from: &str, edge_type: &str) -> BTreeSet<String> {
    generation
        .edges
        .iter()
        .filter(|edge| edge.from == from && edge.type_name == edge_type)
        .map(|edge| edge.to.clone())
        .collect()
}

#[test]
fn governance_fixture_loads_projects_all_endpoints_and_traverses() {
    let (_temp, root) = copy_fixture();
    let loaded = load_fixture(&root);
    assert!(loaded.report.valid, "{:?}", loaded.report.errors);
    let generation = loaded.generation.unwrap();

    assert_eq!(
        generation
            .edges
            .iter()
            .filter(|edge| edge.from == "gov:CITES" && edge.type_name == META_FROM_TYPE)
            .count(),
        3
    );
    assert_eq!(
        generation
            .edges
            .iter()
            .filter(|edge| edge.from == "gov:CITES" && edge.type_name == META_TO_TYPE)
            .count(),
        3
    );
    let cites_from = outgoing(&generation, "gov:CITES", META_FROM_TYPE);
    assert_eq!(cites_from, BTreeSet::from(["gov:Claim".to_string()]));
    let cites_to = outgoing(&generation, "gov:CITES", META_TO_TYPE);
    assert_eq!(
        cites_to,
        BTreeSet::from([
            "gov:Decision".to_string(),
            "gov:Evidence".to_string(),
            "gov:Review".to_string(),
        ])
    );

    assert_eq!(
        outgoing(&generation, "claim/scope@2", "gov:CITES"),
        BTreeSet::from([
            "decision/accept@1".to_string(),
            "evidence/document@1".to_string(),
            "review/scope@1".to_string(),
        ])
    );
    assert_eq!(
        outgoing(&generation, "decision/accept@1", "gov:RESOLVES"),
        BTreeSet::from([
            "subject/scope@1".to_string(),
            "tension/coverage@1".to_string(),
        ])
    );
}

#[test]
fn edge_endpoint_mismatch_enumerates_declared_pairs() {
    let (_temp, root) = copy_fixture();
    let edges = root
        .join(".bbox")
        .join("graphs")
        .join(GRAPH_ID)
        .join("edges.jsonl");
    let mut contents = fs::read_to_string(&edges).unwrap();
    contents.push_str(
        "{\"from\":\"claim/scope@2\",\"type\":\"gov:CITES\",\"to\":\"tension/coverage@1\",\"properties\":{\"version_pin\":\"tension/coverage@1\"}}\n",
    );
    fs::write(edges, contents).unwrap();

    let loaded = load_fixture(&root);
    let error = loaded
        .report
        .errors
        .iter()
        .find(|error| error.code == "edge.endpoint_type_mismatch")
        .unwrap();
    assert!(error.message.contains("edge type `gov:CITES`"));
    for pair in [
        "(`gov:Claim`, `gov:Evidence`)",
        "(`gov:Claim`, `gov:Review`)",
        "(`gov:Claim`, `gov:Decision`)",
    ] {
        assert!(error.message.contains(pair), "{}", error.message);
    }
}

#[test]
fn enum_value_outside_declared_members_is_rejected() {
    let (_temp, root) = copy_fixture();
    let vertices = root
        .join(".bbox")
        .join("graphs")
        .join(GRAPH_ID)
        .join("vertices.jsonl");
    let contents = fs::read_to_string(&vertices).unwrap().replacen(
        "\"status\":\"active\"",
        "\"status\":\"paused\"",
        1,
    );
    fs::write(vertices, contents).unwrap();

    let loaded = load_fixture(&root);
    let error = loaded
        .report
        .errors
        .iter()
        .find(|error| error.code == "property.enum_violation")
        .unwrap();
    assert!(error.message.contains("`paused`"));
    assert!(error.message.contains("`draft`, `active`, `retired`"));
}

#[test]
fn duplicate_declared_endpoint_pair_is_rejected() {
    let (_temp, root) = copy_fixture();
    let schema_path = root
        .join(".bbox")
        .join("graphs")
        .join(GRAPH_ID)
        .join("schema.json");
    let mut schema: Value = serde_json::from_slice(&fs::read(&schema_path).unwrap()).unwrap();
    let cites = schema["edge_types"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|definition| definition["type"] == "gov:CITES")
        .unwrap();
    let duplicate = cites["endpoints"][0].clone();
    cites["endpoints"].as_array_mut().unwrap().push(duplicate);
    fs::write(&schema_path, serde_json::to_vec_pretty(&schema).unwrap()).unwrap();

    let loaded = load_fixture(&root);
    assert!(
        loaded
            .report
            .errors
            .iter()
            .any(|error| error.code == "schema.duplicate_endpoint_pair")
    );
}
