//! Tests for the `builtin.java.guice` macro.
//!
//! `builtin.java.guice` is the thin affirmative "extract a class + wire the new
//! delegate via Guice field injection" macro produced when the Guice
//! wiring-mode policy was dissolved out of `extract_java_class` (the engine no
//! longer carries any DI-library knowledge). The macro is a single
//! `DelegateRefactor` to `extract_java_class` supplying a framework-neutral
//! `WiringSpec` (`external_injection` + `@Inject` + `javax.inject.Inject`).
//!
//! Unlike the Lombok macro, this path uses only the delegate (pure-Rust
//! `extract_java_class`) and a `record` op — no JVM worker — so these tests run
//! unconditionally.

#![cfg(test)]

use std::path::Path;

use crate::MacroPlan;
use crate::model::MacroInvocation;
use crate::planner::MacroPlanner;
use crate::planner_ctx::MacroPlannerContext;
use crate::registry::MacroRegistry;

/// Plan `builtin.java.guice` for the given inputs and return the resulting
/// `MacroPlan`. No JVM worker is required (delegate is pure Rust).
fn plan_guice(
    project_dir: &Path,
    source: &Path,
    target: &Path,
    module_name: &str,
    delegate_field: &str,
    item_names: &[&str],
) -> MacroPlan {
    let def = MacroRegistry::get(None, "builtin.java.guice")
        .expect("registry get must not error")
        .expect("builtin.java.guice must be registered");

    let mut inputs = serde_json::Map::new();
    inputs.insert("source".into(), serde_json::json!(source.to_string_lossy()));
    inputs.insert("target".into(), serde_json::json!(target.to_string_lossy()));
    inputs.insert("module_name".into(), serde_json::json!(module_name));
    inputs.insert("delegate_field".into(), serde_json::json!(delegate_field));
    inputs.insert("item_names".into(), serde_json::json!(item_names));

    let inv = MacroInvocation {
        macro_id: "builtin.java.guice".into(),
        version: None,
        project_dir: project_dir.to_string_lossy().into_owned(),
        inputs,
        anchors: None,
        operator_opt_outs: vec![],
    };

    // Default context: UnavailableBackend is fine — the delegate path calls
    // refactor::plan_with_ctx (pure Rust), never the Java macro backend.
    let ctx = MacroPlannerContext::default();
    MacroPlanner::plan(&inv, &def, &ctx).expect("guice macro plan must succeed")
}

/// Concatenate every source-edit replacement so tests can assert on emitted
/// fragments without reimplementing byte-range application.
fn source_edit_replacements(plan: &MacroPlan, source: &Path) -> String {
    let src = source.to_string_lossy();
    plan.edits
        .file_edits
        .iter()
        .filter(|fe| src.ends_with(fe.path.as_str()) || fe.path == src)
        .flat_map(|fe| fe.edits.iter().map(|e| e.replacement.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The new target file is modeled by `extract_java_class` as a full-content
/// `FileEdit` (not a `FileCreate`); return its emitted content.
fn target_content(plan: &MacroPlan, target: &Path) -> String {
    let tgt = target.to_string_lossy();
    let fe = plan
        .edits
        .file_edits
        .iter()
        .find(|fe| tgt.ends_with(fe.path.as_str()) || fe.path == tgt)
        .expect("target file edit must be present");
    match &fe.new_text {
        Some(nt) => nt.clone(),
        None => fe
            .edits
            .iter()
            .map(|e| e.replacement.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

// The macro delegates with external_injection and emits the Guice-shaped
// wiring: `@Inject` + `<Target> <field>;` on the source (no ctor wiring), and
// an `@Inject` constructor + javax.inject.Inject import on the target.
#[test]
fn guice_macro_emits_field_injection_wiring() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    std::fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    std::fs::write(
        &source,
        "package a;\n\
             import javax.inject.Inject;\n\
             public class Admin {\n\
            \x20   @Inject private Helper helper;\n\
            \x20   public Long save() { return helper.compute(); }\n\
             }\n",
    )
    .unwrap();

    let plan = plan_guice(
        dir.path(),
        &source,
        &target,
        "Service",
        "service",
        &["save"],
    );
    assert!(plan.refusals.is_empty(), "no refusals: {:?}", plan.refusals);

    let src_edits = source_edit_replacements(&plan, &source);
    assert!(
        src_edits.contains("@Inject\n    private Service service;"),
        "source delegate field must be @Inject field-injected: {src_edits}"
    );
    assert!(
        !src_edits.contains("this.service = new Service"),
        "external injection must skip ctor wiring: {src_edits}"
    );

    let tgt = target_content(&plan, &target);
    assert!(
        tgt.contains("import javax.inject.Inject;"),
        "target must carry javax.inject.Inject import: {tgt}"
    );
    assert!(
        tgt.contains("@Inject\n") && tgt.contains("public Service("),
        "target ctor must be @Inject-annotated: {tgt}"
    );
}

// Anti-regression: the macro carries NO @Inject-detection guard. A source with
// no injected fields must still PLAN (the operator explicitly chose Guice
// wiring). Guards the de-scope decision so nobody later "restores parity" by
// inventing a false refusal.
#[test]
fn guice_macro_plans_even_when_source_has_no_inject_fields() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    std::fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Plain.java");
    let target = pkg.join("Service.java");
    std::fs::write(
        &source,
        "package a;\n\
             public class Plain {\n\
            \x20   public Long save() { return 1L; }\n\
             }\n",
    )
    .unwrap();

    let plan = plan_guice(
        dir.path(),
        &source,
        &target,
        "Service",
        "service",
        &["save"],
    );
    assert!(
        plan.refusals.is_empty(),
        "macro must NOT refuse a non-DI source — there is no @Inject guard: {:?}",
        plan.refusals
    );
    assert!(
        target_content(&plan, &target).contains("class Service"),
        "macro must still produce the extracted target"
    );
    let src_edits = source_edit_replacements(&plan, &source);
    assert!(
        src_edits.contains("@Inject\n    private Service service;"),
        "external injection is applied unconditionally (operator's choice): {src_edits}"
    );
}

// Typed-scalar forwarding through DelegateRefactor: deep_analysis is a bool.
// If interpolation stringified the whole-placeholder to "true", the delegate's
// RefactorPlanParams deserialize (Option<bool>) would fail and the plan would
// error. A successful plan proves the value is forwarded as a typed bool.
#[test]
fn guice_macro_forwards_deep_analysis_as_typed_bool() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    std::fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    std::fs::write(
        &source,
        "package a;\n\
             import javax.inject.Inject;\n\
             public class Admin {\n\
            \x20   @Inject private Helper helper;\n\
            \x20   public Long save() { return helper.compute(); }\n\
             }\n",
    )
    .unwrap();

    let def = MacroRegistry::get(None, "builtin.java.guice")
        .expect("registry get must not error")
        .expect("builtin.java.guice must be registered");

    let mut inputs = serde_json::Map::new();
    inputs.insert("source".into(), serde_json::json!(source.to_string_lossy()));
    inputs.insert("target".into(), serde_json::json!(target.to_string_lossy()));
    inputs.insert("module_name".into(), serde_json::json!("Service"));
    inputs.insert("delegate_field".into(), serde_json::json!("service"));
    inputs.insert("item_names".into(), serde_json::json!(["save"]));
    // Explicit bool — must survive as a JSON bool through the delegate param.
    inputs.insert("deep_analysis".into(), serde_json::json!(true));

    let inv = MacroInvocation {
        macro_id: "builtin.java.guice".into(),
        version: None,
        project_dir: dir.path().to_string_lossy().into_owned(),
        inputs,
        anchors: None,
        operator_opt_outs: vec![],
    };
    let ctx = MacroPlannerContext::default();
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan must succeed — deep_analysis must forward as a typed bool, not a string");
    assert!(plan.refusals.is_empty(), "no refusals: {:?}", plan.refusals);
}

// The shipped builtin must delegate to extract_java_class with the exact
// external-injection WiringSpec object — proving the Guice policy lives as
// macro data, not engine Rust.
#[test]
fn guice_macro_delegates_with_external_injection_wiring_spec() {
    use crate::model::MacroOperation;

    let def = MacroRegistry::get(None, "builtin.java.guice")
        .expect("registry get must not error")
        .expect("builtin.java.guice must be registered");

    let delegate = def
        .operations
        .iter()
        .find_map(|op| match op {
            MacroOperation::DelegateRefactor {
                refactor_kind,
                params,
            } if refactor_kind == "extract_java_class" => Some(params),
            _ => None,
        })
        .expect("macro must delegate to extract_java_class");

    let wiring = &delegate["wiring_mode"];
    assert_eq!(wiring["strategy"], serde_json::json!("external_injection"));
    assert_eq!(
        wiring["delegate_field_annotations"],
        serde_json::json!(["@Inject"])
    );
    assert_eq!(
        wiring["delegate_field_annotation_imports"],
        serde_json::json!(["javax.inject.Inject"])
    );
    assert_eq!(
        wiring["target_constructor_annotations"],
        serde_json::json!(["@Inject"])
    );
    assert_eq!(
        wiring["target_constructor_annotation_imports"],
        serde_json::json!(["javax.inject.Inject"])
    );
}
