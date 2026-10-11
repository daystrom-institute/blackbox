#[test]
fn daemon_manifest_does_not_link_harness_or_capability_implementation_crates() {
    let manifest: toml::Value =
        toml::from_str(include_str!("../Cargo.toml")).expect("parse workspace manifest");
    let dependencies = manifest["dependencies"]
        .as_table()
        .expect("root dependencies table");

    assert!(
        !dependencies.contains_key("bro-harness"),
        "the custom harness must not be linked"
    );
    assert!(
        !dependencies.contains_key("bro-capabilities"),
        "daemon capability projection crosses MCP, not an in-process trait slot"
    );
}

#[test]
fn daemon_lock_graph_excludes_harness_code_mode_and_v8() {
    use std::collections::{BTreeMap, BTreeSet};

    let lock: toml::Value =
        toml::from_str(include_str!("../Cargo.lock")).expect("parse workspace lockfile");
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for package in lock["package"].as_array().expect("lockfile packages") {
        let name = package["name"].as_str().expect("package name").to_string();
        let dependencies = package
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .filter_map(|dependency| dependency.split_whitespace().next())
            .map(str::to_string);
        graph.entry(name).or_default().extend(dependencies);
    }

    let mut reachable = BTreeSet::new();
    let mut pending = vec!["blackbox".to_string()];
    while let Some(package) = pending.pop() {
        if !reachable.insert(package.clone()) {
            continue;
        }
        pending.extend(graph.get(&package).into_iter().flatten().cloned());
    }

    for forbidden in ["bro-harness", "bro-capabilities", "bro-code-mode", "v8"] {
        assert!(
            !reachable.contains(forbidden),
            "{forbidden} must stay outside blackboxd's transitive dependency graph"
        );
    }
}

#[test]
fn workspace_has_no_custom_harness_refactor_or_v8_packages() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    for forbidden in [
        "bro-harness",
        "bro-tools",
        "bro-capabilities",
        "bro-code-mode",
        "bro-apply-patch",
        "bro-lsp",
        "bbox-lsp",
        "bbox-refactor",
        "v8",
        "deno_core",
    ] {
        assert!(
            !lock["package"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"].as_str() == Some(forbidden)),
            "retired package remains: {forbidden}"
        );
        assert!(
            !manifest["workspace"]["members"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m.as_str() == Some(&format!("crates/{forbidden}"))),
            "retired member remains: {forbidden}"
        );
    }
}
