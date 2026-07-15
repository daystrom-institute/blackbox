#[test]
fn fleetd_manifest_has_no_forbidden_implementation_edges() {
    let manifest = include_str!("../Cargo.toml");
    let dependencies = manifest
        .split_once("[dependencies]")
        .expect("fleetd manifest must declare dependencies")
        .1
        .split("\n[")
        .next()
        .expect("fleetd dependency section must be bounded");
    for forbidden in ["bro-tools", "bro-harness", "blackops-core", "corpusd"] {
        assert!(
            !dependencies.lines().any(|line| {
                line.split_once('=')
                    .is_some_and(|(name, _)| name.trim() == forbidden)
            }),
            "fleetd must not depend on forbidden implementation crate {forbidden}"
        );
    }
}
