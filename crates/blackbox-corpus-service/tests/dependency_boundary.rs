use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

#[test]
// This compile-boundary test synchronously invokes Cargo metadata as an
// isolated test process, outside any production Tokio request path.
#[allow(clippy::disallowed_methods)]
fn service_tree_excludes_execution_and_operational_implementations() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&manifest)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["id"].as_str().unwrap().to_string(),
                package["name"].as_str().unwrap().to_string(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let root = packages
        .iter()
        .find_map(|(id, name)| (name == "blackbox-corpus-service").then(|| id.clone()))
        .expect("corpus service package missing from metadata");
    let mut edges = BTreeMap::<String, Vec<String>>::new();
    for node in metadata["resolve"]["nodes"].as_array().unwrap() {
        let id = node["id"].as_str().unwrap().to_string();
        let dependencies = node["deps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|dependency| {
                dependency["dep_kinds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|kind| kind["kind"].is_null() || kind["kind"] == "build")
            })
            .map(|dependency| dependency["pkg"].as_str().unwrap().to_string())
            .collect();
        edges.insert(id, dependencies);
    }

    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::from([root]);
    while let Some(id) = queue.pop_front() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        queue.extend(edges.get(&id).into_iter().flatten().cloned());
    }
    let names = reachable
        .iter()
        .filter_map(|id| packages.get(id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let forbidden_exact = [
        "blackbox",
        "blackops-core",
        "blackopsd",
        "fleet-core",
        "fleetd",
        "bro-harness",
        "bro-tools",
        "bro-code-mode",
        "v8",
        "deno_core",
        "async-openai",
    ];
    let present = forbidden_exact
        .into_iter()
        .filter(|name| names.contains(*name))
        .collect::<Vec<_>>();
    assert!(
        present.is_empty(),
        "forbidden implementation dependencies reached corpus service: {present:?}"
    );
    assert!(names.contains("bbox-corpus-index"));
    assert!(names.contains("bro-capabilities"));
    assert!(names.contains("bro-protocol"));
}
