//! Compiler-graph backstop for the runtime contract bottom.
//!
//! The final service binaries arrive in later increments. This probe makes the
//! already-established bottom and client boundaries fail immediately if an
//! implementation dependency points upward while those increments proceed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn local_dependencies(manifest: &Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(manifest).unwrap();
    let value: toml::Value = toml::from_str(&text).unwrap();
    let mut names = BTreeSet::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = value.get(section).and_then(toml::Value::as_table) else {
            continue;
        };
        for (name, dependency) in table {
            let fields = dependency.as_table();
            let is_local = fields.and_then(|fields| fields.get("path")).is_some();
            if is_local {
                let package_name = fields
                    .and_then(|fields| fields.get("package"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or(name);
                names.insert(package_name.to_string());
            }
        }
    }
    names
}

fn manifest(root: &Path, crate_name: &str) -> PathBuf {
    root.join("crates").join(crate_name).join("Cargo.toml")
}

fn assert_local_deps(root: &Path, crate_name: &str, allowed: &[&str]) {
    let actual = local_dependencies(&manifest(root, crate_name));
    let allowed = allowed
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual, allowed,
        "{crate_name} gained a forbidden local dependency"
    );
}

fn assert_local_deps_within(root: &Path, crate_name: &str, allowed: &[&str]) {
    let actual = local_dependencies(&manifest(root, crate_name));
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    let forbidden = actual
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        forbidden.is_empty(),
        "{crate_name} gained forbidden local dependencies: {forbidden:?}"
    );
}

#[test]
fn contract_bottom_and_thin_clients_have_no_upward_edges() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_local_deps(root, "bro-core", &[]);
    assert_local_deps(root, "bro-protocol", &["bro-core"]);
    assert_local_deps(root, "bro-capabilities", &["bro-core"]);
    assert_local_deps_within(
        root,
        "bro-rpc",
        &["bro-capabilities", "bro-core", "bro-protocol"],
    );
    assert_local_deps(root, "bro-fleet-client", &["bro-core", "bro-protocol"]);
}

#[test]
fn harness_never_links_daemon_or_service_implementations() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dependencies = local_dependencies(&manifest(root, "bro-harness"));
    for forbidden in [
        "blackbox",
        "fleet-core",
        "blackops-core",
        "bbox-stores",
        "bbox-indexing",
    ] {
        assert!(
            !dependencies.contains(forbidden),
            "bro-harness must not depend on {forbidden}"
        );
    }
}

#[test]
fn renamed_path_dependency_is_checked_by_package_identity() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("Cargo.toml");
    std::fs::write(
        &manifest,
        r#"
[package]
name = "probe"
version = "0.0.0"

[dependencies]
innocent-alias = { package = "forbidden-implementation", path = "../forbidden" }
"#,
    )
    .unwrap();
    assert_eq!(
        local_dependencies(&manifest),
        BTreeSet::from(["forbidden-implementation".to_string()])
    );
}
