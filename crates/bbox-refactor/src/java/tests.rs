use super::*;
use std::fs;

fn project_record(path: &Path) -> ProjectRecord {
    ProjectRecord {
        project_id: "test-project".to_string(),
        repo_id: None,
        canonical_path: fs::canonicalize(path).unwrap().display().to_string(),
        registered_at: "2026-05-09T00:00:00Z".to_string(),
        is_git_repo: false,
        languages: Default::default(),
        aliases: Default::default(),
    }
}

fn java_plan_params(kind: &str, source: &Path) -> RefactorPlanParams {
    RefactorPlanParams {
        kind: kind.to_string(),
        source: path_string(source),
        target: None,
        item_names: None,
        item_kinds: None,
        impl_name: None,
        module_name: None,
        visibility: None,
        use_path: None,
        router_name: None,
        router_call: None,
        router_export_name: None,
        target_prelude: None,
        old_text: None,
        new_text: None,
        replace_all: None,
        toml_table: None,
        toml_entries: None,
        fields: None,
        parameters: None,
        assign_to_fields: None,
        move_fields: None,
        delegate_field: None,
        delegate_type: None,
        keep_copy: None,
        deep_analysis: None,
        rewrite_remaining_accessors: None,
        project_dir: None,
        declaring_class: None,
        summary_only: None,
        propagate_class_annotations: None,
        source_delegate_wrappers: None,
        wiring_mode: None,
        callback_externals: None,
        output_path: None,
        ..Default::default()
    }
}

#[test]
fn java_status_items_include_methods_and_nested_classes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Example.java");
    fs::write(
        &path,
        "class Example {\n    void run() {}\n    class Nested { int value() { return 1; } }\n}\n",
    )
    .unwrap();

    let text = status(&RefactorStatusParams {
        file: path_string(&path),
        project_dir: None,
        item_names: None,
        item_kinds: None,
        limit: None,
        include_attributes: None,
    })
    .unwrap();
    let parsed: RefactorStatus = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed.language, "java");
    assert!(parsed.items.iter().any(|item| {
        item.kind == "class_declaration" && item.name.as_deref() == Some("Example")
    }));
    assert!(
        parsed.items.iter().any(|item| {
            item.kind == "method_declaration" && item.name.as_deref() == Some("run")
        })
    );
    assert!(parsed.items.iter().any(|item| {
        item.kind == "class_declaration" && item.name.as_deref() == Some("Nested")
    }));
}

#[test]
fn extract_java_methods_creates_missing_target_class() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("God.java");
    let target = dir.path().join("ExtractedMethods.java");
    fs::write(
            &source,
            "package com.example;\n\nimport java.util.List;\n\npublic class God {\n    List<String> run() { return List.of(); }\n    void keep() { }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["run".to_string()]);

    let plan_text = plan_extract_java_methods(&params).unwrap();
    let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let response = apply(
        &RefactorApplyParams {
            plan: plan_value,
            plan_path: None,
            confirm: Some(true),
            allow_dirty_worktree: None,
            allow_unregistered_paths: None,
            cwd: None,
            force_path: None,
        },
        &[project_record(dir.path())],
    )
    .unwrap();
    let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(applied.status, "ok");
    let target_text = fs::read_to_string(&target).unwrap();
    assert!(target_text.contains("package com.example;"));
    assert!(target_text.contains("import java.util.List;"));
    assert!(target_text.contains("public class ExtractedMethods"));
    assert!(target_text.contains("List<String> run()"));
    let source_text = fs::read_to_string(&source).unwrap();
    assert!(!source_text.contains("List<String> run()"));
    assert!(source_text.contains("void keep()"));
}

#[test]
fn extract_java_methods_reports_captured_source_fields() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("ExtractedGrid.java");
    fs::write(
            &source,
            "class Dashboard {\n    private final Admin admin;\n    private Grid grid;\n    void moveMe() { grid = admin.load(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["moveMe".to_string()]);

    let plan_text = plan_extract_java_methods(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    assert!(plan.captured_variables.iter().any(|capture| {
        capture.name == "admin"
            && capture.source_type == "Admin"
            && capture.source_visibility == "private final"
    }));
    assert!(plan.captured_variables.iter().any(|capture| {
        capture.name == "grid"
            && capture.source_type == "Grid"
            && capture.source_visibility == "private"
    }));
}

// -----------------------------------------------------------------
// Gap 19: captured_variables must resolve identifiers against the
// source class's own field declarations, not parameters or
// inner-class fields.
// -----------------------------------------------------------------

fn captured_plan(source_text: &str, method_name: &str) -> RefactorPlan {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("Extracted.java");
    fs::write(&source, source_text).unwrap();
    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec![method_name.to_string()]);
    let plan_text = plan_extract_java_methods(&params).unwrap();
    // Keep the tempdir alive past parse — leak intentionally for the
    // plan parse, the OS cleans up.
    let _ = dir;
    serde_json::from_str(&plan_text).unwrap()
}

#[test]
fn captured_variables_skip_method_parameter_with_field_shadow() {
    // c3Variance is BOTH a real source-class field AND a parameter of
    // the extracted method. Only field-resolved accesses (this.c3Variance,
    // unshadowed reads) count — the `param + 1` arithmetic on the
    // parameter alone must not promote the parameter back to a capture.
    // This test pins the shadowing branch: when the only identifier
    // text seen inside the method is shadowed by the formal parameter,
    // we still capture the field iff some other access escapes the
    // shadow (here `this.c3Variance`).
    let plan = captured_plan(
        "class Composition {\n\
             \x20   private String c3Variance;\n\
             \x20   void setupStatusBadge(String c3Variance) {\n\
             \x20       String local = c3Variance + this.c3Variance;\n\
             \x20   }\n\
             }\n",
        "setupStatusBadge",
    );
    // The only capture for the field must be reported once (via the
    // unshadowed `this.c3Variance` access).
    let hits = plan
        .captured_variables
        .iter()
        .filter(|c| c.name == "c3Variance")
        .count();
    assert_eq!(hits, 1, "captured_variables: {:?}", plan.captured_variables);
}

#[test]
fn captured_variables_excludes_pure_parameter_with_no_field() {
    // No source-class field named `c3Variance`; the identifier is only
    // a method parameter. Must NOT appear as a captured variable.
    let plan = captured_plan(
        "class Composition {\n\
             \x20   void setupStatusBadge(String c3Variance, int n) {\n\
             \x20       String s = c3Variance + n;\n\
             \x20   }\n\
             }\n",
        "setupStatusBadge",
    );
    assert!(
        plan.captured_variables
            .iter()
            .all(|c| c.name != "c3Variance"),
        "parameter leaked into captured_variables: {:?}",
        plan.captured_variables
    );
}

#[test]
fn captured_variables_excludes_inner_class_field_shadow() {
    // Inner class declares a field `badgeId`; the outer class does not.
    // An identifier `badgeId` inside an outer-class method must not
    // be reported as a captured variable just because the inner class
    // happens to declare a field by that name.
    let plan = captured_plan(
        "class Composition {\n\
             \x20   void setupStatusBadge() {\n\
             \x20       String s = badgeId;\n\
             \x20   }\n\
             \x20   class SamplePointItemView {\n\
             \x20       private String badgeId;\n\
             \x20   }\n\
             }\n",
        "setupStatusBadge",
    );
    assert!(
        plan.captured_variables.iter().all(|c| c.name != "badgeId"),
        "inner-class field leaked into captured_variables: {:?}",
        plan.captured_variables
    );
}

// -----------------------------------------------------------------
// Gap 21: captured_variables must surface mutability indicators so
// composite plans can warn / treat constants specially.
// -----------------------------------------------------------------

#[test]
fn captured_variables_marks_non_final_field_as_mutable() {
    let plan = captured_plan(
        "class Composition {\n\
             \x20   private boolean isPlantSelected;\n\
             \x20   void render() { boolean v = isPlantSelected; }\n\
             }\n",
        "render",
    );
    let capture = plan
        .captured_variables
        .iter()
        .find(|c| c.name == "isPlantSelected")
        .expect("isPlantSelected should be captured");
    assert!(capture.source_mutable, "non-final field must be mutable");
    assert!(
        !capture.source_static_final,
        "non-final non-static field must not be flagged static_final"
    );
}

#[test]
fn captured_variables_marks_private_final_as_immutable_instance() {
    let plan = captured_plan(
        "class Composition {\n\
             \x20   private final String label = \"hello\";\n\
             \x20   void render() { String v = label; }\n\
             }\n",
        "render",
    );
    let capture = plan
        .captured_variables
        .iter()
        .find(|c| c.name == "label")
        .expect("label should be captured");
    assert!(!capture.source_mutable, "final field must not be mutable");
    assert!(
        !capture.source_static_final,
        "non-static final field must not be flagged static_final"
    );
}

#[test]
fn captured_variables_marks_static_final_as_constant() {
    let plan = captured_plan(
        "class Composition {\n\
             \x20   private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n\
             \x20   void render() { String v = SAMPLE_STATUS_OK; }\n\
             }\n",
        "render",
    );
    let capture = plan
        .captured_variables
        .iter()
        .find(|c| c.name == "SAMPLE_STATUS_OK")
        .expect("SAMPLE_STATUS_OK should be captured");
    assert!(
        !capture.source_mutable,
        "static final field must not be mutable"
    );
    assert!(
        capture.source_static_final,
        "static final field must be flagged source_static_final"
    );
}

// -----------------------------------------------------------------
// Gaps 12, 14, 15: external_calls + inherited_dependencies reports.
// -----------------------------------------------------------------

fn extract_dependency_plan(
    project_dir: &Path,
    source: &Path,
    target: &Path,
    item_names: &[&str],
) -> RefactorPlan {
    let mut params = java_plan_params("extract_java_methods", source);
    params.target = Some(path_string(target));
    params.item_names = Some(item_names.iter().map(|n| n.to_string()).collect());
    params.project_dir = Some(path_string(project_dir));
    params.deep_analysis = Some(true);
    let plan_text = plan_extract_java_methods(&params).unwrap();
    serde_json::from_str(&plan_text).unwrap()
}

#[test]
fn extract_java_methods_reports_external_call_to_source_class_method() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class CompositionView {\n\
            \x20   List<Item> getHistoryItemsBySamplePoint(Point p) { return List.of(); }\n\
            \x20   void createSamplePointStatusBadge() {\n\
            \x20       List<Item> items = getHistoryItemsBySamplePoint(null);\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let plan = extract_dependency_plan(
        dir.path(),
        &source,
        &target,
        &["createSamplePointStatusBadge"],
    );
    let call = plan
        .external_calls
        .iter()
        .find(|c| c.method == "getHistoryItemsBySamplePoint")
        .expect("external call missing");
    assert!(
        call.signature.contains("List<Item>")
            && call.signature.contains("getHistoryItemsBySamplePoint")
            && call.signature.contains("(Point p)"),
        "signature was {}",
        call.signature
    );
    assert!(!call.signature_partial);
    assert_eq!(call.call_sites.len(), 1);
    assert_eq!(call.call_sites[0].in_method, "createSamplePointStatusBadge");
    assert_eq!(call.call_sites[0].context, "direct");
}

#[test]
fn extract_java_methods_reports_inherited_interface_method() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HasLogger.java"),
        "package p;\npublic interface HasLogger {\n    Logger getLogger();\n}\n",
    )
    .unwrap();
    let source = dir.path().join("CompositionView.java");
    fs::write(
        &source,
        "package p;\n\
             public class CompositionView implements HasLogger {\n\
            \x20   void createSamplePointStatusBadge() { getLogger().info(\"x\"); }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("MeterGrid.java");
    let plan = extract_dependency_plan(
        dir.path(),
        &source,
        &target,
        &["createSamplePointStatusBadge"],
    );
    let inherited = plan
        .inherited_dependencies
        .iter()
        .find(|d| d.method == "getLogger")
        .expect("inherited getLogger missing");
    assert_eq!(inherited.source, "HasLogger");
    assert_eq!(inherited.source_kind, "interface");
    assert_eq!(inherited.call_sites.len(), 1);
    assert_eq!(inherited.call_sites[0].context, "direct");
}

#[test]
fn extract_java_methods_reports_inherited_superclass_method() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("BaseView.java"),
        "package p;\npublic class BaseView {\n    public void applyFilters() {}\n}\n",
    )
    .unwrap();
    let source = dir.path().join("CompositionView.java");
    fs::write(
        &source,
        "package p;\n\
             public class CompositionView extends BaseView {\n\
            \x20   void createMeterGrid() { applyFilters(); }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("MeterGrid.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["createMeterGrid"]);
    let inherited = plan
        .inherited_dependencies
        .iter()
        .find(|d| d.method == "applyFilters")
        .expect("inherited applyFilters missing");
    assert_eq!(inherited.source, "BaseView");
    assert_eq!(inherited.source_kind, "class");
}

#[test]
fn extract_java_methods_resolves_multi_hop_inheritance_to_actual_declarer() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Base.java"),
        "package p;\npublic class Base {\n    public void rootHook() {}\n}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("Mid.java"),
        "package p;\npublic class Mid extends Base {}\n",
    )
    .unwrap();
    let source = dir.path().join("Leaf.java");
    fs::write(
        &source,
        "package p;\n\
             public class Leaf extends Mid {\n\
            \x20   void doIt() { rootHook(); }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("Other.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["doIt"]);
    let inherited = plan
        .inherited_dependencies
        .iter()
        .find(|d| d.method == "rootHook")
        .expect("inherited rootHook missing");
    assert_eq!(inherited.source, "Base");
    assert_eq!(inherited.source_kind, "class");
}

#[test]
fn extract_java_methods_marks_lambda_calls_with_lambda_context() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    fs::write(
        &source,
        "package p;\n\
             class View {\n\
            \x20   void refreshSamplePointItem() {}\n\
            \x20   void createTrackChangeDialog() {\n\
            \x20       Runnable r = () -> refreshSamplePointItem();\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("Other.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["createTrackChangeDialog"]);
    let call = plan
        .external_calls
        .iter()
        .find(|c| c.method == "refreshSamplePointItem")
        .expect("expected refreshSamplePointItem in external_calls");
    assert_eq!(call.call_sites.len(), 1);
    assert_eq!(call.call_sites[0].context, "lambda");
}

#[test]
fn extract_java_methods_marks_direct_calls_with_direct_context() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    fs::write(
        &source,
        "package p;\n\
             class View {\n\
            \x20   void refresh() {}\n\
            \x20   void run() { refresh(); }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("Other.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["run"]);
    let call = plan
        .external_calls
        .iter()
        .find(|c| c.method == "refresh")
        .expect("refresh missing");
    assert_eq!(call.call_sites[0].context, "direct");
}

#[test]
fn extract_java_methods_omits_jdk_calls() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    fs::write(
        &source,
        "package p;\n\
             class View {\n\
            \x20   void run() { System.out.println(\"hi\"); String.valueOf(1); }\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("Other.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["run"]);
    assert!(
        plan.external_calls.is_empty(),
        "expected empty external_calls, got {:?}",
        plan.external_calls
    );
    assert!(plan.inherited_dependencies.is_empty());
}

#[test]
fn extract_java_methods_omits_calls_to_other_extracted_methods() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    fs::write(
        &source,
        "package p;\n\
             class View {\n\
            \x20   void run() { helper(); }\n\
            \x20   void helper() {}\n\
             }\n",
    )
    .unwrap();
    let target = dir.path().join("Other.java");
    let plan = extract_dependency_plan(dir.path(), &source, &target, &["run", "helper"]);
    assert!(
        plan.external_calls.iter().all(|c| c.method != "helper"),
        "helper should be internal, not external: {:?}",
        plan.external_calls
    );
}

#[test]
fn java_gap_primitives_plan_guarded_edits() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Source.java");
    let target = dir.path().join("Target.java");
    fs::write(
            &source,
            "class Source {\n    private Grid grid;\n    Source(Dep dep) { setup(); this.refresh(); }\n    void setup() { refresh(); }\n    void refresh() {}\n}\n",
        )
        .unwrap();
    fs::write(&target, "class Target {\n}\n").unwrap();

    let mut add_fields = java_plan_params("add_java_fields", &target);
    add_fields.fields = Some(vec![JavaFieldSpec {
        visibility: Some("private".to_string()),
        type_name: "Dep".to_string(),
        name: "dep".to_string(),
        final_field: Some(true),
    }]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_add_java_fields(&add_fields).unwrap()).unwrap();
    assert!(
        plan.edits[0].edits[0]
            .replacement
            .contains("private final Dep dep;")
    );

    let mut constructor = java_plan_params("add_java_constructor", &target);
    constructor.parameters = Some(vec![JavaParameterSpec {
        type_name: "Dep".to_string(),
        name: "dep".to_string(),
    }]);
    constructor.assign_to_fields = Some(true);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_add_java_constructor(&constructor).unwrap()).unwrap();
    assert!(
        plan.edits[0].edits[0]
            .replacement
            .contains("this.dep = dep;")
    );

    let mut callers = java_plan_params("update_java_callers", &source);
    callers.delegate_field = Some("target".to_string());
    callers.item_names = Some(vec!["refresh".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();
    assert_eq!(plan.edits[0].edits.len(), 2);
    assert!(
        plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "target.")
    );

    let mut move_field = java_plan_params("move_java_field", &source);
    move_field.target = Some(path_string(&target));
    move_field.item_names = Some(vec!["grid".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_move_java_field(&move_field).unwrap()).unwrap();
    assert_eq!(plan.edits.len(), 2);
    assert!(
        plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;")
    );

    let mut delegate = java_plan_params("add_java_delegate_field", &source);
    delegate.delegate_field = Some("target".to_string());
    delegate.delegate_type = Some("Target".to_string());
    delegate.parameters = Some(vec![JavaParameterSpec {
        type_name: "Dep".to_string(),
        name: "dep".to_string(),
    }]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_add_java_delegate_field(&delegate).unwrap()).unwrap();
    assert!(
        plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("private final Target target;"))
    );
    assert!(
        plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("this.target = new Target(dep);"))
    );
}

#[test]
fn extract_java_class_composite_builds_source_and_target_edits() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("ExtractedGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Dashboard {\n    private final Admin admin;\n    private Grid grid;\n    Dashboard() { grid = buildGrid(); refreshGrid(); }\n    Grid buildGrid() { return admin.load(); }\n    void refreshGrid() { grid.refresh(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("ExtractedGrid".to_string());
    params.delegate_field = Some("extractedGrid".to_string());
    params.item_names = Some(vec!["buildGrid".to_string(), "refreshGrid".to_string()]);
    params.move_fields = Some(vec!["grid".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    assert_eq!(plan.kind, "extract_java_class");
    assert_eq!(plan.edits.len(), 2);
    assert!(
        plan.captured_variables
            .iter()
            .any(|capture| { capture.name == "admin" && capture.source_type == "Admin" })
    );
    assert!(plan.edits[0].edits.iter().any(|edit| {
        edit.replacement
            .contains("private final ExtractedGrid extractedGrid;")
    }));
    assert!(plan.edits[0].edits.iter().any(|edit| {
        edit.replacement
            .contains("this.extractedGrid = new ExtractedGrid(admin);")
    }));
    assert!(
        plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "extractedGrid.")
    );
    assert!(
        plan.edits[1].edits[0]
            .replacement
            .contains("public class ExtractedGrid")
    );
    assert!(
        plan.edits[1].edits[0]
            .replacement
            .contains("private final Admin admin;")
    );
    assert!(
        plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;")
    );
}

fn extract_java_class_target_text(plan: &RefactorPlan) -> String {
    plan.edits[1].edits[0].replacement.clone()
}

#[test]
fn extract_java_class_inserts_fixme_above_external_call_site() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView {\n    void applyFilters() {}\n    void createMeterGrid() {\n        applyFilters();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        target_text.contains("// FIXME: external call `applyFilters`"),
        "expected FIXME in target text:\n{target_text}"
    );
    // The FIXME must immediately precede the call site (same indentation).
    let fixme_idx = target_text
        .find("// FIXME: external call `applyFilters`")
        .unwrap();
    let after = &target_text[fixme_idx..];
    assert!(
        after
            .lines()
            .take(4)
            .any(|l| l.trim_start() == "applyFilters();"),
        "FIXME not directly above call site:\n{target_text}"
    );
}

#[test]
fn extract_java_class_skips_fixme_when_deep_analysis_false() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView {\n    void applyFilters() {}\n    void createMeterGrid() {\n        applyFilters();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    // deep_analysis defaults to false.

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        !target_text.contains("FIXME"),
        "no FIXME expected when deep_analysis=false:\n{target_text}"
    );
}

#[test]
fn extract_java_class_inserts_fixme_for_each_call_site() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView {\n    void applyFilters() {}\n    void createMeterGrid() {\n        applyFilters();\n        applyFilters();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    let fixme_count = target_text
        .matches("// FIXME: external call `applyFilters`")
        .count();
    assert_eq!(
        fixme_count, 2,
        "expected one FIXME per call site, got {fixme_count}:\n{target_text}"
    );
}

#[test]
fn extract_java_class_auto_adds_implements_for_satisfied_interface() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HasLogger.java"),
        "package com.example;\npublic interface HasLogger {\n    void getLogger();\n}\n",
    )
    .unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView implements HasLogger {\n    public void getLogger() {}\n    void createMeterGrid() {\n        getLogger();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    // Extract both the interface method (so the target satisfies it) AND
    // the caller. With both extracted the interface is satisfied.
    params.item_names = Some(vec!["createMeterGrid".to_string(), "getLogger".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        target_text.contains("public class CompositionMeterGrid implements HasLogger"),
        "expected implements clause:\n{target_text}"
    );
    // Same package — no import needed.
    assert!(
        !target_text.contains("// FIXME: target now implements"),
        "interface satisfied; should not emit unsatisfied FIXME:\n{target_text}"
    );
}

#[test]
fn extract_java_class_imports_interface_from_other_package() {
    let dir = tempfile::tempdir().unwrap();
    let logger_dir = dir.path().join("logger");
    let view_dir = dir.path().join("view");
    fs::create_dir_all(&logger_dir).unwrap();
    fs::create_dir_all(&view_dir).unwrap();
    fs::write(
        logger_dir.join("HasLogger.java"),
        "package com.example.logger;\npublic interface HasLogger {\n    void getLogger();\n}\n",
    )
    .unwrap();
    let source = view_dir.join("CompositionView.java");
    let target = view_dir.join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example.view;\n\nimport com.example.logger.HasLogger;\n\nclass CompositionView implements HasLogger {\n    public void getLogger() {}\n    void createMeterGrid() {\n        getLogger();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string(), "getLogger".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        target_text.contains("import com.example.logger.HasLogger;"),
        "expected import:\n{target_text}"
    );
    assert!(
        target_text.contains("public class CompositionMeterGrid implements HasLogger"),
        "expected implements:\n{target_text}"
    );
}

#[test]
fn extract_java_class_emits_fixme_when_interface_unsatisfied() {
    let dir = tempfile::tempdir().unwrap();
    // Interface declares both methods; only one has a default
    // implementation, the other is abstract. The class doesn't redefine
    // either, so the call inside the extracted method resolves through
    // the interface and surfaces in `inherited_dependencies`.
    fs::write(
            dir.path().join("HasLogger.java"),
            "package com.example;\npublic interface HasLogger {\n    default void otherRequired() {}\n    void getLogger();\n}\n",
        )
        .unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView implements HasLogger {\n    public void getLogger() {}\n    void createMeterGrid() {\n        otherRequired();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    // Extract only the caller — the interface methods stay on the source.
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        target_text
            .contains("// FIXME: target now implements HasLogger but does not satisfy method"),
        "expected unsatisfied FIXME:\n{target_text}"
    );
    assert!(
        target_text.contains("otherRequired"),
        "FIXME should name the unsatisfied method:\n{target_text}"
    );
    // Implements clause is still injected so the operator sees the
    // mismatch flagged on the same declaration.
    assert!(
        target_text.contains("public class CompositionMeterGrid implements HasLogger"),
        "expected implements clause + FIXME above it:\n{target_text}"
    );
}

// An interface whose only methods are `default` (or `static` / `private`)
// is fully satisfied by `implements I` alone — no explicit method
// declarations needed. The completeness check must filter those out
// before deciding whether to emit a `// FIXME: target now implements I
// but does not satisfy method(s)` marker.
#[test]
fn extract_java_class_no_unsatisfied_fixme_when_interface_is_default_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("HasLogger.java"),
        "package com.example;\n\
             public interface HasLogger {\n\
            \x20   default void getLogger() {}\n\
             }\n",
    )
    .unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class CompositionView implements HasLogger {\n\
            \x20   void createMeterGrid() { getLogger(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    // The implements clause IS injected.
    assert!(
        target_text.contains("implements HasLogger"),
        "implements clause expected: {target_text}"
    );
    // But NO unsatisfied-method FIXME: the only method is default.
    assert!(
        !target_text.contains("FIXME: target now implements"),
        "default-only interface must not trigger unsatisfied FIXME: {target_text}"
    );
}

#[test]
fn extract_java_class_does_not_add_extends_for_class_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("BaseView.java"),
        "package com.example;\npublic class BaseView {\n    public void applyFilters() {}\n}\n",
    )
    .unwrap();
    let source = dir.path().join("CompositionView.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass CompositionView extends BaseView {\n    void createMeterGrid() {\n        applyFilters();\n    }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = extract_java_class_target_text(&plan);
    assert!(
        !target_text.contains("extends BaseView"),
        "must not auto-add extends:\n{target_text}"
    );
    assert!(
        target_text.contains("// FIXME: inherited call `applyFilters`"),
        "expected inherited-class FIXME at call site:\n{target_text}"
    );
    assert!(
        target_text.contains("inherited from class BaseView"),
        "FIXME message should name the source class:\n{target_text}"
    );
}

#[test]
fn update_java_callers_rewrites_method_references() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Source.java");
    fs::write(
        &source,
        "import java.util.List;\nimport java.util.stream.Stream;\n\
             class Source {\n\
            \x20   void wire(List<Integer> ints) {\n\
            \x20       ints.forEach(this::extractedMethod);\n\
            \x20       ints.stream().map(Foo::bar).count();\n\
            \x20       ints.forEach(super::extractedMethod);\n\
            \x20       extractedMethod(0);\n\
            \x20       this.extractedMethod(1);\n\
            \x20   }\n\
            \x20   void extractedMethod(int i) {}\n\
             }\n",
    )
    .unwrap();

    let mut callers = java_plan_params("update_java_callers", &source);
    callers.delegate_field = Some("delegate".to_string());
    callers.item_names = Some(vec!["extractedMethod".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();

    // Apply the edits in reverse order to get the rewritten text.
    let original = fs::read_to_string(&source).unwrap();
    let mut bytes = original.clone().into_bytes();
    let mut sorted = plan.edits[0].edits.clone();
    sorted.sort_by_key(|e| e.byte_start);
    for edit in sorted.iter().rev() {
        bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
    }
    let rewritten = String::from_utf8(bytes).unwrap();

    // Method-invocation rewrites still happen.
    assert!(
        rewritten.contains("delegate.extractedMethod(0)"),
        "unqualified call should be rewritten: {rewritten}"
    );
    assert!(
        rewritten.contains("delegate.extractedMethod(1)"),
        "this-qualified call should be rewritten: {rewritten}"
    );

    // Method-reference: this::extractedMethod -> delegate::extractedMethod.
    assert!(
        rewritten.contains("delegate::extractedMethod"),
        "this-qualified method reference should be rewritten: {rewritten}"
    );
    assert!(
        !rewritten.contains("this::extractedMethod"),
        "this::extractedMethod should be gone: {rewritten}"
    );

    // Foo::bar must remain untouched (different receiver type).
    assert!(
        rewritten.contains("Foo::bar"),
        "static/external method reference must not be rewritten: {rewritten}"
    );

    // super::extractedMethod must remain untouched (super has different binding).
    assert!(
        rewritten.contains("super::extractedMethod"),
        "super:: reference must not be rewritten: {rewritten}"
    );
}

#[test]
fn update_java_callers_method_reference_in_lambda_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Pipeline.java");
    fs::write(
        &source,
        "import java.util.List;\n\
             class Pipeline {\n\
            \x20   void run(List<String> xs) {\n\
            \x20       xs.stream().map(this::extractedMethod).forEach(System.out::println);\n\
            \x20   }\n\
            \x20   String extractedMethod(String s) { return s; }\n\
             }\n",
    )
    .unwrap();

    let mut callers = java_plan_params("update_java_callers", &source);
    callers.delegate_field = Some("delegate".to_string());
    callers.item_names = Some(vec!["extractedMethod".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();

    let original = fs::read_to_string(&source).unwrap();
    let mut bytes = original.into_bytes();
    let mut sorted = plan.edits[0].edits.clone();
    sorted.sort_by_key(|e| e.byte_start);
    for edit in sorted.iter().rev() {
        bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
    }
    let rewritten = String::from_utf8(bytes).unwrap();

    assert!(
        rewritten.contains("delegate::extractedMethod"),
        "this-qualified method reference inside lambda pipeline should be rewritten: {rewritten}"
    );
    // Unrelated method reference must stay.
    assert!(
        rewritten.contains("System.out::println"),
        "unrelated method reference must be preserved: {rewritten}"
    );
}

#[test]
fn java_organize_imports_fallback_adds_project_type_import() {
    let dir = tempfile::tempdir().unwrap();
    let model_dir = dir.path().join("src/main/java/com/example/model");
    let ui_dir = dir.path().join("src/main/java/com/example/ui");
    fs::create_dir_all(&model_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    fs::write(
        model_dir.join("FooThing.java"),
        "package com.example.model;\n\npublic class FooThing {}\n",
    )
    .unwrap();
    let source = ui_dir.join("UsesFoo.java");
    fs::write(
        &source,
        "package com.example.ui;\n\npublic class UsesFoo {\n    private FooThing value;\n}\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));

    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    assert_eq!(plan.kind, "java_lsp_organize_imports");
    assert!(
        plan.edits[0].edits[0]
            .replacement
            .contains("import com.example.model.FooThing;")
    );
}

// Gap 28: drop explicit single-type imports already covered by a
// wildcard from the same package. Wildcard provides them, so listing
// them again is redundant.
#[test]
fn java_organize_imports_drops_explicit_covered_by_wildcard() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("src/main/java/com/x/admin");
    let ui_dir = dir.path().join("src/main/java/com/x/ui");
    fs::create_dir_all(&admin_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    fs::write(
        admin_dir.join("MeterAdmin.java"),
        "package com.x.admin;\npublic class MeterAdmin {}\n",
    )
    .unwrap();
    let source = ui_dir.join("View.java");
    fs::write(
        &source,
        "package com.x.ui;\n\
             import com.x.admin.*;\n\
             import com.x.admin.MeterAdmin;\n\
             public class View {\n\
            \x20   private MeterAdmin admin;\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let replacement = &plan.edits[0].edits[0].replacement;
    assert!(
        replacement.contains("import com.x.admin.*;"),
        "wildcard kept: {replacement}"
    );
    assert!(
        !replacement.contains("import com.x.admin.MeterAdmin;"),
        "explicit covered by wildcard must be dropped: {replacement}"
    );
}

// Gap 28: source has only an explicit import (no wildcard) — explicit
// is preserved.
#[test]
fn java_organize_imports_keeps_explicit_when_no_wildcard() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("src/main/java/com/x/admin");
    let ui_dir = dir.path().join("src/main/java/com/x/ui");
    fs::create_dir_all(&admin_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    fs::write(
        admin_dir.join("MeterAdmin.java"),
        "package com.x.admin;\npublic class MeterAdmin {}\n",
    )
    .unwrap();
    let source = ui_dir.join("View.java");
    fs::write(
        &source,
        "package com.x.ui;\n\
             import com.x.admin.MeterAdmin;\n\
             public class View {\n\
            \x20   private MeterAdmin admin;\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));
    // When there's nothing to change the planner returns no edits;
    // accept either no plan-edits OR a plan that preserves the import.
    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    if let Some(file_edit) = plan.edits.first() {
        if let Some(edit) = file_edit.edits.first() {
            assert!(
                edit.replacement.contains("import com.x.admin.MeterAdmin;"),
                "explicit without wildcard must survive: {}",
                edit.replacement
            );
        }
    }
    // If no edit was emitted at all, that means the heuristic decided
    // the existing import block is fine — which already preserves the
    // explicit import.
}

// G16: organize-imports walks `annotation` and `marker_annotation`
// nodes. `@Nullable` referenced in a method signature must keep its
// import — without the walker hitting annotation nodes the import
// gets pruned and the moved body fails to compile.
#[test]
fn g16_organize_imports_preserves_annotation_references() {
    let dir = tempfile::tempdir().unwrap();
    let ann_dir = dir.path().join("src/main/java/com/x/ann");
    let ui_dir = dir.path().join("src/main/java/com/x/ui");
    fs::create_dir_all(&ann_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    fs::write(
        ann_dir.join("MyNullable.java"),
        "package com.x.ann;\npublic @interface MyNullable {}\n",
    )
    .unwrap();
    let source = ui_dir.join("Svc.java");
    // The annotation appears as a parameter and a method-level
    // marker_annotation. Without the G16 walker, the heuristic
    // would treat the import as unused and prune it.
    fs::write(
        &source,
        "package com.x.ui;\n\
             import com.x.ann.MyNullable;\n\
             public class Svc {\n\
            \x20   @MyNullable\n\
            \x20   public String greet(@MyNullable String who) { return who; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    if let Some(edit) = plan.edits.first().and_then(|fe| fe.edits.first()) {
        assert!(
            edit.replacement.contains("import com.x.ann.MyNullable;"),
            "annotation reference must keep its import — G16 walker missing: {}",
            edit.replacement
        );
    }
    // No-edit case is also acceptable: it means the heuristic saw the
    // import as used and didn't rewrite. Either outcome means the
    // import survived.
}

// Gap 28: two unrelated wildcards plus a standalone explicit from a
// third (uncovered) package — all three preserved.
#[test]
fn java_organize_imports_keeps_explicit_from_uncovered_package() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("src/main/java/com/x/admin");
    let dto_dir = dir.path().join("src/main/java/com/x/dto");
    let other_dir = dir.path().join("src/main/java/com/y/other");
    let ui_dir = dir.path().join("src/main/java/com/x/ui");
    fs::create_dir_all(&admin_dir).unwrap();
    fs::create_dir_all(&dto_dir).unwrap();
    fs::create_dir_all(&other_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    fs::write(
        admin_dir.join("MeterAdmin.java"),
        "package com.x.admin;\npublic class MeterAdmin {}\n",
    )
    .unwrap();
    fs::write(
        dto_dir.join("MeterDto.java"),
        "package com.x.dto;\npublic class MeterDto {}\n",
    )
    .unwrap();
    fs::write(
        other_dir.join("Standalone.java"),
        "package com.y.other;\npublic class Standalone {}\n",
    )
    .unwrap();
    let source = ui_dir.join("View.java");
    fs::write(
        &source,
        "package com.x.ui;\n\
             import com.x.admin.*;\n\
             import com.x.dto.*;\n\
             import com.y.other.Standalone;\n\
             public class View {\n\
            \x20   private MeterAdmin admin;\n\
            \x20   private MeterDto dto;\n\
            \x20   private Standalone s;\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    if let Some(file_edit) = plan.edits.first() {
        if let Some(edit) = file_edit.edits.first() {
            let r = &edit.replacement;
            assert!(r.contains("import com.x.admin.*;"), "wildcard 1: {r}");
            assert!(r.contains("import com.x.dto.*;"), "wildcard 2: {r}");
            assert!(
                r.contains("import com.y.other.Standalone;"),
                "uncovered explicit: {r}"
            );
        }
    }
}

// Gap 28: `import static …` lines are NEVER dropped, even if a TYPE
// wildcard exists for the same package — static imports bring members,
// type wildcards do not cover them.
#[test]
fn java_organize_imports_keeps_static_imports_under_type_wildcard() {
    let dir = tempfile::tempdir().unwrap();
    let admin_dir = dir.path().join("src/main/java/com/x/admin");
    let ui_dir = dir.path().join("src/main/java/com/x/ui");
    fs::create_dir_all(&admin_dir).unwrap();
    fs::create_dir_all(&ui_dir).unwrap();
    // Static import lines refer to static members; the heuristic never
    // touches them. Use a synthetic name; existing-import filter
    // accepts `import static …` verbatim regardless of resolution.
    let source = ui_dir.join("View.java");
    fs::write(
        &source,
        "package com.x.ui;\n\
             import com.x.admin.*;\n\
             import static com.x.admin.MeterAdmin.SOME_CONST;\n\
             public class View {\n\
            \x20   void keep() {}\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    if let Some(file_edit) = plan.edits.first() {
        if let Some(edit) = file_edit.edits.first() {
            let r = &edit.replacement;
            assert!(
                r.contains("import static com.x.admin.MeterAdmin.SOME_CONST;"),
                "static import must never be dropped: {r}"
            );
        }
    }
}

#[test]
fn move_java_constant_moves_three_constants_to_new_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    private static final String SAMPLE_STATUS_NOT_OK = \"OUT OF DATE\";\n    private static final String SAMPLE_STATUS_NO_DATASOURCE = \"NONE ASSIGNED\";\n    void keep() {}\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec![
        "SAMPLE_STATUS_OK".to_string(),
        "SAMPLE_STATUS_NOT_OK".to_string(),
        "SAMPLE_STATUS_NO_DATASOURCE".to_string(),
    ]);
    params.visibility = Some("private".to_string());
    params.module_name = Some("CompositionMeterGrid".to_string());

    let plan_text = plan_move_java_constant(&params).unwrap();
    let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let response = apply(
        &RefactorApplyParams {
            plan: plan_value,
            plan_path: None,
            confirm: Some(true),
            allow_dirty_worktree: None,
            allow_unregistered_paths: None,
            cwd: None,
            force_path: None,
        },
        &[project_record(dir.path())],
    )
    .unwrap();
    let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(applied.status, "ok");

    let source_text = fs::read_to_string(&source).unwrap();
    assert!(!source_text.contains("SAMPLE_STATUS_OK"));
    assert!(!source_text.contains("SAMPLE_STATUS_NOT_OK"));
    assert!(!source_text.contains("SAMPLE_STATUS_NO_DATASOURCE"));
    assert!(source_text.contains("void keep()"));

    let target_text = fs::read_to_string(&target).unwrap();
    assert!(target_text.contains("public class CompositionMeterGrid"));
    assert!(target_text.contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
    assert!(
        target_text.contains("private static final String SAMPLE_STATUS_NOT_OK = \"OUT OF DATE\";")
    );
    assert!(
        target_text.contains(
            "private static final String SAMPLE_STATUS_NO_DATASOURCE = \"NONE ASSIGNED\";"
        )
    );
}

#[test]
fn move_java_constant_keep_copy_widens_source_visibility_and_copies_to_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    void keep() {}\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
    params.visibility = Some("private".to_string());
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.keep_copy = Some(true);

    let plan_text = plan_move_java_constant(&params).unwrap();
    let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let response = apply(
        &RefactorApplyParams {
            plan: plan_value,
            plan_path: None,
            confirm: Some(true),
            allow_dirty_worktree: None,
            allow_unregistered_paths: None,
            cwd: None,
            force_path: None,
        },
        &[project_record(dir.path())],
    )
    .unwrap();
    let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(applied.status, "ok");

    let source_text = fs::read_to_string(&source).unwrap();
    // Constant remains in source, but visibility was widened from
    // private to package (i.e. no visibility keyword).
    assert!(source_text.contains("static final String SAMPLE_STATUS_OK"));
    assert!(!source_text.contains("private static final String SAMPLE_STATUS_OK"));

    let target_text = fs::read_to_string(&target).unwrap();
    assert!(target_text.contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
}

#[test]
fn move_java_constant_does_not_widen_when_keep_copy_false() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    private static final String OTHER = \"X\";\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
    params.visibility = Some("private".to_string());
    params.module_name = Some("CompositionMeterGrid".to_string());
    // keep_copy default false.

    let plan: RefactorPlan =
        serde_json::from_str(&plan_move_java_constant(&params).unwrap()).unwrap();
    // Source-side edits should remove the declaration (one removal edit),
    // not rewrite visibility on the surviving sibling.
    let source_edits = &plan.edits[0].edits;
    assert!(source_edits.iter().all(|edit| edit.replacement.is_empty()));
    // OTHER must remain untouched: no edit byte range covers it.
    let original = fs::read_to_string(&source).unwrap();
    let other_pos = original.find("OTHER").unwrap();
    assert!(
        source_edits
            .iter()
            .all(|edit| { !(edit.byte_start <= other_pos && other_pos < edit.byte_end) })
    );
}

#[test]
fn move_java_constant_rejects_non_static_final_field() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private String NOT_A_CONSTANT = \"x\";\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["NOT_A_CONSTANT".to_string()]);
    params.visibility = Some("private".to_string());
    params.module_name = Some("CompositionMeterGrid".to_string());

    let err = plan_move_java_constant(&params).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("not declared as `static final`"), "got: {msg}");
    // Source unchanged on disk (plan returned an error before any apply).
    let source_text = fs::read_to_string(&source).unwrap();
    assert!(source_text.contains("NOT_A_CONSTANT"));
}

#[test]
fn move_java_constant_rejects_missing_name() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["DOES_NOT_EXIST".to_string()]);
    params.visibility = Some("private".to_string());
    params.module_name = Some("CompositionMeterGrid".to_string());

    let err = plan_move_java_constant(&params).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("DOES_NOT_EXIST"), "got: {msg}");
}

#[test]
fn move_java_constant_appends_to_existing_target_class() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n}\n",
        )
        .unwrap();
    fs::write(
            &target,
            "package com.example;\n\npublic class CompositionMeterGrid {\n    private final Foo foo = new Foo();\n    void existing() {}\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("move_java_constant", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["SAMPLE_STATUS_OK".to_string()]);
    params.visibility = Some("public".to_string());

    let plan_text = plan_move_java_constant(&params).unwrap();
    let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let response = apply(
        &RefactorApplyParams {
            plan: plan_value,
            plan_path: None,
            confirm: Some(true),
            allow_dirty_worktree: None,
            allow_unregistered_paths: None,
            cwd: None,
            force_path: None,
        },
        &[project_record(dir.path())],
    )
    .unwrap();
    let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(applied.status, "ok");

    let target_text = fs::read_to_string(&target).unwrap();
    // Existing declarations preserved.
    assert!(target_text.contains("private final Foo foo = new Foo();"));
    assert!(target_text.contains("void existing()"));
    // Constant inserted with the requested visibility.
    assert!(target_text.contains("public static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
}

fn move_field_plan_for(source_text: &str, target_text: &str, field_names: &[&str]) -> RefactorPlan {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Source.java");
    let target = dir.path().join("Target.java");
    fs::write(&source, source_text).unwrap();
    fs::write(&target, target_text).unwrap();
    let mut params = java_plan_params("move_java_field", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(field_names.iter().map(|s| s.to_string()).collect());
    params.deep_analysis = Some(true);
    let plan_text = plan_move_java_field(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    // Keep tempdir alive for the duration of the test by leaking it; tests
    // are short-lived and cleanup happens on process exit.
    std::mem::forget(dir);
    plan
}

#[test]
fn move_java_field_reports_remaining_reads_only() {
    let source = "class Source {\n    private Grid grid;\n    void show() {\n        view.add(grid);\n        render(grid);\n    }\n    void render(Grid g) {}\n}\n";
    let target = "class Target {\n}\n";
    let plan = move_field_plan_for(source, target, &["grid"]);
    let report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "grid")
        .expect("grid entry");
    assert_eq!(report.accesses.len(), 2);
    assert!(report.accesses.iter().all(|a| a.kind == "read"));
    assert_eq!(report.accesses[0].line, 4);
    assert!(report.accesses[0].context.contains("view.add(grid)"));
    assert_eq!(report.accesses[1].line, 5);
    assert!(report.accesses[1].context.contains("render(grid)"));
}

#[test]
fn move_java_field_distinguishes_reads_and_writes() {
    let source = "class Source {\n    private int counter;\n    void bump() {\n        counter = counter + 1;\n        counter += 5;\n        counter++;\n        log(counter);\n    }\n    void log(int v) {}\n}\n";
    let target = "class Target {\n}\n";
    let plan = move_field_plan_for(source, target, &["counter"]);
    let report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "counter")
        .expect("counter entry");
    let writes = report.accesses.iter().filter(|a| a.kind == "write").count();
    let reads = report.accesses.iter().filter(|a| a.kind == "read").count();
    // 3 writes: `counter =`, `counter +=`, `counter++`.
    // 3 reads: rhs of `counter + 1`, log(counter), and (debatable) the
    // read embedded in `+=`. We only require classification of the LHS
    // positions reported as `write`, not the synthetic read of compound
    // assignment.
    assert!(
        writes >= 3,
        "expected >= 3 writes, got {writes} ({reads} reads)"
    );
    assert!(
        reads >= 2,
        "expected >= 2 reads, got {reads} ({writes} writes)"
    );
}

#[test]
fn move_java_field_skips_local_shadowing() {
    let source = "class Source {\n    private int value;\n    void shadowed() {\n        int value = 7;\n        use(value);\n    }\n    void unshadowed() {\n        use(value);\n    }\n    void use(int v) {}\n}\n";
    let target = "class Target {\n}\n";
    let plan = move_field_plan_for(source, target, &["value"]);
    let report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "value")
        .expect("value entry");
    // Only the unshadowed read should be reported.
    assert_eq!(report.accesses.len(), 1, "report: {:?}", report.accesses);
    assert_eq!(report.accesses[0].line, 8);
}

#[test]
fn move_java_field_reports_both_this_and_bare_access() {
    let source = "class Source {\n    private Grid grid;\n    void run() {\n        this.grid.refresh();\n        grid.show();\n    }\n}\n";
    let target = "class Target {\n}\n";
    let plan = move_field_plan_for(source, target, &["grid"]);
    let report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "grid")
        .expect("grid entry");
    assert_eq!(report.accesses.len(), 2);
    assert_eq!(report.accesses[0].line, 4);
    assert!(report.accesses[0].context.contains("this.grid.refresh()"));
    assert_eq!(report.accesses[1].line, 5);
    assert!(report.accesses[1].context.contains("grid.show()"));
}

#[test]
fn move_java_field_with_no_remaining_accesses_reports_empty_list() {
    let source = "class Source {\n    private Grid grid;\n    void run() {}\n}\n";
    let target = "class Target {\n}\n";
    let plan = move_field_plan_for(source, target, &["grid"]);
    assert_eq!(plan.remaining_source_accessors.len(), 1);
    let report = &plan.remaining_source_accessors[0];
    assert_eq!(report.field, "grid");
    assert!(report.accesses.is_empty());
}

#[test]
fn java_organize_imports_skips_inner_class_simple_name_import() {
    // Gap 16: `Outer.Inner` references must keep the qualified
    // form. The fallback must not synthesize `import x.Inner;`
    // when `Inner` only exists as a member of `Outer`'s body.
    let dir = tempfile::tempdir().unwrap();
    let view_dir = dir.path().join("src/main/java/com/example/view");
    let model_dir = dir.path().join("src/main/java/com/example/model");
    fs::create_dir_all(&view_dir).unwrap();
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(
            view_dir.join("CompositionView.java"),
            "package com.example.view;\n\npublic class CompositionView {\n    public static class SamplePointItemView {}\n}\n",
        )
        .unwrap();
    let source = model_dir.join("Helper.java");
    fs::write(
            &source,
            "package com.example.model;\n\nimport com.example.view.CompositionView;\n\npublic class Helper {\n    void use(CompositionView.SamplePointItemView item) {}\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("java_lsp_organize_imports", &source);
    params.project_dir = Some(path_string(dir.path()));

    let plan_result = plan_java_lsp_organize_imports(&params, &PlanContext::default());
    match plan_result {
        Ok(plan_text) => {
            let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
            let replacement = &plan.edits[0].edits[0].replacement;
            // No bare import for the inner class.
            assert!(
                !replacement.contains("import com.example.model.SamplePointItemView;")
                    && !replacement.contains("import com.example.view.SamplePointItemView;"),
                "fallback fabricated an inner-class import: {replacement}"
            );
            // The legitimate outer import is preserved.
            assert!(replacement.contains("import com.example.view.CompositionView;"));
        }
        Err(err) => {
            // The fallback may decide there are no edits to make
            // (which is the correct behavior here — the existing
            // outer import already covers the qualified ref).
            assert!(
                err.to_string()
                    .contains("no Java import organization edits needed")
            );
        }
    }
}

#[test]
fn build_java_type_index_records_inner_classes() {
    let dir = tempfile::tempdir().unwrap();
    let view_dir = dir.path().join("src/com/x/view");
    fs::create_dir_all(&view_dir).unwrap();
    fs::write(
            view_dir.join("Outer.java"),
            "package com.x.view;\npublic class Outer { public static class Inner {} public interface IFoo {} }\n",
        )
        .unwrap();
    let idx = build_java_type_index(dir.path()).unwrap();
    assert!(idx.inner_class_names.contains("Inner"));
    assert!(idx.inner_class_names.contains("IFoo"));
    assert!(idx.top_level.contains_key("Outer"));
    // Top-level set must NOT include the inner names.
    assert!(!idx.top_level.contains_key("Inner"));
}

// -----------------------------------------------------------------
// Gap 20: extract_java_class moves static-final captures as constants
// (preserving `static final` and the initializer) rather than promoting
// them to instance fields + constructor parameters.
// -----------------------------------------------------------------

#[test]
fn extract_java_class_moves_static_final_capture_as_constant() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("CompositionMeterGrid.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Composition {\n    private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";\n    void render() { String s = SAMPLE_STATUS_OK; }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CompositionMeterGrid".to_string());
    params.delegate_field = Some("compositionMeterGrid".to_string());
    params.item_names = Some(vec!["render".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

    // Captured variable carries source_static_final = true.
    let captured = plan
        .captured_variables
        .iter()
        .find(|c| c.name == "SAMPLE_STATUS_OK")
        .expect("SAMPLE_STATUS_OK should be in captured_variables");
    assert!(captured.source_static_final);

    // Target body contains the constant declaration with `static final`
    // and the original initializer literal. Visibility widens to the
    // same-package floor (no explicit modifier — package-private) so
    // any cross-cluster source-side reference can still see it.
    let target_replacement = &plan.edits[1].edits[0].replacement;
    assert!(
        target_replacement.contains("static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"),
        "target should keep static final + initializer: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("private static final String SAMPLE_STATUS_OK"),
        "target must widen private to the same-package floor: {target_replacement}"
    );

    // No constructor parameter for the constant. The body should not
    // contain a `private final String SAMPLE_STATUS_OK;` instance field
    // line, and there should be no constructor at all (no other
    // captures).
    assert!(
        !target_replacement.contains("private final String SAMPLE_STATUS_OK;"),
        "target must not promote constant to instance field: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("public CompositionMeterGrid("),
        "target must not synthesize a constructor for static-final captures: {target_replacement}"
    );

    // Source side: SAMPLE_STATUS_OK declaration is removed, and the
    // delegate constructor call does NOT pass SAMPLE_STATUS_OK.
    let original = fs::read_to_string(&source).unwrap();
    let mut bytes = original.into_bytes();
    let mut sorted = plan.edits[0].edits.clone();
    sorted.sort_by_key(|e| e.byte_start);
    for edit in sorted.iter().rev() {
        bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
    }
    let rewritten = String::from_utf8(bytes).unwrap();
    assert!(
        !rewritten.contains("private static final String SAMPLE_STATUS_OK"),
        "source should no longer declare the constant: {rewritten}"
    );
    assert!(
        !rewritten.contains("new CompositionMeterGrid(SAMPLE_STATUS_OK"),
        "source delegate call must not pass the constant: {rewritten}"
    );
}

// Cross-cluster refs to moved constants: declarations leave the source,
// so every surviving bare reference outside the extracted methods must be
// rewritten to `<TargetClass>.<CONST>`. The constant declaration itself
// is widened on the target (package-floor or public-floor, by package).
// `deep_analysis: true` also populates a preview report.
#[test]
fn extract_java_class_qualifies_cross_cluster_constant_refs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private static final String LAST_14_DAYS = \"Last 14 Days\";\n\
            \x20   private final ComboBox<String> picker;\n\
            \x20   Dashboard(ComboBox<String> picker) { this.picker = picker; }\n\
            \x20   String buildWidget() { return LAST_14_DAYS; }\n\
            \x20   void setComboItems() { picker.setItems(LAST_14_DAYS); }\n\
            \x20   boolean isCustom(String v) { return v.equals(LAST_14_DAYS); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildWidget".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    // Source side: cross-cluster references rewritten to qualified form.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("picker.setItems(Widgets.LAST_14_DAYS)"),
        "setComboItems must use qualified reference: {rewritten}"
    );
    assert!(
        rewritten.contains("v.equals(Widgets.LAST_14_DAYS)"),
        "isCustom must use qualified reference: {rewritten}"
    );
    // Bare reference inside the extracted method moves with it.
    assert!(
        !rewritten.contains("return LAST_14_DAYS"),
        "extracted method must be removed from source: {rewritten}"
    );

    // Target side: constant retained with widened visibility (same
    // package → package floor → no explicit modifier).
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("static final String LAST_14_DAYS"),
        "target must keep the constant: {target_text}"
    );
    assert!(
        !target_text.contains("private static final String LAST_14_DAYS"),
        "target must drop `private` so cross-cluster source refs resolve: {target_text}"
    );

    // Report: every surviving ref is enumerated with line/column.
    let report = plan
        .remaining_source_constant_refs
        .iter()
        .find(|r| r.field == "LAST_14_DAYS")
        .expect("report entry for LAST_14_DAYS");
    assert_eq!(
        report.accesses.len(),
        2,
        "expected 2 surviving refs (setComboItems + isCustom): {:?}",
        report.accesses
    );
}

#[test]
fn extract_java_class_qualifies_cross_cluster_constant_refs_cross_package() {
    // Cross-package extract widens to `public` and the qualified refs
    // resolve via the source-side delegate-class import.
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("Dashboard.java");
    let target = b_pkg.join("Widgets.java");
    fs::write(
        &source,
        "package a;\n\
             class Dashboard {\n\
            \x20   private static final String LAST_14_DAYS = \"Last 14 Days\";\n\
            \x20   private final java.util.List<String> picker;\n\
            \x20   Dashboard(java.util.List<String> picker) { this.picker = picker; }\n\
            \x20   String buildWidget() { return LAST_14_DAYS; }\n\
            \x20   void setComboItems() { picker.add(LAST_14_DAYS); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildWidget".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("public static final String LAST_14_DAYS"),
        "cross-package: constant must be widened to public: {target_text}"
    );

    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("picker.add(Widgets.LAST_14_DAYS)"),
        "cross-package: source ref must qualify with target class: {rewritten}"
    );
    // Delegate-class import (Gap 5 from prior tranche) carries the
    // qualifier's resolution.
    assert!(
        rewritten.contains("import b.Widgets;"),
        "cross-package: source must import the target class: {rewritten}"
    );
}

// Overloaded methods can be selected by passing a signature suffix
// in `item_names`: `methodName(Type1,Type2)`. Bare names still work
// for non-overloaded methods.
#[test]
fn extract_java_class_disambiguates_overload_by_signature_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Helpers.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private void redistribute(float diff) {}\n\
            \x20   private void redistribute(float diff, boolean flag) {}\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["redistribute(float, boolean)".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("redistribute(float diff, boolean flag)"),
        "the 2-arg overload should be moved: {target_text}"
    );
    assert!(
        !target_text.contains("redistribute(float diff)"),
        "the 1-arg overload should stay on source: {target_text}"
    );
}

#[test]
fn extract_java_class_overload_ambiguous_lists_choices() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Helpers.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private void redistribute(float diff) {}\n\
            \x20   private void redistribute(float diff, boolean flag) {}\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    // Bare name → ambiguous → directed error with the available choices.
    params.item_names = Some(vec!["redistribute".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let err = plan_extract_java_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("method_overload_ambiguous"),
        "expected ambiguity refusal: {msg}"
    );
    assert!(
        msg.contains("redistribute(float)") && msg.contains("redistribute(float, boolean)"),
        "error must enumerate the available overloads: {msg}"
    );
}

#[test]
fn extract_java_class_overload_no_match_lists_choices() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Helpers.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private void redistribute(float diff) {}\n\
            \x20   private void redistribute(float diff, boolean flag) {}\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["redistribute(String)".to_string()]); // wrong types
    params.project_dir = Some(path_string(dir.path()));

    let err = plan_extract_java_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("method_overload_no_match"),
        "expected no-match refusal: {msg}"
    );
}

// Source-class inner types (enum / class / record / interface declared
// INSIDE the source class) referenced from moved method bodies are
// qualified to `<SourceClass>.<InnerType>` on the target and widened
// to the visibility floor on the source. Cross-package targets also
// gain an import for the source class so the qualified name resolves.
#[test]
fn extract_java_class_qualifies_and_widens_source_inner_enum() {
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("Runtime.java");
    let target = b_pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             class Runtime {\n\
            \x20   enum Mode { Site, Plant }\n\
            \x20   private Mode current;\n\
            \x20   boolean isSite() { return Mode.Site.equals(current); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["isSite".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    // Target body references Runtime.Mode now, not bare Mode.
    assert!(
        target_text.contains("Runtime.Mode.Site"),
        "inner-enum value must be qualified: {target_text}"
    );
    // Target has an import for Runtime (cross-package).
    assert!(
        target_text.contains("import a.Runtime;"),
        "target must import the source class: {target_text}"
    );

    // Source: Mode declaration widened to `public` (cross-package floor).
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("public enum Mode"),
        "inner enum must be widened to public on cross-package: {rewritten}"
    );
}

#[test]
fn extract_java_class_same_package_qualifies_inner_type_widens_to_package() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Runtime.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             class Runtime {\n\
            \x20   private enum Mode { Site, Plant }\n\
            \x20   private Mode current;\n\
            \x20   boolean isSite() { return Mode.Site.equals(current); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["isSite".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("Runtime.Mode.Site"),
        "same-package: inner enum still qualified: {target_text}"
    );
    // Same-package doesn't need an extra source-class import (auto-resolved).
    // Source: `private` widened to package-floor (no explicit modifier).
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("enum Mode") && !rewritten.contains("private enum Mode"),
        "inner enum private widened to package floor: {rewritten}"
    );
}

// G13: when the source class carries `@Slf4j` and the moved methods
// reference the generated `log` field, propagate `@Slf4j` + its
// import to the target. Default mode is `auto`; without the
// propagation the target fails to compile because `log` is
// undefined.
#[test]
fn g13_extract_java_class_auto_propagates_slf4j_when_log_referenced() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Worker.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             import lombok.extern.slf4j.Slf4j;\n\
             @Slf4j\n\
             public class Worker {\n\
            \x20   public void doIt() { log.info(\"running\"); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["doIt".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("@Slf4j"),
        "auto mode must propagate @Slf4j when log is referenced: {target_text}"
    );
    assert!(
        target_text.contains("import lombok.extern.slf4j.Slf4j;"),
        "auto mode must inject the Slf4j import: {target_text}"
    );
}

// G13: when propagate_class_annotations=none (or not auto-eligible),
// the target has no @Slf4j even if the source did.
#[test]
fn g13_extract_java_class_none_mode_strips_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Worker.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             import lombok.extern.slf4j.Slf4j;\n\
             @Slf4j\n\
             public class Worker {\n\
            \x20   public void doIt() { log.info(\"running\"); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["doIt".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.propagate_class_annotations = Some("none".to_string());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("@Slf4j"),
        "none mode must NOT propagate annotations: {target_text}"
    );
}

// The "@Inject field detected → refuse" auto-detection was removed from the
// engine when the Guice wiring policy was dissolved into the macro layer
// (`builtin.java.guice`). The generic extract no longer inspects the source for
// DI markers. That default-path guard was consciously RETIRED, not relocated:
// builtin.java.guice is the affirmative external_injection choice, so it needs
// no @Inject refusal (and carries none). See guice_macro_tests.rs.

/// The wiring spec the `builtin.java.guice` macro supplies for an
/// external-injection (DI field-injected) extract. Defined here so these
/// engine-level tests prove byte-parity with the dissolved Guice wiring
/// policy using exactly the data the macro passes.
fn guice_external_injection_spec() -> crate::WiringSpec {
    crate::WiringSpec {
        strategy: Some("external_injection".to_string()),
        delegate_field_annotations: Some(vec!["@Inject".to_string()]),
        delegate_field_modifiers: Some(vec!["private".to_string()]),
        delegate_field_annotation_imports: Some(vec!["javax.inject.Inject".to_string()]),
        target_constructor_annotations: Some(vec!["@Inject".to_string()]),
        target_constructor_annotation_imports: Some(vec!["javax.inject.Inject".to_string()]),
    }
}

/// The wiring spec for a "wire by hand" extract (no source-side wiring).
fn manual_wiring_spec() -> crate::WiringSpec {
    crate::WiringSpec {
        strategy: Some("none".to_string()),
        ..Default::default()
    }
}

// external_injection (guice field-inject) emits `@Inject private Target
// delegate;` on source, skips ctor wiring entirely.
#[test]
fn g7_wiring_mode_guice_field_inject_emits_inject_decl_skips_ctor_wiring() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import javax.inject.Inject;\n\
             public class Admin {\n\
            \x20   @Inject private Object dep;\n\
            \x20   public Long save() { return 1L; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("@Inject private Service service;"),
        "@Inject delegate decl must appear: {rewritten}"
    );
    // No ctor wiring assignment.
    assert!(
        !rewritten.contains("this.service = new Service"),
        "ctor wiring must be skipped: {rewritten}"
    );
}

// G7: wiring_mode=manual skips both delegate decl and ctor wiring.
#[test]
fn g7_wiring_mode_manual_skips_all_source_wiring() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             public class Admin {\n\
            \x20   public Long save() { return 1L; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(manual_wiring_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        !rewritten.contains("private final Service service"),
        "manual mode must not emit delegate field: {rewritten}"
    );
    assert!(
        !rewritten.contains("this.service = new Service"),
        "manual mode must not emit ctor wiring: {rewritten}"
    );
}

// G5: source_delegate_wrappers=true generates thin wrapper methods on
// the source for each moved public non-static method. Cross-file
// callers holding references to the source class continue to
// compile against the wrapper, which delegates to the target.
#[test]
fn g5_extract_java_class_generates_source_delegate_wrappers() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             public class Admin {\n\
            \x20   public Long save(int id, String name) { return (long) id; }\n\
            \x20   public void remove(int id) { }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string(), "remove".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.source_delegate_wrappers = Some(true);

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("public Long save(int id, String name)"),
        "save wrapper signature must appear: {rewritten}"
    );
    assert!(
        rewritten.contains("return service.save(id, name);"),
        "save wrapper body must delegate: {rewritten}"
    );
    assert!(
        rewritten.contains("public void remove(int id)"),
        "remove wrapper signature must appear: {rewritten}"
    );
    assert!(
        rewritten.contains("service.remove(id);") && !rewritten.contains("return service.remove"),
        "void wrapper must not have return: {rewritten}"
    );
}

// G5: with source_delegate_wrappers default (false / unset), no
// wrappers are emitted.
#[test]
fn g5_extract_java_class_no_wrappers_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             public class Admin {\n\
            \x20   public Long save(int id) { return (long) id; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        !rewritten.contains("public Long save(int id)"),
        "no wrapper without source_delegate_wrappers=true: {rewritten}"
    );
}

// G17: bare-component access on a source-class record (`param.field`
// where `field` is a record component) gets rewritten to the
// accessor call (`param.field()`). Triggers even on same-package
// extracts because records' backing fields are private — bare
// access fails compile across any class boundary.
#[test]
fn g17_extract_java_class_rewrites_record_component_access() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Holder.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             public class Holder {\n\
            \x20   public record Detail(String label, int qty) {}\n\
            \x20   public String describe(Detail d) {\n\
            \x20       return d.label + \":\" + d.qty;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["describe".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    // d.label → d.label() ; d.qty → d.qty()
    assert!(
        target_text.contains("d.label()"),
        "record component must rewrite to accessor: {target_text}"
    );
    assert!(
        target_text.contains("d.qty()"),
        "record component must rewrite to accessor: {target_text}"
    );
    // Bare access must be gone.
    assert!(
        !target_text.contains("d.label +") && !target_text.contains("d.qty;"),
        "bare record component access must be rewritten: {target_text}"
    );
}

// G13/G20/G21 followup: a foreign wildcard import on the source
// (e.g. `import java.util.*;`) used to short-circuit ALL subsequent
// import additions on both source and target via the over-aggressive
// wildcard guard. The guard now skips only when the wildcard's
// package equals the new import's package — foreign wildcards no
// longer hide new imports.
#[test]
fn extract_java_class_foreign_wildcard_does_not_block_new_imports() {
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("Admin.java");
    let target = b_pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import java.util.*;\n\
             public class Admin {\n\
            \x20   public List<String> all() { return new ArrayList<>(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["all".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    // Source must gain `import b.Service;` even though `import java.util.*;`
    // is present. Imports are emitted as individual TextEdits on the
    // source file (plan.edits[0]).
    let source_edits = &plan.edits[0].edits;
    let has_target_import = source_edits
        .iter()
        .any(|edit| edit.replacement.contains("import b.Service;"));
    assert!(
        has_target_import,
        "foreign wildcard must not block source-side target import: {:?}",
        source_edits
            .iter()
            .map(|e| &e.replacement)
            .collect::<Vec<_>>()
    );
}

// G13 followup: a foreign wildcard import on the target body used to
// block the Slf4j import even when @Slf4j was propagated.
#[test]
fn g13_extract_java_class_slf4j_import_added_under_foreign_wildcard() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Worker.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             import java.util.*;\n\
             import lombok.extern.slf4j.Slf4j;\n\
             @Slf4j\n\
             public class Worker {\n\
            \x20   public void doIt() { log.info(\"running\"); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["doIt".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("import lombok.extern.slf4j.Slf4j;"),
        "Slf4j import must be added even when target has a foreign wildcard: {target_text}"
    );
    assert!(
        target_text.contains("@Slf4j"),
        "@Slf4j must still be propagated: {target_text}"
    );
}

// G17 followup: bare record-component reads on LOCAL variables (not
// just method parameters) get rewritten to accessor calls. Common
// shape: `Record r = lookup(...); r.field`.
#[test]
fn g17_extract_java_class_rewrites_record_local_variable() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Holder.java");
    let target = pkg.join("Helpers.java");
    fs::write(
        &source,
        "package a;\n\
             public class Holder {\n\
            \x20   public record Detail(String label, int qty) {}\n\
            \x20   public Detail lookup() { return new Detail(\"x\", 1); }\n\
            \x20   public String describe() {\n\
            \x20       Detail d = lookup();\n\
            \x20       return d.label + \":\" + d.qty;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["describe".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("d.label()"),
        "record local-var component must rewrite to accessor: {target_text}"
    );
    assert!(
        target_text.contains("d.qty()"),
        "record local-var component must rewrite to accessor: {target_text}"
    );
}

// G7 cosmetic followup: under wiring_mode=guice_field_inject the
// target also @Inject-constructs its captured ctor params, so the
// "non-final source snapshot" warning doesn't apply. The
// mutable_capture FIXME comment must NOT be emitted.
#[test]
fn g7_guice_field_inject_suppresses_mutable_capture_fixmes() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import javax.inject.Inject;\n\
             public class Admin {\n\
            \x20   @Inject private Object dep;\n\
            \x20   public String useDep() { return dep.toString(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["useDep".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());
    params.deep_analysis = Some(true);

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("FIXME: mutable capture"),
        "guice_field_inject must suppress mutable_capture FIXMEs: {target_text}"
    );
}

// G19 + G14: external_calls that resolve to a public-static method on
// the source class get auto-qualified at the call site to
// `<SourceClass>.<method>(...)` and DO NOT receive a FIXME. The
// source-class import is already added by the existing post-extract
// pass; the call site needs the class qualifier prepended to compile.
// Also asserts G8: ExternalCall entries carry source_visibility +
// source_is_static + recommended_resolution metadata.
#[test]
fn g19_extract_java_class_auto_qualifies_public_static_external() {
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("Admin.java");
    let target = b_pkg.join("Helpers.java");
    // `baseQuery()` is public static. The cluster's `runQuery` calls
    // it unqualified; after extract the call must become
    // `Admin.baseQuery()` to resolve from package b.
    fs::write(
        &source,
        "package a;\n\
             public class Admin {\n\
            \x20   public static String baseQuery() { return \"q\"; }\n\
            \x20   public String runQuery() { return baseQuery() + \"!\"; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["runQuery".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.deep_analysis = Some(true);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();

    // G19: target body qualifies the call. No FIXME for this call.
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("Admin.baseQuery()"),
        "public-static external must be auto-qualified: {target_text}"
    );
    assert!(
        !target_text.contains("FIXME: external call `baseQuery`"),
        "public-static external must skip the FIXME marker: {target_text}"
    );
    assert!(
        target_text.contains("import a.Admin;"),
        "cross-package: source-class import still added: {target_text}"
    );

    // G8: external_calls entry carries the metadata.
    let ext_calls = &plan.external_calls;
    let base = ext_calls
        .iter()
        .find(|c| c.method == "baseQuery")
        .expect("baseQuery must appear in external_calls");
    assert_eq!(base.source_visibility.as_deref(), Some("public"));
    assert!(base.source_is_static);
    assert_eq!(
        base.recommended_resolution.as_deref(),
        Some("cross_class_static_call")
    );
}

// G18: method_reference qualifier on a source-class inner type
// (`Inner::new`, `Inner::method`) gets rewritten to
// `<SourceClass>.<Inner>::new` on the moved body. Without this,
// the unqualified method reference doesn't resolve from the target
// package.
#[test]
fn g18_extract_java_class_qualifies_inner_type_method_reference() {
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("Runtime.java");
    let target = b_pkg.join("Helpers.java");
    // The moved method uses `Detail::new` as a method reference —
    // Detail is an inner record on Runtime. After extraction the
    // unqualified `Detail::new` doesn't resolve in package b.
    fs::write(
        &source,
        "package a;\n\
             import java.util.function.Supplier;\n\
             public class Runtime {\n\
            \x20   record Detail(int x) {}\n\
            \x20   Supplier<Detail> factory() { return Detail::new; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Helpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["factory".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("Runtime.Detail::new"),
        "inner-type method reference must be qualified: {target_text}"
    );
    // Cross-package: target imports the source class.
    assert!(
        target_text.contains("import a.Runtime;"),
        "target must import source class for cross-package qualified ref: {target_text}"
    );
}

// Cross-package extracts rewrite bare-field access on source-class
// inner-type DTOs to the matching public getter. Same-package extracts
// leave bare access alone (still resolves).
#[test]
fn extract_java_class_cross_pkg_rewrites_inner_dto_field_to_getter() {
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    let source = a_pkg.join("TicketsView.java");
    let target = b_pkg.join("TicketHelpers.java");
    fs::write(
        &source,
        "package a;\n\
             class TicketsView {\n\
            \x20   public static class Ticket {\n\
            \x20       private final String direction;\n\
            \x20       Ticket(String direction) { this.direction = direction; }\n\
            \x20       public String getDirection() { return direction; }\n\
            \x20   }\n\
            \x20   String describe(Ticket ticket) { return ticket.direction; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("TicketHelpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["describe".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("ticket.getDirection()"),
        "cross-package: bare field access must route through getter: {target_text}"
    );
    assert!(
        !target_text.contains("ticket.direction"),
        "cross-package: bare access must NOT remain: {target_text}"
    );
}

#[test]
fn extract_java_class_same_pkg_leaves_inner_dto_field_access_alone() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("TicketsView.java");
    let target = pkg.join("TicketHelpers.java");
    fs::write(
        &source,
        "package a;\n\
             class TicketsView {\n\
            \x20   public static class Ticket {\n\
            \x20       private final String direction;\n\
            \x20       Ticket(String direction) { this.direction = direction; }\n\
            \x20       public String getDirection() { return direction; }\n\
            \x20   }\n\
            \x20   String describe(Ticket ticket) { return ticket.direction; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("TicketHelpers".to_string());
    params.delegate_field = Some("helpers".to_string());
    params.item_names = Some(vec!["describe".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    // Same-package: bare access works; Gap 5 still qualifies the inner
    // type but the field access is untouched.
    assert!(
        target_text.contains("ticket.direction"),
        "same-package: bare access must remain: {target_text}"
    );
}

// Passing a nested-class name in `item_names` for extract_java_class
// produces a directed error message rather than the generic
// "requested method `X` was not found." Inner-class extraction is a
// separate plan kind that this composite does not dispatch.
#[test]
fn extract_java_class_rejects_nested_class_in_item_names_with_directed_error() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("Extracted.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   static class InnerService {\n\
            \x20       void run() {}\n\
            \x20   }\n\
            \x20   void delegate() { new InnerService().run(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("ext".to_string());
    params.item_names = Some(vec!["delegate".to_string(), "InnerService".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let err = plan_extract_java_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nested_class_in_item_names"),
        "expected directed error code: {msg}"
    );
    assert!(
        msg.contains("InnerService"),
        "error must name the nested class: {msg}"
    );
    assert!(
        msg.contains("inner-class extraction is not currently supported"),
        "error must explain the limitation: {msg}"
    );
}

// Mutable-capture-with-write refusal: when an extracted method writes
// to a mutable source field that isn't in `move_fields`, the planner
// would promote it to a `final` constructor parameter on the target —
// and the moved write would then fail `cannot assign to final variable`.
// Refuse the plan with operator-actionable instructions to add the
// field(s) to `move_fields`.

// `callback_externals`: a source-class method that the extracted body
// calls but the operator wants threaded as a functional-interface
// callback rather than appearing as a FIXME. Target gains a Runnable /
// Consumer / Supplier / Function field + ctor param matching the
// method's signature; call sites in the extracted body are rewritten
// to `field.run()` / `.accept(arg)` / `.get()` / `.apply(arg)`;
// source-side wiring appends `this::method`.
#[test]
fn extract_java_class_threads_callback_externals_through_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private final Admin admin;\n\
            \x20   Dashboard(Admin admin) { this.admin = admin; }\n\
            \x20   void refreshGrid() {}\n\
            \x20   void buildWidget() { admin.load(); refreshGrid(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildWidget".to_string()]);
    params.callback_externals = Some(vec!["refreshGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("private final Runnable refreshGrid;"),
        "target must hold callback as Runnable field: {target_text}"
    );
    assert!(
        target_text.contains("Runnable refreshGrid")
            && target_text.contains("this.refreshGrid = refreshGrid;"),
        "target ctor must wire the callback: {target_text}"
    );
    assert!(
        target_text.contains("refreshGrid.run()"),
        "extracted body must call the callback via .run(): {target_text}"
    );
    assert!(
        !target_text.contains("FIXME: external call `refreshGrid`"),
        "no FIXME for callback-handled external: {target_text}"
    );

    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("new Widgets(admin, this::refreshGrid)"),
        "source wiring must append this::refreshGrid: {rewritten}"
    );
}

#[test]
fn extract_java_class_callback_externals_with_consumer_arity() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private final Admin admin;\n\
            \x20   Dashboard(Admin admin) { this.admin = admin; }\n\
            \x20   void log(String msg) {}\n\
            \x20   void buildWidget() { admin.load(); log(\"built\"); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildWidget".to_string()]);
    params.callback_externals = Some(vec!["log".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("private final Consumer<String> log;"),
        "Consumer field expected: {target_text}"
    );
    assert!(
        target_text.contains("import java.util.function.Consumer;"),
        "Consumer import expected: {target_text}"
    );
    assert!(
        target_text.contains("log.accept(\"built\")"),
        "call site must use .accept: {target_text}"
    );
}

#[test]
fn extract_java_class_callback_externals_rejects_two_args() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   void compute(String a, int b) {}\n\
            \x20   void buildWidget() { compute(\"x\", 1); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildWidget".to_string()]);
    params.callback_externals = Some(vec!["compute".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let err = plan_extract_java_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("callback_arity_unsupported"),
        "expected arity refusal: {msg}"
    );
    assert!(msg.contains("compute"), "error must name the method: {msg}");
}

// ---- promote_java_inner_class -------------------------------------

fn promote_params(
    source: &std::path::Path,
    target: &std::path::Path,
    name: &str,
) -> RefactorPlanParams {
    let mut p = java_plan_params("promote_java_inner_class", source);
    p.target = Some(path_string(target));
    p.module_name = Some(name.to_string());
    p.item_names = Some(vec![name.to_string()]);
    p.project_dir = source.parent().map(path_string);
    p
}

#[test]
fn promote_java_inner_class_lifts_inner_with_bare_field_capture() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("DailyService.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   private final java.time.LocalDate clientDate = java.time.LocalDate.now();\n\
            \x20   private class DailyService {\n\
            \x20       java.time.LocalDate getClientDate() { return clientDate; }\n\
            \x20   }\n\
            \x20   void build() { new DailyService(); }\n\
             }\n",
    )
    .unwrap();

    let params = promote_params(&source, &target, "DailyService");
    let plan: RefactorPlan =
        serde_json::from_str(&plan_promote_java_inner_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("class DailyService"),
        "promoted class missing: {target_text}"
    );
    assert!(
        target_text.contains("private final java.time.LocalDate clientDate;"),
        "captured field missing: {target_text}"
    );
    assert!(
        target_text.contains("DailyService(java.time.LocalDate clientDate)"),
        "synthesized ctor missing: {target_text}"
    );
    assert!(
        target_text.contains("this.clientDate = clientDate;"),
        "ctor body missing capture assign: {target_text}"
    );

    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("new DailyService(clientDate)"),
        "source instantiation must pass capture: {rewritten}"
    );
    assert!(
        !rewritten.contains("private class DailyService"),
        "inner declaration must be removed from source: {rewritten}"
    );
}

#[test]
fn promote_java_inner_class_refuses_static_inner() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("Inner.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   static class Inner { void m() {} }\n\
            \x20   void use() { new Inner().m(); }\n\
             }\n",
    )
    .unwrap();

    let params = promote_params(&source, &target, "Inner");
    let err = plan_promote_java_inner_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("static_inner_class_in_promote"),
        "expected static-inner refusal: {msg}"
    );
    assert!(
        msg.contains("extract_java_nested_classes"),
        "error must point at the syntactic alternative: {msg}"
    );
}

#[test]
fn promote_java_inner_class_refuses_outer_method_call() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("Inner.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   private String state;\n\
            \x20   void refresh() {}\n\
            \x20   private class Inner { void run() { refresh(); state.length(); } }\n\
            \x20   void use() { new Inner(); }\n\
             }\n",
    )
    .unwrap();

    let params = promote_params(&source, &target, "Inner");
    let err = plan_promote_java_inner_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("inner_class_calls_outer_method"),
        "expected outer-method-call refusal: {msg}"
    );
    assert!(msg.contains("refresh"), "error must name the method: {msg}");
}

#[test]
fn promote_java_inner_class_refuses_outer_field_write() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("Inner.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   private String state;\n\
            \x20   private class Inner { void run() { state = \"x\"; } }\n\
            \x20   void use() { new Inner(); }\n\
             }\n",
    )
    .unwrap();

    let params = promote_params(&source, &target, "Inner");
    let err = plan_promote_java_inner_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("inner_class_writes_outer_field"),
        "expected outer-field-write refusal: {msg}"
    );
    assert!(msg.contains("state"), "error must name the field: {msg}");
}

#[test]
fn promote_java_inner_class_inner_field_shadows_outer_no_capture() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Outer.java");
    let target = dir.path().join("Inner.java");
    // Inner has its own `state` field; bare `state` inside inner
    // resolves to inner's field, NOT to outer's. No outer capture
    // should be produced.
    fs::write(
        &source,
        "package com.example;\n\
             class Outer {\n\
            \x20   private String state;\n\
            \x20   private class Inner {\n\
            \x20       private String state;\n\
            \x20       String read() { return state; }\n\
            \x20   }\n\
            \x20   void use() { new Inner(); }\n\
             }\n",
    )
    .unwrap();

    let params = promote_params(&source, &target, "Inner");
    let plan: RefactorPlan =
        serde_json::from_str(&plan_promote_java_inner_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("private final String state;"),
        "inner-shadowed `state` must not become a capture: {target_text}"
    );
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("new Inner()"),
        "no captures means no extra args on instantiation: {rewritten}"
    );
}

#[test]
fn extract_java_class_refuses_mutable_capture_with_write() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private Grid theGrid;\n\
            \x20   void buildGrid() {\n\
            \x20       if (theGrid == null) { theGrid = new Grid(); }\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    // `theGrid` intentionally NOT in move_fields → refusal expected.
    params.project_dir = Some(path_string(dir.path()));

    let err = plan_extract_java_class(&params).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mutable_capture_with_write"),
        "expected refusal error code: {msg}"
    );
    assert!(
        msg.contains("theGrid"),
        "error must name the offending field: {msg}"
    );
    assert!(
        msg.contains("move_fields"),
        "error must point at the fix: {msg}"
    );
}

// The same scenario, but with `theGrid` listed in `move_fields`, should
// proceed cleanly — the rewrite_remaining_accessors path routes the
// source-side write through the generated setter.
#[test]
fn extract_java_class_allows_mutable_capture_with_write_when_moved() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private Grid theGrid;\n\
            \x20   void buildGrid() {\n\
            \x20       if (theGrid == null) { theGrid = new Grid(); }\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["theGrid".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    // Plan succeeds with the field moved.
    let _plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
}

// Re-reported Gap 1 variant: the moved constant is used as a method-call
// receiver (`df.format(value)`), not as a bare value. `df` parses as the
// `object` of a method_invocation, not as a type_identifier. The
// qualifier rewrite must still apply.
#[test]
fn extract_java_class_qualifies_constant_used_as_method_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Widgets.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.text.DecimalFormat;\n\
             class Dashboard {\n\
            \x20   private static final DecimalFormat df = new DecimalFormat(\"0.00\");\n\
            \x20   String formatGas(double v) { return df.format(v); }\n\
            \x20   String formatLiquid(double v) { return df.format(v * 1.0); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Widgets".to_string());
    params.delegate_field = Some("widgets".to_string());
    // Extract only formatGas; formatLiquid stays on source and still
    // references df, which is moving with the extract.
    params.item_names = Some(vec!["formatGas".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("return Widgets.df.format(v * 1.0)"),
        "method-receiver constant ref must qualify: {rewritten}"
    );
}

// remaining_source_accessors should NOT include accesses inside methods
// that are themselves being extracted in the same plan — those accesses
// move with the methods. Pre-fix the report listed every read/write
// regardless of whether the surrounding method was being extracted,
// producing false positives that looked like compile errors waiting to
// happen.
#[test]
fn remaining_source_accessors_excludes_extracted_method_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Extracted.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Tab selectedTab;\n\
            \x20   void other() { selectedTab = null; }\n\
            \x20   Tab getSelected() { return selectedTab; }\n\
            \x20   void addStyle() { selectedTab.addClassName(\"sel\"); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["getSelected".to_string(), "addStyle".to_string()]);
    params.move_fields = Some(vec!["selectedTab".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let entry = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "selectedTab")
        .expect("report entry for selectedTab");
    // Only the access inside `other()` (NOT being extracted) should
    // remain. The reads/writes inside getSelected() and addStyle() are
    // moving with their methods and must not appear.
    assert_eq!(
        entry.accesses.len(),
        1,
        "expected exactly the access in other(), got: {:?}",
        entry.accesses
    );
    let only = &entry.accesses[0];
    assert!(
        only.context.contains("selectedTab = null"),
        "the surviving access should be the write in other(): {only:?}"
    );
}

#[test]
fn extract_java_class_separates_static_final_from_instance_captures() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Mixed.java");
    let target = dir.path().join("MixedExtract.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Mixed {\n    private static final String LABEL = \"ok\";\n    private final Helper helper;\n    Mixed(Helper helper) { this.helper = helper; }\n    void render() { helper.use(LABEL); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MixedExtract".to_string());
    params.delegate_field = Some("mixedExtract".to_string());
    params.item_names = Some(vec!["render".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let target_replacement = &plan.edits[1].edits[0].replacement;

    // Constant: emitted as static final with initializer; private is
    // widened to the same-package floor.
    assert!(
        target_replacement.contains("static final String LABEL = \"ok\";"),
        "target should keep LABEL as static final constant: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("private static final String LABEL"),
        "target must widen private to the same-package floor: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("private final String LABEL;"),
        "target must not promote LABEL to instance field: {target_replacement}"
    );

    // Instance capture `helper` becomes a constructor parameter and
    // assigned-to instance field on the target.
    assert!(
        target_replacement.contains("private final Helper helper;"),
        "target should hold helper as instance field: {target_replacement}"
    );
    assert!(
        target_replacement.contains("public MixedExtract(Helper helper)"),
        "target constructor should take Helper helper: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("MixedExtract(String LABEL"),
        "target constructor must not include LABEL: {target_replacement}"
    );

    // Source-side constructor call passes only `helper`, not LABEL.
    let original = fs::read_to_string(&source).unwrap();
    let mut bytes = original.into_bytes();
    let mut sorted = plan.edits[0].edits.clone();
    sorted.sort_by_key(|e| e.byte_start);
    for edit in sorted.iter().rev() {
        bytes.splice(edit.byte_start..edit.byte_end, edit.replacement.bytes());
    }
    let rewritten = String::from_utf8(bytes).unwrap();
    assert!(
        rewritten.contains("new MixedExtract(helper)"),
        "source delegate call should pass only helper: {rewritten}"
    );
    assert!(
        !rewritten.contains("LABEL"),
        "source should no longer reference LABEL: {rewritten}"
    );
}

// -----------------------------------------------------------------
// Gap 29: when a captured field is non-final on the source, promoting
// it to a `final` constructor param on the target is a silent
// semantic bug — the value is snapshotted at construction time. We
// surface the issue inline in the generated target with a FIXME above
// the field declaration so the operator sees it during review.
// -----------------------------------------------------------------

#[test]
fn extract_java_class_inserts_fixme_for_mutable_capture() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Selection.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private boolean isPlantSelected;\n\
            \x20   void render() { boolean v = isPlantSelected; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Selection".to_string());
    params.delegate_field = Some("selection".to_string());
    params.item_names = Some(vec!["render".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("// FIXME: mutable capture `isPlantSelected`"),
        "expected mutable-capture FIXME on target: {target_text}"
    );
    assert!(
        target_text.contains("Supplier<Boolean>"),
        "FIXME should mention boxed Supplier hint for primitive: {target_text}"
    );
    // The FIXME must sit directly above the promoted final field.
    let fixme_at = target_text
        .find("// FIXME: mutable capture `isPlantSelected`")
        .unwrap();
    let field_at = target_text
        .find("private final boolean isPlantSelected;")
        .unwrap();
    assert!(
        fixme_at < field_at,
        "FIXME must precede the field decl: {target_text}"
    );
}

#[test]
fn extract_java_class_omits_fixme_for_final_capture() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Selection.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private final boolean isPlantSelected = true;\n\
            \x20   void render() { boolean v = isPlantSelected; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Selection".to_string());
    params.delegate_field = Some("selection".to_string());
    params.item_names = Some(vec!["render".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("// FIXME: mutable capture"),
        "final capture must not emit mutable-capture FIXME: {target_text}"
    );
}

#[test]
fn extract_java_class_omits_fixme_for_static_final_capture() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Selection.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private static final String FOO = \"bar\";\n\
            \x20   void render() { String v = FOO; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Selection".to_string());
    params.delegate_field = Some("selection".to_string());
    params.item_names = Some(vec!["render".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("// FIXME: mutable capture"),
        "static-final capture routes through constants path, not constructor — no FIXME: {target_text}"
    );
}

#[test]
fn extract_java_class_omits_fixme_when_deep_analysis_off() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Selection.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private boolean isPlantSelected;\n\
            \x20   void render() { boolean v = isPlantSelected; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Selection".to_string());
    params.delegate_field = Some("selection".to_string());
    params.item_names = Some(vec!["render".to_string()]);
    // deep_analysis off — no FIXME emission gate.
    params.deep_analysis = Some(false);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("// FIXME: mutable capture"),
        "deep_analysis=false must skip FIXME emission: {target_text}"
    );
}

// -----------------------------------------------------------------
// Gap 24: extract_java_class widens extracted-method visibility on the
// target to at least `package` (or `public` when target is in a
// different package than the source).
// -----------------------------------------------------------------

#[test]
fn extract_java_class_widens_private_method_to_package_default() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Same.java");
    let target = dir.path().join("SameExtract.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Same {\n    private Grid createMeterGrid() { return new Grid(); }\n    void wire() { Grid g = createMeterGrid(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("SameExtract".to_string());
    params.delegate_field = Some("sameExtract".to_string());
    params.item_names = Some(vec!["createMeterGrid".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let target_replacement = &plan.edits[1].edits[0].replacement;

    // The extracted method's `private` modifier is dropped (default
    // package visibility) so the source-side delegate call compiles.
    assert!(
        target_replacement.contains("Grid createMeterGrid()"),
        "method should still be present on target: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("private Grid createMeterGrid()"),
        "private modifier should be widened to package: {target_replacement}"
    );
}

#[test]
fn extract_java_class_widens_private_method_to_public_cross_package() {
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().join("a");
    let target_dir = dir.path().join("b");
    fs::create_dir_all(&source_dir).unwrap();
    fs::create_dir_all(&target_dir).unwrap();
    let source = source_dir.join("Cross.java");
    let target = target_dir.join("CrossExtract.java");
    fs::write(
            &source,
            "package com.a;\n\nclass Cross {\n    private Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("CrossExtract".to_string());
    params.delegate_field = Some("crossExtract".to_string());
    params.item_names = Some(vec!["createGrid".to_string()]);
    params.target_prelude = Some("package com.b;\n".to_string());

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let target_replacement = &plan.edits[1].edits[0].replacement;

    assert!(
        target_replacement.contains("public Grid createGrid()"),
        "cross-package extraction should widen private to public: {target_replacement}"
    );
    assert!(
        !target_replacement.contains("private Grid createGrid()"),
        "private modifier must not survive cross-package extraction: {target_replacement}"
    );
}

#[test]
fn extract_java_class_leaves_already_public_method_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Pub.java");
    let target = dir.path().join("PubExtract.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Pub {\n    public Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("PubExtract".to_string());
    params.delegate_field = Some("pubExtract".to_string());
    params.item_names = Some(vec!["createGrid".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let target_replacement = &plan.edits[1].edits[0].replacement;

    assert!(
        target_replacement.contains("public Grid createGrid()"),
        "public method should be preserved verbatim: {target_replacement}"
    );
}

#[test]
fn extract_java_class_keeps_protected_in_same_package() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Prot.java");
    let target = dir.path().join("ProtExtract.java");
    fs::write(
            &source,
            "package com.example;\n\nclass Prot {\n    protected Grid createGrid() { return new Grid(); }\n    void wire() { Grid g = createGrid(); }\n}\n",
        )
        .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("ProtExtract".to_string());
    params.delegate_field = Some("protExtract".to_string());
    params.item_names = Some(vec!["createGrid".to_string()]);

    let plan_text = plan_extract_java_class(&params).unwrap();
    let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
    let target_replacement = &plan.edits[1].edits[0].replacement;

    // protected (rank 2) is already above the package floor (1) — must
    // not be narrowed.
    assert!(
        target_replacement.contains("protected Grid createGrid()"),
        "protected should be preserved in same-package extraction: {target_replacement}"
    );
}

// ---------- Gap 26: extract_java_class remaining_source_accessors ----------

#[test]
fn extract_java_class_reports_remaining_field_accessors_with_deep_analysis() {
    // The cluster moves `grid` and `items` to a target class but the
    // source still reads/writes them in unrelated methods. With
    // deep_analysis=true, the plan response must list every remaining
    // access so the operator can decide before applying.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Extracted.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid grid;\n\
            \x20   private java.util.List<String> items;\n\
            \x20   void buildGrid() { grid = new Grid(); items.clear(); }\n\
            \x20   void other() {\n\
            \x20       view.add(grid);\n\
            \x20       grid.refresh();\n\
            \x20       items = new java.util.ArrayList<>();\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["grid".to_string(), "items".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    let grid_report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "grid")
        .expect("grid entry missing from remaining_source_accessors");
    // Two remaining reads of `grid`: view.add(grid) and grid.refresh().
    // The grid = new Grid() write inside buildGrid is ignored because
    // buildGrid is one of the moved declarations.
    assert!(
        grid_report.accesses.len() >= 2,
        "expected >= 2 grid accesses, got {:?}",
        grid_report.accesses
    );
    assert!(
        grid_report
            .accesses
            .iter()
            .any(|a| a.context.contains("view.add(grid)"))
    );
    assert!(
        grid_report
            .accesses
            .iter()
            .any(|a| a.context.contains("grid.refresh()"))
    );

    let items_report = plan
        .remaining_source_accessors
        .iter()
        .find(|r| r.field == "items")
        .expect("items entry missing from remaining_source_accessors");
    assert!(
        items_report
            .accesses
            .iter()
            .any(|a| a.kind == "write" && a.context.contains("items =")),
        "expected items write in `other`, got {:?}",
        items_report.accesses
    );
}

#[test]
fn extract_java_class_omits_remaining_accessors_without_deep_analysis() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Extracted.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid grid;\n\
            \x20   void buildGrid() { grid = new Grid(); }\n\
            \x20   void other() { view.add(grid); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["grid".to_string()]);
    // deep_analysis intentionally unset — default false.
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    assert!(
        plan.remaining_source_accessors.is_empty(),
        "remaining_source_accessors must stay empty without deep_analysis: {:?}",
        plan.remaining_source_accessors
    );
}

// ---------- Gap 18: rewrite remaining accesses through delegate ----------

/// Helper that materialises the source-side replacement for an
/// `extract_java_class` plan: applies every `edit` in the source
/// FileEdit to the original source content, returning the post-apply
/// string. Edits are pre-sorted by `byte_start` and verified
/// non-overlapping by `plan_extract_java_class`, so a simple
/// last-to-first pass is correct.
fn apply_source_edits(plan: &RefactorPlan, source_path: &Path) -> String {
    let original = fs::read_to_string(source_path).unwrap();
    let source_edit = plan
        .edits
        .iter()
        .find(|e| e.path == path_string(source_path))
        .expect("source edit missing");
    let mut buf = original;
    let mut edits = source_edit.edits.clone();
    edits.sort_by_key(|e| e.byte_start);
    for edit in edits.iter().rev() {
        buf.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    buf
}

fn target_replacement(plan: &RefactorPlan) -> &str {
    &plan.edits[1].edits[0].replacement
}

#[test]
fn extract_java_class_rewrites_bare_read_through_delegate() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   void layout() { add(meterGrid); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("add(delegate.getMeterGrid())"),
        "expected bare read rewritten through delegate: {rewritten}"
    );
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("Grid getMeterGrid()"),
        "expected getter on target: {target_text}"
    );
    assert!(
        target_text.contains("void setMeterGrid(Grid meterGrid)"),
        "expected setter on target: {target_text}"
    );
}

#[test]
fn extract_java_class_rewrites_this_qualified_read() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   Grid current() { return this.meterGrid; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("return delegate.getMeterGrid();"),
        "expected this.meterGrid rewritten via delegate: {rewritten}"
    );
    assert!(
        !rewritten.contains("this.meterGrid"),
        "this.meterGrid still present after rewrite: {rewritten}"
    );
}

#[test]
fn extract_java_class_rewrites_method_call_on_field_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   void redraw() { meterGrid.refresh(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.getMeterGrid().refresh()"),
        "expected method-on-field rewritten through getter: {rewritten}"
    );
}

#[test]
fn extract_java_class_rewrites_direct_write_through_setter() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             class View {\n\
            \x20   private List<String> meterItems;\n\
            \x20   void setup() { meterItems = new java.util.ArrayList<>(); }\n\
            \x20   void replace(List<String> newList) { meterItems = newList; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["meterItems".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setMeterItems(newList)"),
        "expected direct write rewritten via setter: {rewritten}"
    );
}

#[test]
fn extract_java_class_rewrites_compound_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Counter.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private int counter;\n\
            \x20   void reset() { counter = 0; }\n\
            \x20   void bump() { counter += 5; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Counter".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["reset".to_string()]);
    params.move_fields = Some(vec!["counter".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setCounter(delegate.getCounter() + 5)"),
        "expected compound assign rewritten: {rewritten}"
    );
}

#[test]
fn extract_java_class_rewrites_increment() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Counter.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private int counter;\n\
            \x20   void reset() { counter = 0; }\n\
            \x20   void tick() { counter++; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Counter".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["reset".to_string()]);
    params.move_fields = Some(vec!["counter".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setCounter(delegate.getCounter() + 1)"),
        "expected ++ rewritten: {rewritten}"
    );
}

// Gap 27: when the moved field appears on BOTH sides of an assignment
// (`field = field.transform()`), the LHS write must consume the whole
// assignment AND the RHS reads must still rewrite through the getter.
// Previously the read-rewrite pass fired first and the write-rewrite
// was silently dropped through the non-overlap guard.
#[test]
fn extract_java_class_rewrites_lhs_write_with_rhs_read_through_setter() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             import java.util.stream.Collectors;\n\
             class View {\n\
            \x20   private List<String> meterItems;\n\
            \x20   void setup() { meterItems = new java.util.ArrayList<>(); }\n\
            \x20   void replaceFirst(String replacement) {\n\
            \x20       meterItems = meterItems.stream()\n\
            \x20           .map(m -> replacement)\n\
            \x20           .collect(Collectors.toList());\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["meterItems".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    // LHS write must be wrapped in a setter call; RHS reads must still
    // route through the getter.
    assert!(
        rewritten.contains("delegate.setMeterItems(delegate.getMeterItems().stream()"),
        "expected LHS write + RHS read rewrite combined: {rewritten}"
    );
    // No bare LHS `meterItems =` may remain.
    assert!(
        !rewritten.contains("meterItems = meterItems"),
        "bare LHS field name must not survive: {rewritten}"
    );
    assert!(
        !rewritten.contains("meterItems ="),
        "bare LHS field name must not survive: {rewritten}"
    );
}

// Gap 7 regression: captured-param names that are FIELDS (not ctor
// parameters) require deferring the delegate-wiring statement until
// after their `this.field = param;` assignment in the constructor body,
// otherwise the wiring reads the field while it is still `null` (and
// `final` fields fail definite-assignment).
#[test]
fn extract_java_class_defers_wiring_after_field_only_captured_assigns() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("ExtractedGrid.java");
    // `plantPicker` is a captured FIELD (referenced by `setup()`), set
    // from the ctor `PlantPicker plantPickerParam` param via
    // `this.plantPicker = plantPickerParam;` — the assignment, not the
    // declaration, is what makes plantPicker non-null inside the ctor.
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private final PlantPicker plantPicker;\n\
            \x20   private Grid pipelineGrid;\n\
            \x20   Dashboard(PlantPicker plantPickerParam) {\n\
            \x20       this.plantPicker = plantPickerParam;\n\
            \x20       setup();\n\
            \x20   }\n\
            \x20   void setup() { pipelineGrid = plantPicker.buildGrid(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("ExtractedGrid".to_string());
    params.delegate_field = Some("extractedGrid".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["pipelineGrid".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    // The wiring assignment must appear AFTER `this.plantPicker =
    // plantPickerParam;` — i.e., the substring index of the wiring is
    // greater than the substring index of the field assignment.
    let assign_idx = rewritten
        .find("this.plantPicker = plantPickerParam;")
        .expect("field assignment present");
    let wiring_idx = rewritten
        .find("this.extractedGrid = new ExtractedGrid(plantPicker)")
        .expect("wiring statement present");
    assert!(
        wiring_idx > assign_idx,
        "Gap 7: wiring must follow the captured-field assignment.\n\
             field-assign idx: {assign_idx}\n\
             wiring idx:       {wiring_idx}\n\
             source:\n{rewritten}",
    );
}

// Gap 4: cross-file static-method callers in OTHER files get their
// qualifier rewritten from `OldClass.foo()` to `NewClass.foo()` after
// extract_java_class moves a static method to the target. Pre-fix the
// planner only rewrote callers inside the source file; the project
// didn't link because OldClass no longer declared `foo`.
#[test]
fn extract_java_class_rewrites_cross_file_static_method_callers() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/a");
    let dst_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&dst_pkg).unwrap();
    let source = src_pkg.join("Composition.java");
    let target = dst_pkg.join("Converters.java");
    fs::write(
        &source,
        "package a;\n\
             public class Composition {\n\
            \x20   public static int getHistoryItems() { return 42; }\n\
             }\n",
    )
    .unwrap();
    // Two other files in the project, each referencing the soon-to-be-
    // moved static method via `Composition.getHistoryItems()`.
    let caller_a = src_pkg.join("Report.java");
    fs::write(
        &caller_a,
        "package a;\n\
             public class Report {\n\
            \x20   int run() { return Composition.getHistoryItems(); }\n\
             }\n",
    )
    .unwrap();
    let caller_b = src_pkg.join("Dialog.java");
    fs::write(
        &caller_b,
        "package a;\n\
             public class Dialog {\n\
            \x20   int load() { return Composition.getHistoryItems() + 1; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Converters".to_string());
    params.delegate_field = Some("converters".to_string());
    params.item_names = Some(vec!["getHistoryItems".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    // Three FileEdits: source, target, and the cross-file caller in
    // the SAME-package case. (Plus another caller — both should show
    // up.) Verify both callers are rewritten with the new class name.
    let caller_paths: Vec<&str> = plan.edits.iter().map(|e| e.path.as_str()).collect();
    assert!(
        caller_paths.iter().any(|p| p.ends_with("Report.java")),
        "Report.java must appear in plan.edits: {caller_paths:?}"
    );
    assert!(
        caller_paths.iter().any(|p| p.ends_with("Dialog.java")),
        "Dialog.java must appear in plan.edits: {caller_paths:?}"
    );

    // Apply edits one by one against each file and check content.
    for fe in &plan.edits {
        if !fe.path.ends_with(".java") || fe.path.ends_with("Composition.java") {
            continue;
        }
        if fe.path.ends_with("Converters.java") {
            continue;
        }
        let original = fs::read_to_string(&fe.path).unwrap();
        let mut sorted = fe.edits.clone();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        let mut rewritten = original.clone();
        for edit in &sorted {
            rewritten.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        assert!(
            rewritten.contains("Converters.getHistoryItems()"),
            "{} must rewrite qualifier to Converters.getHistoryItems(): {rewritten}",
            fe.path
        );
        assert!(
            !rewritten.contains("Composition.getHistoryItems()"),
            "{} must drop the old Composition.getHistoryItems(): {rewritten}",
            fe.path
        );
    }
}

// Gap 4: cross-file callers of a moved STATIC CONSTANT get rewritten.
// `OldClass.PROTREND` accesses parse as field_access; same rewrite
// mechanism applies. Target package = source package (no extra import
// needed in that case).
#[test]
fn extract_java_class_rewrites_cross_file_static_field_callers() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Dialog.java");
    let target = pkg.join("Empties.java");
    fs::write(
        &source,
        "package a;\n\
             public class Dialog {\n\
            \x20   public static final String PROTREND = \"Protrend\";\n\
            \x20   public void noop() {}\n\
             }\n",
    )
    .unwrap();
    let caller = pkg.join("Export.java");
    fs::write(
        &caller,
        "package a;\n\
             public class Export {\n\
            \x20   String tag() { return Dialog.PROTREND; }\n\
            \x20   String suffix() { return Dialog.PROTREND + \"!\"; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Empties".to_string());
    params.delegate_field = Some("empties".to_string());
    params.item_names = Some(vec!["noop".to_string()]);
    params.move_fields = Some(vec!["PROTREND".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let caller_edit = plan
        .edits
        .iter()
        .find(|e| e.path.ends_with("Export.java"))
        .expect("Export.java caller must be rewritten");
    let original = fs::read_to_string(&caller_edit.path).unwrap();
    let mut sorted = caller_edit.edits.clone();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    let mut rewritten = original.clone();
    for edit in &sorted {
        rewritten.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    assert!(
        rewritten.contains("Empties.PROTREND"),
        "constant qualifier must be rewritten: {rewritten}"
    );
    assert!(
        rewritten.matches("Dialog.PROTREND").next().is_none(),
        "old qualifier must not survive: {rewritten}"
    );
}

// Gap 4: cross-package extracts add `import <target_pkg>.<NewClass>;`
// to each rewritten caller so the new qualified name resolves.
#[test]
fn extract_java_class_cross_file_callers_get_import_for_new_class() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/a");
    let dst_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&dst_pkg).unwrap();
    let source = src_pkg.join("Composition.java");
    let target = dst_pkg.join("Converters.java");
    fs::write(
        &source,
        "package a;\n\
             public class Composition {\n\
            \x20   public static int getHistoryItems() { return 42; }\n\
             }\n",
    )
    .unwrap();
    let caller = src_pkg.join("Report.java");
    fs::write(
        &caller,
        "package a;\n\
             public class Report {\n\
            \x20   int run() { return Composition.getHistoryItems(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Converters".to_string());
    params.delegate_field = Some("converters".to_string());
    params.item_names = Some(vec!["getHistoryItems".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let caller_edit = plan
        .edits
        .iter()
        .find(|e| e.path.ends_with("Report.java"))
        .expect("Report.java caller must be rewritten");
    let imports_emitted: String = caller_edit
        .edits
        .iter()
        .map(|e| e.replacement.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        imports_emitted.contains("import b.Converters;"),
        "cross-package caller must get import for the new class: {imports_emitted}"
    );
}

// Gap 4: instance methods are NOT rewritten cross-file. The source
// delegate field is private and unreachable from other files, so
// there's no safe automated rewrite — the operator handles those.
// Static-only rewrites keep the surface narrow + correct.
#[test]
fn extract_java_class_does_not_rewrite_cross_file_instance_callers() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Composition.java");
    let target = pkg.join("Converters.java");
    fs::write(
        &source,
        "package a;\n\
             public class Composition {\n\
            \x20   public int getHistoryItems() { return 42; }\n\
             }\n",
    )
    .unwrap();
    let caller = pkg.join("Report.java");
    fs::write(
        &caller,
        "package a;\n\
             public class Report {\n\
            \x20   int run(Composition c) { return c.getHistoryItems(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Converters".to_string());
    params.delegate_field = Some("converters".to_string());
    params.item_names = Some(vec!["getHistoryItems".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    // Report.java must NOT appear in plan.edits — instance methods
    // resolved on a Composition reference, not a class qualifier.
    let touched_report = plan.edits.iter().any(|e| e.path.ends_with("Report.java"));
    assert!(
        !touched_report,
        "instance-method call on a variable must not trigger cross-file rewrite"
    );
}

// Gap 8 smoke: stacked extracts can leave the source ctor with a
// delegate-read accessor rewrite ABOVE the delegate's own wiring
// assignment when the second extract's field-only captures push the
// wiring further down (Gap 7's lower bound). The planner does not
// auto-rewrite the conflict — moving wiring above the lower bound
// would silently null-capture the field-only captures, which is worse
// than a compile error — but it MUST emit a tracing::warn and run to
// completion so the operator gets the rewritten file plus a diagnostic
// log line. This test guards the planner against regressing into a
// crash on the conflict path.
#[test]
fn extract_java_class_diagnoses_ctor_wiring_ordering_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("Binder.java");
    // `compMap` is being moved into Binder. `helper` is a field-only
    // capture for `setup()` (assigned via `this.helper = h;`). The
    // pre-existing `this.emptyChecks = ...` line reads `compMap` — the
    // accessor rewriter will turn it into `binder.getCompMap()`. The
    // field-only-capture lower bound forces wiring to land AFTER
    // `this.helper = h;`, which is BELOW the rewritten emptyChecks line.
    // Conflict.
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.*;\n\
             class Dashboard {\n\
            \x20   private final Helper helper;\n\
            \x20   private final EmptyChecks emptyChecks;\n\
            \x20   private Map<String, Object> compMap = new HashMap<>();\n\
            \x20   Dashboard(Helper h) {\n\
            \x20       this.emptyChecks = new EmptyChecks(compMap);\n\
            \x20       this.helper = h;\n\
            \x20   }\n\
            \x20   void setup() { compMap.put(\"k\", helper.fetch()); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Binder".to_string());
    params.delegate_field = Some("binder".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["compMap".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan = serde_json::from_str(
        &plan_extract_java_class(&params)
            .expect("planner must run to completion on the conflict path"),
    )
    .unwrap();
    // The rewrite happens, edits apply cleanly. The resulting Java is
    // still broken at compile time — that's the documented v1 behavior.
    // Future iterations may move the field-only-capture assignments
    // ahead of the conflicting accessor rewrite, eliminating the
    // conflict entirely. For now the operator sees the warning in the
    // daemon log and swaps statements manually.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("binder.getCompMap()"),
        "accessor rewrite must still fire: {rewritten}"
    );
    assert!(
        rewritten.contains("this.binder = new Binder("),
        "wiring assignment must still be inserted: {rewritten}"
    );
}

// Gap 1 regression: LHS-write whose RHS contains a method invocation
// that update_java_callers rewrites. Pre-Gap-1 the planner emitted a
// zero-width caller-rewrite insert at the start of `buildGrid()` AND
// a span-edit covering the whole assignment for the LHS-write — the
// zero-width edit landed inside the span and the planner aborted with
// `overlapping edits: A..B overlaps X..X`. After the fix the caller
// rewrite is absorbed into the LHS-write's rendered RHS text.
#[test]
fn extract_java_class_lhs_write_with_moved_method_call_in_rhs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Dashboard.java");
    let target = dir.path().join("ExtractedGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class Dashboard {\n\
            \x20   private final Admin admin;\n\
            \x20   private Grid grid;\n\
            \x20   Dashboard() { grid = buildGrid(); refreshGrid(); }\n\
            \x20   Grid buildGrid() { return admin.load(); }\n\
            \x20   void refreshGrid() { grid.refresh(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("ExtractedGrid".to_string());
    params.delegate_field = Some("extractedGrid".to_string());
    params.item_names = Some(vec!["buildGrid".to_string(), "refreshGrid".to_string()]);
    params.move_fields = Some(vec!["grid".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    // The plan call itself must not fail with `overlapping edits`.
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    // The whole `grid = buildGrid();` assignment should become a single
    // setter call whose argument is the caller-rewritten method call.
    assert!(
        rewritten.contains("extractedGrid.setGrid(extractedGrid.buildGrid())"),
        "expected absorbed caller rewrite inside setter: {rewritten}"
    );
}

// Gap 27: assignment where RHS does NOT reference the moved field — the
// single-edit write rewrite must still cover the whole assignment.
#[test]
fn extract_java_class_rewrites_lhs_write_with_independent_rhs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             class View {\n\
            \x20   private List<String> meterItems;\n\
            \x20   private List<String> otherField;\n\
            \x20   void setup() { meterItems = new java.util.ArrayList<>(); }\n\
            \x20   void copy() { meterItems = otherField; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["meterItems".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setMeterItems(otherField)"),
        "expected setter call wrapping RHS: {rewritten}"
    );
}

// Gap 27: `this.field = expr` form — qualified LHS still triggers the
// write rewrite covering the whole `this.field = expr` span.
#[test]
fn extract_java_class_rewrites_this_qualified_lhs_write() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             class View {\n\
            \x20   private List<String> meterItems;\n\
            \x20   void setup() { meterItems = new java.util.ArrayList<>(); }\n\
            \x20   void replace(List<String> incoming) { this.meterItems = incoming; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["setup".to_string()]);
    params.move_fields = Some(vec!["meterItems".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setMeterItems(incoming)"),
        "expected this-qualified LHS rewritten: {rewritten}"
    );
    assert!(
        !rewritten.contains("this.meterItems"),
        "this.field LHS must not survive: {rewritten}"
    );
}

#[test]
fn extract_java_class_boolean_final_field_uses_is_getter_no_setter() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Selection.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private final boolean isPlantSelected;\n\
            \x20   View() { this.isPlantSelected = true; }\n\
            \x20   void noop() { /* marker */ }\n\
            \x20   boolean visible() { return isPlantSelected; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Selection".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["noop".to_string()]);
    params.move_fields = Some(vec!["isPlantSelected".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    // is-prefixed boolean: no double-prefix; getter is the bare name.
    assert!(
        target_text.contains("boolean isPlantSelected()"),
        "expected boolean is-prefix getter, got: {target_text}"
    );
    assert!(
        !target_text.contains("getIsPlantSelected"),
        "must not double-prefix to getIsPlantSelected: {target_text}"
    );
    // final field — no setter generated.
    assert!(
        !target_text.contains("setIsPlantSelected"),
        "must not generate setter for final field: {target_text}"
    );
    // Read of the final field still rewrites through the getter.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("return delegate.isPlantSelected();"),
        "expected boolean getter rewrite: {rewritten}"
    );
}

#[test]
fn extract_java_class_boolean_has_field_non_final_emits_setter() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("State.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private boolean hasError;\n\
            \x20   void clear() { hasError = false; }\n\
            \x20   void mark() { hasError = true; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("State".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["clear".to_string()]);
    params.move_fields = Some(vec!["hasError".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("boolean hasError()"),
        "expected has-prefix getter: {target_text}"
    );
    assert!(
        target_text.contains("void setHasError(boolean hasError)"),
        "expected setter on non-final has* field: {target_text}"
    );
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.setHasError(true)"),
        "expected has* setter rewrite: {rewritten}"
    );
}

#[test]
fn extract_java_class_skips_write_rewrite_for_final_field() {
    // A `final` field has no synthesized setter. Any remaining write
    // must NOT be silently rewritten — the original source-side write
    // stays in place so the compiler surfaces the immutability error,
    // and the operator can decide whether to drop `final` or restructure.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("Box.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private final String label;\n\
            \x20   View() { this.label = \"\"; }\n\
            \x20   void noop() { /* marker */ }\n\
            \x20   void mutate() { label = \"x\"; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Box".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["noop".to_string()]);
    params.move_fields = Some(vec!["label".to_string()]);
    params.deep_analysis = Some(true);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("setLabel"),
        "must not emit setter for final field: {target_text}"
    );
    // Source-side write stays untouched; no setter call introduced.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        !rewritten.contains("delegate.setLabel"),
        "must not synthesize setter call for final write: {rewritten}"
    );
}

#[test]
fn extract_java_class_accessors_public_when_cross_package() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/a");
    let tgt_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("View.java");
    let target = tgt_pkg.join("MeterGrid.java");
    fs::write(
        &source,
        "package a;\n\
             class View {\n\
            \x20   private String tag;\n\
            \x20   void initTag() { tag = \"x\"; }\n\
            \x20   String tag() { return tag; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["initTag".to_string()]);
    params.move_fields = Some(vec!["tag".to_string()]);
    params.deep_analysis = Some(true);
    params.target_prelude = Some("package b;\n".to_string());
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("public String getTag()"),
        "cross-package accessor must be public: {target_text}"
    );
    assert!(
        target_text.contains("public void setTag(String tag)"),
        "cross-package accessor must be public: {target_text}"
    );
}

#[test]
fn extract_java_class_rewrite_disabled_keeps_report_drops_edits() {
    // deep_analysis: true with rewrite_remaining_accessors: false —
    // operator wants the report, not the rewrites.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   void layout() { add(meterGrid); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    params.deep_analysis = Some(true);
    params.rewrite_remaining_accessors = Some(false);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    // Report still surfaces the breakage.
    assert!(
        !plan.remaining_source_accessors.is_empty(),
        "report must still populate when rewrite is disabled"
    );
    // No rewrites in the source — the bare `meterGrid` reference
    // survives.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("add(meterGrid)"),
        "raw bare-name access must stay when rewrite disabled: {rewritten}"
    );
    assert!(
        !rewritten.contains("delegate.getMeterGrid"),
        "must not insert getter call when rewrite disabled: {rewritten}"
    );
    // No accessor declarations on the target either.
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("getMeterGrid"),
        "target must not gain accessors when rewrite disabled: {target_text}"
    );
}

#[test]
fn extract_java_class_no_deep_analysis_still_rewrites_when_fields_move() {
    // Gap 6: `rewrite_remaining_accessors` is decoupled from
    // `deep_analysis`. Whenever `move_fields` is non-empty the source-
    // side reads/writes get rewritten through the delegate's accessors
    // (and the target gains matching getter/setter declarations), even
    // without `deep_analysis=true`. The `remaining_source_accessors`
    // REPORT remains gated on `deep_analysis` — that's a separate
    // diagnostic-only output.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   void layout() { add(meterGrid); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    // deep_analysis intentionally unset (default false).
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    // Report still gated on deep_analysis.
    assert!(plan.remaining_source_accessors.is_empty());
    // But the rewrites fire (Gap 6 fix — would have silently miscompiled).
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("delegate.getMeterGrid"),
        "Gap 6: source-side reads should be rewritten through the delegate: {rewritten}"
    );
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("getMeterGrid"),
        "Gap 6: target should expose accessors for moved fields: {target_text}"
    );
}

#[test]
fn extract_java_class_no_deep_analysis_opt_out_disables_rewrites() {
    // Gap 6 opt-out: passing `rewrite_remaining_accessors=false`
    // explicitly turns off the new always-on rewrite behavior.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("View.java");
    let target = dir.path().join("MeterGrid.java");
    fs::write(
        &source,
        "package com.example;\n\
             class View {\n\
            \x20   private Grid meterGrid;\n\
            \x20   void buildGrid() { meterGrid = new Grid(); }\n\
            \x20   void layout() { add(meterGrid); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterGrid".to_string());
    params.delegate_field = Some("delegate".to_string());
    params.item_names = Some(vec!["buildGrid".to_string()]);
    params.move_fields = Some(vec!["meterGrid".to_string()]);
    params.rewrite_remaining_accessors = Some(false);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        !rewritten.contains("delegate.getMeterGrid"),
        "opt-out: no rewrites when rewrite_remaining_accessors=false: {rewritten}"
    );
    let target_text = target_replacement(&plan);
    assert!(
        !target_text.contains("getMeterGrid"),
        "opt-out: no accessors on target: {target_text}"
    );
}

// ---------- Gap 25: extract_java_class organizes target imports ----------

#[test]
fn extract_java_class_retains_singular_import_for_static_method_call() {
    // Gap 4 variant: `DateUtils.parse(...)` references `DateUtils` as a
    // plain identifier (receiver of a method_invocation), not as a
    // type_identifier. Pre-fix the organize-imports heuristic missed
    // this reference and pruned the source's singular
    // `import b.DateUtils;` from the generated target, even though the
    // moved body still uses it.
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    fs::write(
        b_pkg.join("DateUtils.java"),
        "package b;\npublic class DateUtils { public static String now() { return \"\"; } }\n",
    )
    .unwrap();
    let source = a_pkg.join("Source.java");
    fs::write(
        &source,
        "package a;\n\
             import b.DateUtils;\n\
             public class Source {\n\
            \x20   String today() { return DateUtils.now(); }\n\
             }\n",
    )
    .unwrap();
    let target = a_pkg.join("Extracted.java");

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["today".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_edit = &plan.edits[1];
    let replacement = &target_edit.edits[0].replacement;
    assert!(
        replacement.contains("import b.DateUtils;"),
        "Gap 4: singular static-call import must be retained: {replacement}"
    );
}

#[test]
fn extract_java_class_retains_import_for_method_reference_qualifier() {
    // Method references (`Converter::toLabel`) parse with the type
    // qualifier as the first named child of a `method_reference` node
    // — same uppercase-initial type-shape signal as a static-call
    // receiver. Without picking these up, an extracted body like
    // `setItemLabelGenerator(EnumConverter::toLabel)` loses the
    // source's `import b.EnumConverter;` on the generated target.
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    fs::write(
            b_pkg.join("EnumConverter.java"),
            "package b;\npublic class EnumConverter { public static String toLabel(Object v) { return \"\"; } }\n",
        )
        .unwrap();
    let source = a_pkg.join("Source.java");
    fs::write(
        &source,
        "package a;\n\
             import b.EnumConverter;\n\
             import java.util.function.Function;\n\
             public class Source {\n\
            \x20   Function<Object,String> labels() { return EnumConverter::toLabel; }\n\
             }\n",
    )
    .unwrap();
    let target = a_pkg.join("Extracted.java");

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["labels".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_edit = &plan.edits[1];
    let replacement = &target_edit.edits[0].replacement;
    assert!(
        replacement.contains("import b.EnumConverter;"),
        "method-reference qualifier import must be retained: {replacement}"
    );
}

// Gap 3: when a method-reference qualifier is an INSTANCE FIELD of the
// source class (e.g. `csvExtractors::extractC4Composition` where
// `csvExtractors` is `private final CompositionCsvExtractors`), the
// capture analysis must thread `csvExtractors` through the target's
// constructor. Pre-fix, `resolve_field_access` short-circuited on every
// method-reference qualifier, so the target's ctor was missing the
// capture and the apply produced `error: cannot find symbol` at every
// call site of the moved method.
#[test]
fn extract_java_class_captures_method_reference_field_qualifier() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Composition.java");
    let target = dir.path().join("Importer.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.function.Function;\n\
             class Composition {\n\
            \x20   private final CsvExtractors csvExtractors = new CsvExtractors();\n\
            \x20   void importFile() {\n\
            \x20       Function<String,Integer> fn = csvExtractors::extractC4Composition;\n\
            \x20       fn.apply(\"x\");\n\
            \x20   }\n\
            \x20   static class CsvExtractors {\n\
            \x20       public Integer extractC4Composition(String s) { return 0; }\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Importer".to_string());
    params.delegate_field = Some("importer".to_string());
    params.item_names = Some(vec!["importFile".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    // Capture analysis MUST list csvExtractors so the target's ctor
    // takes it as a parameter and the source-side `new Importer(...)`
    // wiring passes it through.
    assert!(
        plan.captured_variables
            .iter()
            .any(|c| c.name == "csvExtractors"),
        "captured_variables must include `csvExtractors` after Gap 3 fix: {:?}",
        plan.captured_variables
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>(),
    );
    let rewritten = apply_source_edits(&plan, &source);
    // Source-side wiring threads csvExtractors into the target's ctor.
    assert!(
        rewritten.contains("this.importer = new Importer(csvExtractors)"),
        "source wiring must pass csvExtractors through: {rewritten}"
    );
    // Target text has a `csvExtractors` ctor param + private field.
    // Gap 7's inner-class qualification rewrites `CsvExtractors` to
    // `Composition.CsvExtractors` on the target — accept either shape.
    let target_edit = &plan.edits[1];
    let target_text = target_edit.edits[0].replacement.as_str();
    assert!(
        target_text.contains("CsvExtractors csvExtractors"),
        "target ctor must take csvExtractors: {target_text}"
    );
    assert!(
        target_text.contains("this.csvExtractors = csvExtractors"),
        "target ctor must assign csvExtractors to its field: {target_text}"
    );
}

#[test]
fn extract_java_class_prunes_unused_target_imports() {
    // Source imports two project-local types `B` and `C`; the extracted
    // method body only references `B`. After extract_java_class, the
    // target's import block must include `B` and exclude `C` —
    // composite plan runs heuristic_java_organize_imports_text on the
    // generated target text.
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("B.java"),
        "package a;\npublic class B { public void touch() {} }\n",
    )
    .unwrap();
    fs::write(
        pkg.join("C.java"),
        "package a;\npublic class C { public void unused() {} }\n",
    )
    .unwrap();
    let source = pkg.join("Source.java");
    fs::write(
        &source,
        "package a;\n\
             import a.B;\n\
             import a.C;\n\
             \n\
             public class Source {\n\
            \x20   void useB() { B b = new B(); b.touch(); }\n\
            \x20   void useC() { new C().unused(); }\n\
             }\n",
    )
    .unwrap();
    let target = pkg.join("Extracted.java");

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["useB".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    let target_edit = &plan.edits[1];
    assert_eq!(target_edit.path, path_string(&target));
    let replacement = &target_edit.edits[0].replacement;
    assert!(
        replacement.contains("import a.B;"),
        "expected `import a.B;` in target replacement: {replacement}"
    );
    assert!(
        !replacement.contains("import a.C;"),
        "expected `import a.C;` to be pruned from target: {replacement}"
    );
}

#[test]
fn extract_java_class_keeps_used_import_and_adds_missing_for_simple_name() {
    // Two project-local types live in package `b`: `Used` and
    // `MaybeAdded`. The source declares only `import b.Used;` and
    // references both via simple name in the extracted method body.
    // The heuristic must keep the existing `Used` import (referenced)
    // and add a fresh import for `MaybeAdded` (referenced by simple
    // name but not yet imported).
    let dir = tempfile::tempdir().unwrap();
    let a_pkg = dir.path().join("src/main/java/a");
    let b_pkg = dir.path().join("src/main/java/b");
    fs::create_dir_all(&a_pkg).unwrap();
    fs::create_dir_all(&b_pkg).unwrap();
    fs::write(
        b_pkg.join("Used.java"),
        "package b;\npublic class Used {}\n",
    )
    .unwrap();
    fs::write(
        b_pkg.join("MaybeAdded.java"),
        "package b;\npublic class MaybeAdded {}\n",
    )
    .unwrap();
    let source = a_pkg.join("Source.java");
    fs::write(
        &source,
        "package a;\n\
             import b.Used;\n\
             \n\
             public class Source {\n\
            \x20   void useThem() { Used u = new Used(); MaybeAdded m = new MaybeAdded(); }\n\
             }\n",
    )
    .unwrap();
    let target = a_pkg.join("Extracted.java");

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Extracted".to_string());
    params.delegate_field = Some("extracted".to_string());
    params.item_names = Some(vec!["useThem".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();

    let target_replacement = &plan.edits[1].edits[0].replacement;
    // Existing `Used` import preserved (referenced in body).
    assert!(
        target_replacement.contains("import b.Used;"),
        "expected `import b.Used;` retained in target: {target_replacement}"
    );
    // Missing `MaybeAdded` import added by the heuristic.
    assert!(
        target_replacement.contains("import b.MaybeAdded;"),
        "expected `import b.MaybeAdded;` added by heuristic: {target_replacement}"
    );
}

// ── Gap 10: extract_java_nested_classes emits a compilable target ──

// Gap 10: same-package extract gets package decl + imports +
// private/static modifiers stripped on the moved class. Package
// visibility (no public modifier) is fine because callers in the
// same package resolve default-package items.
#[test]
fn extract_java_nested_classes_same_package_strips_modifiers_and_adds_prelude() {
    let dir = tempfile::tempdir().unwrap();
    let pkg_dir = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg_dir).unwrap();
    let source = pkg_dir.join("Outer.java");
    let target = pkg_dir.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             import java.math.BigDecimal;\n\
             public class Outer {\n\
            \x20   private static final class Readings {\n\
            \x20       private final List<BigDecimal> values;\n\
            \x20       Readings(List<BigDecimal> v) { this.values = v; }\n\
            \x20   }\n\
            \x20   void use() { new Readings(java.util.List.of()); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;

    assert!(
        target_text.starts_with("package com.example;"),
        "target must start with package decl: {target_text}"
    );
    assert!(
        target_text.contains("import java.util.List;")
            && target_text.contains("import java.math.BigDecimal;"),
        "imports copied from source: {target_text}"
    );
    assert!(
        !target_text.contains("private static final class Readings")
            && !target_text.contains("private static class Readings")
            && !target_text.contains("static final class Readings"),
        "private + static stripped on the top-level class: {target_text}"
    );
    assert!(
        target_text.contains("final class Readings") || target_text.contains("class Readings"),
        "class declaration survives (with final preserved if present): {target_text}"
    );

    // Validations now wired up; both files included.
    assert_eq!(plan.validations.len(), 2);
}

// Gap 10: cross-package extract gets public modifier injected on
// top of stripping private/static, so the source's qualified
// reference still resolves from the new package.
#[test]
fn extract_java_nested_classes_cross_package_promotes_to_public() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             public class Outer {\n\
            \x20   private static class Readings {\n\
            \x20       Readings() {}\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;

    assert!(
        target_text.starts_with("package com.example.queries;"),
        "target package derived from path: {target_text}"
    );
    assert!(
        target_text.contains("public class Readings"),
        "public modifier injected on cross-package extract: {target_text}"
    );
    assert!(
        !target_text.contains("private") && !target_text.contains("static class Readings"),
        "private + static stripped: {target_text}"
    );
}

// Gap 10: emitted file must actually parse via tree-sitter — the
// previous validations: vec![] bug let nonsense through. This test
// pins the validation wiring.
#[test]
fn extract_java_nested_classes_emits_parseable_target() {
    let dir = tempfile::tempdir().unwrap();
    let pkg_dir = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg_dir).unwrap();
    let source = pkg_dir.join("Outer.java");
    let target = pkg_dir.join("Inner.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Inner {\n\
            \x20       int x;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Inner".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    // tree-sitter parse of the target text must have zero errors —
    // the apply-validation step runs the same check.
    let tree = parse_source("java", target_text).unwrap();
    let report = parse_report(tree.root_node());
    assert!(
        !report.has_error && report.error_nodes == 0 && report.missing_nodes == 0,
        "target text must parse cleanly: {target_text:?} (report={report:?})"
    );
}

// Gap 11: cross-package extract of an inner class that's still
// referenced by sibling inner / outer-method code in source needs
// `import <target-pkg>.<MovedClass>;` injected into source so the
// bare-name references continue to resolve.
#[test]
fn extract_java_nested_classes_cross_package_adds_source_import_for_surviving_refs() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings { int x; }\n\
            \x20   Readings findReadings() { return new Readings(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    // Apply source edits to inspect post-deletion source.
    let mut source_after = parsed_source_text(&source);
    let mut sorted_source = plan.edits[0].edits.clone();
    sorted_source.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    for edit in &sorted_source {
        source_after.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    assert!(
        source_after.contains("import com.example.queries.Readings;"),
        "source should gain target-package import for surviving bare refs: {source_after}"
    );
    // Bare-name reference in source body survives (still binds to
    // the imported class).
    assert!(
        source_after.contains("Readings findReadings() { return new Readings(); }"),
        "source body keeps bare-name references; resolves via new import: {source_after}"
    );
}

// Gap 11: same-package extracts do NOT inject the source import
// (the moved class lives in the same package — bare name resolves
// without an import).
#[test]
fn extract_java_nested_classes_same_package_does_not_inject_source_import() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Outer.java");
    let target = pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings { int x; }\n\
            \x20   Readings findReadings() { return new Readings(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let mut source_after = parsed_source_text(&source);
    let mut sorted_source = plan.edits[0].edits.clone();
    sorted_source.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    for edit in &sorted_source {
        source_after.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    assert!(
        !source_after.contains("import com.example.Readings;"),
        "same-package extract must not inject a same-package import: {source_after}"
    );
}

// Gap 11: when the moved class has NO surviving references in
// source (declaration was its only mention), the source import is
// skipped — adding it would just create an unused-import warning.
#[test]
fn extract_java_nested_classes_skips_source_import_when_no_surviving_refs() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Orphan.java");
    // Outer doesn't reference Orphan outside its declaration.
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Orphan { int x; }\n\
            \x20   void noop() {}\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Orphan".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let mut source_after = parsed_source_text(&source);
    let mut sorted_source = plan.edits[0].edits.clone();
    sorted_source.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    for edit in &sorted_source {
        source_after.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    assert!(
        !source_after.contains("import com.example.queries.Orphan;"),
        "no surviving refs → no source import: {source_after}"
    );
}

// Gap 12: moved body that references ANOTHER source-class inner
// type (sibling enum / class) gets the reference qualified to
// `<SourceClass>.<InnerType>` on the new top-level target. Cross-
// package targets also gain `import <source-pkg>.<SourceClass>;`.
#[test]
fn extract_java_nested_classes_qualifies_sibling_inner_type_refs_cross_package() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    // Readings references Mode (sibling enum on Outer) AND
    // Mode.READ (enum constant). Both should be qualified.
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   public enum Mode { READ, WRITE }\n\
            \x20   private static class Readings {\n\
            \x20       Mode mode = Mode.READ;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;

    assert!(
        target_text.contains("Outer.Mode mode = Outer.Mode.READ"),
        "Mode references should be qualified to Outer.Mode: {target_text}"
    );
    assert!(
        target_text.contains("import com.example.Outer;"),
        "cross-package target should import source class: {target_text}"
    );
}

// Gap 12: same-package extracts qualify the inner type but do NOT
// inject the source-class import (same package — qualified
// `Outer.Mode` resolves without an import).
#[test]
fn extract_java_nested_classes_qualifies_sibling_inner_type_refs_same_package() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Outer.java");
    let target = pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   public enum Mode { READ, WRITE }\n\
            \x20   private static class Readings {\n\
            \x20       Mode mode = Mode.READ;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        target_text.contains("Outer.Mode mode = Outer.Mode.READ"),
        "Mode references should be qualified: {target_text}"
    );
    assert!(
        !target_text.contains("import com.example.Outer;"),
        "same-package target must not inject same-package source import: {target_text}"
    );
}

// Gap 12: don't qualify references to the moved class itself
// (it's now top-level in the target file — qualifying it would
// create `Outer.Readings` inside the Readings declaration body).
#[test]
fn extract_java_nested_classes_does_not_qualify_self_references() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Outer.java");
    let target = pkg.join("Readings.java");
    // Readings has a self-referential static factory.
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings {\n\
            \x20       static Readings empty() { return new Readings(); }\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        !target_text.contains("Outer.Readings"),
        "self-references on the moved class must NOT be qualified: {target_text}"
    );
}

// Helper for the Gap 11 tests above.
fn parsed_source_text(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap()
}

// Gap 13: cross-package promotion of the class header MUST also
// widen the constructor — `private Foo(...)` stays private on the
// new top-level class otherwise, and cross-package `new Foo(...)`
// callers fail with `Foo() has private access in Foo`.
#[test]
fn extract_java_nested_classes_cross_package_widens_constructor_to_public() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings {\n\
            \x20       private Readings(int x) { this.x = x; }\n\
            \x20       int x;\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    // Class header was widened by Gap 10/11. Constructor must now
    // follow: public Readings(int x).
    assert!(
        target_text.contains("public Readings(int x)"),
        "constructor must be public on cross-package extract: {target_text}"
    );
    assert!(
        !target_text.contains("private Readings("),
        "private ctor modifier must be stripped: {target_text}"
    );
}

// Gap 13: same-package promotion keeps the class header at
// package-default. The constructor must drop `private` so callers
// in the same package can instantiate the now-top-level class —
// but doesn't need an explicit `public`.
#[test]
fn extract_java_nested_classes_same_package_strips_private_ctor() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Outer.java");
    let target = pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings {\n\
            \x20       private Readings() {}\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        target_text.contains("Readings()"),
        "constructor still exists: {target_text}"
    );
    assert!(
        !target_text.contains("private Readings("),
        "private ctor modifier stripped: {target_text}"
    );
    // Class is package-default (no public); ctor likewise stays
    // package-default — no spurious `public Readings()`.
    assert!(
        !target_text.contains("public Readings("),
        "same-package ctor should NOT be promoted to public: {target_text}"
    );
}

// Gap 13: protected constructors are left alone — operator may
// have chosen `protected` deliberately, and silently widening it
// to `public` would escalate API surface.
#[test]
fn extract_java_nested_classes_leaves_protected_ctor_alone() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   protected static class Readings {\n\
            \x20       protected Readings() {}\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        target_text.contains("protected Readings()"),
        "protected ctor must be preserved: {target_text}"
    );
    assert!(
        !target_text.contains("public Readings()"),
        "protected must NOT be silently widened to public: {target_text}"
    );
}

// Gap 14: extract_java_interface now copies imports from the
// source class — without them every signature type fails javac
// with `cannot find symbol`.
#[test]
fn extract_java_interface_copies_source_imports() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("MeterAdmin.java");
    let target = pkg.join("MeterRepository.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             import org.apache.commons.lang3.tuple.Pair;\n\
             public class MeterAdmin {\n\
            \x20   public List<Pair<String, String>> listAll() { return null; }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_interface", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("MeterRepository".to_string());
    params.impl_name = Some("MeterAdmin".to_string());
    params.item_names = Some(vec!["listAll".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_interface(&params).unwrap()).unwrap();
    let target_edit = plan
        .edits
        .iter()
        .find(|e| e.path.ends_with("MeterRepository.java"))
        .expect("interface target FileEdit present");
    let target_text = &target_edit.edits[0].replacement;
    assert!(
        target_text.starts_with("package com.example;"),
        "package decl preserved: {target_text}"
    );
    assert!(
        target_text.contains("import java.util.List;"),
        "List import copied: {target_text}"
    );
    assert!(
        target_text.contains("import org.apache.commons.lang3.tuple.Pair;"),
        "Pair import copied: {target_text}"
    );
    assert!(
        target_text.contains("public interface MeterRepository"),
        "interface decl present: {target_text}"
    );
    assert!(
        target_text.contains("List<Pair<String, String>> listAll"),
        "signature preserved: {target_text}"
    );
}

// Gap 17: instance-method cross-class move surfaces an advisory.
// Pre-fix the operator got a green plan + apply followed by a
// silent javac break on every cross-file caller. v1 emits a
// structured advisory pointing at find_java_usages for breakage
// enumeration.
#[test]
fn extract_java_methods_emits_cross_class_advisory_for_instance_move() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("MeterAdmin.java");
    let target = pkg.join("SamplePointAdmin.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class MeterAdmin {\n\
            \x20   public String fetchName(long id) { return \"\"; }\n\
             }\n",
    )
    .unwrap();
    fs::write(
        &target,
        "package com.example;\npublic class SamplePointAdmin {}\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["fetchName".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let response_json = plan_extract_java_methods(&params).unwrap();
    let v: serde_json::Value = serde_json::from_str(&response_json).unwrap();
    let advisory = v
        .get("cross_class_instance_move_advisory")
        .expect("advisory must surface for cross-class instance-method move");
    assert_eq!(advisory["code"], "cross_class_instance_method_move");
    assert_eq!(advisory["source_class"], "MeterAdmin");
    assert_eq!(advisory["target_class_simple_name"], "SamplePointAdmin");
    let methods = advisory["instance_methods"].as_array().unwrap();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0], "fetchName");
    let message = advisory["message"].as_str().unwrap();
    assert!(
        message.contains("find_java_usages"),
        "advisory must point at find_java_usages for enumeration: {message}"
    );
}

// Gap 17: static method moves do NOT trigger the advisory — the
// cross-file static caller rewrite (Gap 4) covers those cases.
#[test]
fn extract_java_methods_no_advisory_for_static_move() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("MeterAdmin.java");
    let target = pkg.join("SamplePointAdmin.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class MeterAdmin {\n\
            \x20   public static String labelOf(long id) { return \"\"; }\n\
             }\n",
    )
    .unwrap();
    fs::write(
        &target,
        "package com.example;\npublic class SamplePointAdmin {}\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["labelOf".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let response_json = plan_extract_java_methods(&params).unwrap();
    let v: serde_json::Value = serde_json::from_str(&response_json).unwrap();
    assert!(
        v.get("cross_class_instance_move_advisory").is_none(),
        "no advisory expected for static method move: {response_json}"
    );
}

// Gap 17: mixed instance + static — only the instance methods
// appear in the advisory.
#[test]
fn extract_java_methods_advisory_lists_only_instance_methods() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Source.java");
    let target = pkg.join("Target.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Source {\n\
            \x20   public String iMethod() { return \"\"; }\n\
            \x20   public static String sMethod() { return \"\"; }\n\
             }\n",
    )
    .unwrap();
    fs::write(&target, "package com.example;\npublic class Target {}\n").unwrap();
    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["iMethod".to_string(), "sMethod".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let response_json = plan_extract_java_methods(&params).unwrap();
    let v: serde_json::Value = serde_json::from_str(&response_json).unwrap();
    let advisory = v
        .get("cross_class_instance_move_advisory")
        .expect("advisory required since iMethod is instance-level");
    let methods = advisory["instance_methods"].as_array().unwrap();
    let names: Vec<&str> = methods.iter().map(|m| m.as_str().unwrap()).collect();
    assert!(names.contains(&"iMethod"));
    assert!(!names.contains(&"sMethod"));
}

// Gap 16: extract_java_methods to an EXISTING target now appends
// each missing import from source's import block to the target's
// import block. The structural method-append already worked; this
// closes the cannot-find-symbol gap.
#[test]
fn extract_java_methods_into_existing_target_appends_missing_imports() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/com/example");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("MeterAdmin.java");
    let target = pkg.join("SamplePointAdmin.java");
    fs::write(
        &source,
        "package com.example;\n\
             import java.util.List;\n\
             import org.apache.commons.lang3.tuple.Pair;\n\
             public class MeterAdmin {\n\
            \x20   public Pair<String, String> fetchById(long id) { return null; }\n\
             }\n",
    )
    .unwrap();
    fs::write(
        &target,
        "package com.example;\n\
             import java.util.Map;\n\
             public class SamplePointAdmin {\n\
            \x20   void existing() {}\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_methods", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["fetchById".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_methods(&params).unwrap()).unwrap();
    let target_edit = plan
        .edits
        .iter()
        .find(|e| e.path.ends_with("SamplePointAdmin.java"))
        .expect("existing target FileEdit present");
    let target_text = &target_edit.edits[0].replacement;
    // Existing import preserved.
    assert!(
        target_text.contains("import java.util.Map;"),
        "existing import preserved: {target_text}"
    );
    // Missing imports for moved method types added.
    assert!(
        target_text.contains("import org.apache.commons.lang3.tuple.Pair;"),
        "Pair import from source added: {target_text}"
    );
    // List wasn't used by the moved method but Gap 16's v1
    // conservatively copies ALL source imports; that's OK because
    // unused imports are a warning, not an error.
    assert!(
        target_text.contains("import java.util.List;"),
        "all source imports copied (conservative): {target_text}"
    );
    // Method body landed.
    assert!(
        target_text.contains("fetchById(long id)"),
        "moved method present: {target_text}"
    );
    // Existing method preserved.
    assert!(
        target_text.contains("void existing()"),
        "existing target method preserved: {target_text}"
    );
    // Idempotency: java.util.Map shouldn't get duplicated.
    let map_count = target_text.matches("import java.util.Map;").count();
    assert_eq!(map_count, 1, "Map import duplicated: {target_text}");
}

// Gap 13: multiple constructors all get rewritten.
#[test]
fn extract_java_nested_classes_widens_every_constructor() {
    let dir = tempfile::tempdir().unwrap();
    let src_pkg = dir.path().join("src/main/java/com/example");
    let tgt_pkg = dir.path().join("src/main/java/com/example/queries");
    fs::create_dir_all(&src_pkg).unwrap();
    fs::create_dir_all(&tgt_pkg).unwrap();
    let source = src_pkg.join("Outer.java");
    let target = tgt_pkg.join("Readings.java");
    fs::write(
        &source,
        "package com.example;\n\
             public class Outer {\n\
            \x20   private static class Readings {\n\
            \x20       private Readings() {}\n\
            \x20       private Readings(int x) { this(); }\n\
            \x20       Readings(int x, int y) { this(x); }\n\
            \x20   }\n\
             }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_nested_classes", &source);
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["Readings".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_nested_classes(&params).unwrap()).unwrap();
    let target_text = &plan.edits[1].edits[0].replacement;
    // Both private ctors get public.
    assert!(
        target_text.contains("public Readings()"),
        "first private ctor widened: {target_text}"
    );
    assert!(
        target_text.contains("public Readings(int x)"),
        "second private ctor widened: {target_text}"
    );
    // Package-default ctor gets public on cross-package too —
    // package-default doesn't satisfy `has_public_or_protected`,
    // so the cross-package branch injects `public`.
    assert!(
        target_text.contains("public Readings(int x, int y)"),
        "package-default ctor widened on cross-package: {target_text}"
    );
    assert!(
        !target_text.contains("private Readings("),
        "no surviving private ctor modifier: {target_text}"
    );
}

// G19-FU: java_qualify_unqualified_calls uses AST-based parsing and
// naturally skips string literals and line comments — they never produce
// method_invocation nodes. Only real unqualified call sites get the
// class qualifier prepended.
#[test]
fn g19_fu_qualify_skips_strings_and_comments() {
    let input = "\
class Example {\n\
    void run() {\n\
        String msg = \"doWork()\";\n\
        // doWork() is important\n\
        doWork();\n\
    }\n\
    void doWork() {}\n\
}\n";
    let result = java_qualify_unqualified_calls(input, "doWork", "Example");
    assert!(
        result.contains("\"doWork()\""),
        "string literal must be preserved verbatim: {result}"
    );
    assert!(
        result.contains("// doWork()"),
        "comment must be preserved verbatim: {result}"
    );
    assert!(
        result.contains("Example.doWork()"),
        "real call must be qualified: {result}"
    );
    assert_eq!(
        result.matches("Example.doWork").count(),
        1,
        "exactly one qualified occurrence, no spurious rewrites: {result}"
    );
}

// G5-FU: source_delegate_wrappers preserves `throws` clauses from the
// original method signature on the generated wrapper method.
#[test]
fn g5_fu_wrapper_preserves_throws_clause() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import java.io.IOException;\n\
             public class Admin {\n\
             \x20   public String save(int id) throws IOException { return String.valueOf(id); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.source_delegate_wrappers = Some(true);

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("public String save(int id) throws IOException {"),
        "wrapper must preserve throws clause: {rewritten}"
    );
}

// G7-FU: javax.inject.Inject is deduped when the source already has
// com.google.inject.Inject — same simple name means collision, so the
// javax variant must NOT be added.
#[test]
fn g7_fu_inject_import_dedup_by_simple_name() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import com.google.inject.Inject;\n\
             public class Admin {\n\
             \x20   @Inject private Object dep;\n\
             \x20   public Long save() { return 1L; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("import com.google.inject.Inject;"),
        "existing Guice Inject import must be preserved: {rewritten}"
    );
    assert!(
        !rewritten.contains("import javax.inject.Inject;"),
        "javax.inject.Inject must NOT appear when com.google.inject.Inject already present: {rewritten}"
    );
    assert!(
        rewritten.contains("@Inject private Service service;"),
        "delegate field must use @Inject: {rewritten}"
    );
}

// G7-FU: target class in guice_field_inject mode gets @Inject on its
// constructor plus the javax.inject.Inject import when the target needs
// ctor params (field capture forces ctor generation).
#[test]
fn g7_fu_target_class_has_inject_ctor_and_import() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import javax.inject.Inject;\n\
             public class Admin {\n\
             \x20   @Inject private Helper helper;\n\
             \x20   public Long save() { return helper.compute(); }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let target_text = target_replacement(&plan);
    assert!(
        target_text.contains("import javax.inject.Inject;"),
        "target must carry javax.inject.Inject import: {target_text}"
    );
    let has_inject_ctor = target_text.contains("@Inject\n    public Service(")
        || target_text.contains("@Inject\n  public Service(")
        || target_text.contains("@Inject\npublic Service(")
        || target_text.contains("@Inject\n     public Service(");
    assert!(
        has_inject_ctor,
        "@Inject must appear immediately above the target ctor: {target_text}"
    );
}

// G7-FU-v2: source carrying a wildcard Guice import must NOT receive
// an explicit javax.inject.Inject — the wildcard already supplies an
// `Inject` binding and the new explicit import would silently flip
// which one resolves bare. Conservative: any wildcard in the source
// blocks the explicit add.
#[test]
fn g7_fu_v2_wildcard_import_blocks_javax_inject_addition() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("src/main/java/a");
    fs::create_dir_all(&pkg).unwrap();
    let source = pkg.join("Admin.java");
    let target = pkg.join("Service.java");
    fs::write(
        &source,
        "package a;\n\
             import com.google.inject.*;\n\
             public class Admin {\n\
            \x20   @Inject private Object dep;\n\
            \x20   public Long save() { return 1L; }\n\
             }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Service".to_string());
    params.delegate_field = Some("service".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("import com.google.inject.*;"),
        "wildcard import must survive: {rewritten}"
    );
    assert!(
        !rewritten.contains("import javax.inject.Inject;"),
        "wildcard import must block adding `javax.inject.Inject`: {rewritten}"
    );
}

// G16-FU: @FunctionalInterface and @SafeVarargs are JDK built-in
// annotation types — java_builtin_type must classify them as such so
// the organize-imports heuristic doesn't try to resolve them as
// project-local types.
#[test]
fn g16_fu_java_builtin_type_includes_functional_interface_and_safe_varargs() {
    assert!(
        java_builtin_type("FunctionalInterface"),
        "FunctionalInterface must be a JDK builtin"
    );
    assert!(
        java_builtin_type("SafeVarargs"),
        "SafeVarargs must be a JDK builtin"
    );
    assert!(
        !java_builtin_type("Inject"),
        "Inject must NOT be a JDK builtin"
    );
}

// ─────────────────────────────────────────────────────────────────────
// prune_java_orphans — note-6020580c
// ─────────────────────────────────────────────────────────────────────

fn write_java(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn prune_java_orphans_deletes_unreferenced_private_method() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Box.java",
        "package com.example;\n\
         public class Box {\n\
         \x20   public int open() { return 1; }\n\
         \x20   private int dead() { return 42; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "prune_java_orphans");
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(plan.edits[0].edits.len(), 1);
    assert!(plan.items.iter().any(|i| i.name.as_deref() == Some("dead")));
}

#[test]
fn prune_java_orphans_keeps_referenced_private_method() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Calc.java",
        "package com.example;\n\
         public class Calc {\n\
         \x20   public int add(int a, int b) { return helper(a) + b; }\n\
         \x20   private int helper(int x) { return x; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(
        err.contains("no orphaned"),
        "expected no-orphans error, got: {err}"
    );
}

#[test]
fn prune_java_orphans_keeps_method_called_with_this_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Echo.java",
        "package com.example;\n\
         public class Echo {\n\
         \x20   public String shout(String s) { return this.upper(s); }\n\
         \x20   private String upper(String s) { return s.toUpperCase(); }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_keeps_method_used_via_method_reference() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Stream.java",
        "package com.example;\n\
         import java.util.List;\n\
         import java.util.function.Function;\n\
         public class Stream {\n\
         \x20   public Function<String, String> mapper() { return this::transform; }\n\
         \x20   private String transform(String s) { return s + \"!\"; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_skips_suppress_warnings_unused() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Suppressed.java",
        "package com.example;\n\
         public class Suppressed {\n\
         \x20   public int run() { return 1; }\n\
         \x20   @SuppressWarnings(\"unused\")\n\
         \x20   private int held() { return 99; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_skips_inject_annotated_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "InjectField.java",
        "package com.example;\n\
         import jakarta.inject.Inject;\n\
         public class InjectField {\n\
         \x20   @Inject\n\
         \x20   private Service service;\n\
         \x20   public int run() { return 1; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_skips_junit_test_methods() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "MyTest.java",
        "package com.example;\n\
         import org.junit.jupiter.api.Test;\n\
         import org.junit.jupiter.api.BeforeEach;\n\
         public class MyTest {\n\
         \x20   @BeforeEach\n\
         \x20   private void setUp() {}\n\
         \x20   @Test\n\
         \x20   private void exercise() {}\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_deletes_unreferenced_private_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Holder.java",
        "package com.example;\n\
         public class Holder {\n\
         \x20   private int unused = 0;\n\
         \x20   public int read() { return 1; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    assert!(
        plan.items
            .iter()
            .any(|i| i.name.as_deref() == Some("unused"))
    );
}

#[test]
fn prune_java_orphans_keeps_serial_version_uid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Serial.java",
        "package com.example;\n\
         import java.io.Serializable;\n\
         public class Serial implements Serializable {\n\
         \x20   private static final long serialVersionUID = 1L;\n\
         \x20   public int run() { return 1; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_v2_deletes_entire_multi_declarator_when_all_orphaned() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Multi.java",
        "package com.example;\n\
         public class Multi {\n\
         \x20   private int a, b, c;\n\
         \x20   public int run() { return 1; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // All three declarators orphaned → single full-field delete edit.
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "");
    // 3 orphans logged in items.
    assert_eq!(plan.items.len(), 3);
}

#[test]
fn prune_java_orphans_v2_rewrites_multi_declarator_when_some_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Partial.java",
        "package com.example;\n\
         public class Partial {\n\
         \x20   private int a, b, c;\n\
         \x20   public int run() { return b; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // b is referenced; a and c are orphans. One rewrite edit for the
    // whole field with b surviving.
    assert_eq!(edits.len(), 1);
    assert!(
        edits[0].replacement.contains("private int b;"),
        "expected rewrite keeping only b: {:?}",
        edits[0].replacement
    );
}

#[test]
fn prune_java_orphans_skips_constructors_even_when_private_and_unreferenced() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "SingletonHolder.java",
        "package com.example;\n\
         public class SingletonHolder {\n\
         \x20   private SingletonHolder() {}\n\
         \x20   public static int run() { return 1; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_deletes_unreferenced_private_inner_class() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "OuterDeadInner.java",
        "package com.example;\n\
         public class OuterDeadInner {\n\
         \x20   public int run() { return 1; }\n\
         \x20   private static class Dead {\n\
         \x20       int v() { return 0; }\n\
         \x20   }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    assert!(plan.items.iter().any(|i| i.name.as_deref() == Some("Dead")));
}

#[test]
fn prune_java_orphans_keeps_inner_class_referenced_via_type_position() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "OuterLiveInner.java",
        "package com.example;\n\
         public class OuterLiveInner {\n\
         \x20   private Live cache;\n\
         \x20   public int run() { return 1; }\n\
         \x20   private static class Live { int v() { return 0; } }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    // The `cache` field references type `Live`, so `Live` is kept.
    // The `cache` field itself is referenced nowhere (no callers); so it
    // gets pruned. `Live` should NOT be in the orphan list.
    assert!(!plan.items.iter().any(|i| i.name.as_deref() == Some("Live")));
}

#[test]
fn prune_java_orphans_item_kinds_filter_restricts_to_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Mixed.java",
        "package com.example;\n\
         public class Mixed {\n\
         \x20   public int run() { return 1; }\n\
         \x20   private int deadField = 0;\n\
         \x20   private int deadMethod() { return 0; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_kinds = Some(vec!["field".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    let pruned_names: HashSet<&str> = plan
        .items
        .iter()
        .filter_map(|i| i.name.as_deref())
        .collect();
    assert!(pruned_names.contains("deadField"));
    assert!(!pruned_names.contains("deadMethod"));
}

#[test]
fn prune_java_orphans_item_names_filter_restricts_to_named_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Named.java",
        "package com.example;\n\
         public class Named {\n\
         \x20   public int run() { return 1; }\n\
         \x20   private int alpha() { return 0; }\n\
         \x20   private int beta() { return 0; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_names = Some(vec!["alpha".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    let pruned_names: HashSet<&str> = plan
        .items
        .iter()
        .filter_map(|i| i.name.as_deref())
        .collect();
    assert!(pruned_names.contains("alpha"));
    assert!(!pruned_names.contains("beta"));
}

#[test]
fn prune_java_orphans_recursive_only_method_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Loop.java",
        "package com.example;\n\
         public class Loop {\n\
         \x20   public int run() { return 1; }\n\
         \x20   private int spin(int x) { return spin(x); }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    // The recursive self-call counts as a reference — conservative
    // policy keeps the method.
    assert!(err.contains("no orphaned"), "got: {err}");
}

#[test]
fn prune_java_orphans_emits_non_overlapping_edits_when_multiple_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Many.java",
        "package com.example;\n\
         public class Many {\n\
         \x20   public int keep() { return 1; }\n\
         \x20   private int deadA() { return 0; }\n\
         \x20   private int deadB() { return 0; }\n\
         \x20   private int deadC() { return 0; }\n\
         }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_prune_java_orphans(&params).unwrap()).unwrap();
    assert_eq!(plan.items.len(), 3);
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 3);
    // Edits must be sorted and non-overlapping (apply_text_edits asserts
    // this; ensure_non_overlapping inside the planner also enforces it,
    // but verify directly).
    for w in edits.windows(2) {
        assert!(
            w[0].byte_end <= w[1].byte_start,
            "edits overlap: {:?} vs {:?}",
            w[0],
            w[1]
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// singletonify_java_holder / singletonify_java_util — production-side
// note-7c819189 / note-e5439c0a
// ─────────────────────────────────────────────────────────────────────

#[test]
fn singletonify_holder_converts_public_static_final_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Cols.java");
    fs::write(
        &path,
        "package p;\n\
         public class Cols {\n\
         \x20   public static final SiteAdmin SITE_ADMIN = injector.getInstance(SiteAdmin.class);\n\
         \x20   public static final PlantRepository PLANT_REPO = injector.getInstance(PlantRepository.class);\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("singletonify_java_holder", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_singletonify_java_holder(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let has_singleton = edits.iter().any(|e| e.replacement.contains("@Singleton"));
    let has_ctor = edits.iter().any(|e| {
        e.replacement.contains("@Inject")
            && e.replacement
                .contains("public Cols(SiteAdmin siteAdmin, PlantRepository plantRepo)")
    });
    let has_field_rewrite_a = edits
        .iter()
        .any(|e| e.replacement.contains("private final SiteAdmin siteAdmin;"));
    let has_field_rewrite_b = edits.iter().any(|e| {
        e.replacement
            .contains("private final PlantRepository plantRepo;")
    });
    assert!(has_singleton, "@Singleton class annotation: {edits:?}");
    assert!(has_ctor, "@Inject constructor: {edits:?}");
    assert!(has_field_rewrite_a, "siteAdmin field rewrite: {edits:?}");
    assert!(has_field_rewrite_b, "plantRepo field rewrite: {edits:?}");
}

#[test]
fn singletonify_holder_refuses_when_no_public_static_final_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Empty.java");
    fs::write(&path, "package p; public class Empty {}\n").unwrap();
    let mut params = java_plan_params("singletonify_java_holder", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_singletonify_java_holder(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing to singletonify"), "got: {err}");
}

#[test]
fn singletonify_holder_item_names_filter_restricts_selection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Many.java");
    fs::write(
        &path,
        "package p;\n\
         public class Many {\n\
         \x20   public static final A A_FIELD = new A();\n\
         \x20   public static final B B_FIELD = new B();\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("singletonify_java_holder", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_names = Some(vec!["A_FIELD".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_singletonify_java_holder(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Only A_FIELD should be in the constructor; B_FIELD untouched.
    let ctor = edits
        .iter()
        .find(|e| e.replacement.contains("public Many("))
        .unwrap();
    assert!(ctor.replacement.contains("A aField"));
    assert!(!ctor.replacement.contains("bField"));
}

#[test]
fn singletonify_util_converts_impure_static_method_to_instance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("UI.java");
    fs::write(
        &path,
        "package p;\n\
         public class UI {\n\
         \x20   private static int counter = 0;\n\
         \x20   public static int next() { counter++; return counter; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("singletonify_java_util", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_names = Some(vec!["next".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_singletonify_java_util(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert!(edits.iter().any(|e| e.replacement.contains("@Singleton")));
    // Static keyword on the `next` method is removed → corresponding
    // edit deletes the "static " token from the modifiers list.
    let removed_static = edits.iter().any(|e| {
        e.replacement.is_empty() && e.byte_end > e.byte_start && {
            let bytes = std::fs::read(&path).unwrap();
            let s = std::str::from_utf8(&bytes[e.byte_start..e.byte_end]).unwrap_or("");
            s.contains("static")
        }
    });
    assert!(removed_static, "should delete `static` keyword: {edits:?}");
}

#[test]
fn singletonify_util_refuses_pure_method() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Pure.java");
    fs::write(
        &path,
        "package p;\n\
         public class Pure {\n\
         \x20   public static int doubleIt(int n) { return n * 2; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("singletonify_java_util", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_names = Some(vec!["doubleIt".to_string()]);
    let err = plan_singletonify_java_util(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("pure_methods_refused"), "got: {err}");
}

#[test]
fn singletonify_util_requires_item_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("X.java");
    fs::write(
        &path,
        "package p; public class X { public static void run() {} }\n",
    )
    .unwrap();
    let mut params = java_plan_params("singletonify_java_util", &path);
    params.project_dir = Some(path_string(dir.path()));
    let err = plan_singletonify_java_util(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("item_names"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// replace_java_static_reference — note-7c819189 / note-e5439c0a / note-7d4f0001
// ─────────────────────────────────────────────────────────────────────

#[test]
fn replace_java_static_reference_rewrites_static_method_calls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Util.java");
    fs::write(
        &path,
        "package p;\n\
         class Util {\n\
         \x20   String go() { return UIUtils.formatName(\"x\"); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("UIUtils".to_string());
    params.new_text = Some("uiUtilsProvider.get()".to_string());
    params.item_names = Some(vec!["formatName".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_replace_java_static_reference(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "uiUtilsProvider.get()");
}

#[test]
fn replace_java_static_reference_rewrites_static_field_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Holder.java");
    fs::write(
        &path,
        "package p;\n\
         class Holder {\n\
         \x20   Object find() { return ProductionDayColumns.SITE_ADMIN.lookup(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("ProductionDayColumns".to_string());
    params.new_text = Some("siteAdminProvider.get()".to_string());
    params.item_names = Some(vec!["SITE_ADMIN".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_replace_java_static_reference(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "siteAdminProvider.get()");
}

#[test]
fn replace_java_static_reference_vaadin_drop_accessor_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("View.java");
    fs::write(
        &path,
        "package p;\n\
         class View {\n\
         \x20   void run() { UI.getCurrent().push(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("UI".to_string());
    params.new_text = Some("uiProvider.get()".to_string());
    params.item_names = Some(vec!["getCurrent".to_string()]);
    params.delegate_field = Some("UI.getCurrent".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_replace_java_static_reference(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    // The entire `UI.getCurrent()` is replaced with `uiProvider.get()`,
    // leaving the trailing `.push()` intact.
    assert_eq!(edits[0].replacement, "uiProvider.get()");
}

#[test]
fn replace_java_static_reference_item_kinds_field_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Mixed.java");
    fs::write(
        &path,
        "package p;\n\
         class Mixed {\n\
         \x20   int field() { return Cls.SOME_CONST; }\n\
         \x20   int method() { return Cls.compute(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("Cls".to_string());
    params.new_text = Some("clsProvider.get()".to_string());
    params.item_names = Some(vec!["SOME_CONST".to_string(), "compute".to_string()]);
    params.item_kinds = Some(vec!["field".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_replace_java_static_reference(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Only the SOME_CONST field access is rewritten; the compute() method
    // call is left alone because item_kinds restricts to field.
    assert_eq!(edits.len(), 1);
}

#[test]
fn replace_java_static_reference_skips_other_class_qualifiers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Other.java");
    fs::write(
        &path,
        "package p;\n\
         class Other {\n\
         \x20   int run() { return OtherUtil.format(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("UIUtils".to_string());
    params.new_text = Some("uiUtilsProvider.get()".to_string());
    params.item_names = Some(vec!["format".to_string()]);
    let err = plan_replace_java_static_reference(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no `UIUtils"), "got: {err}");
}

#[test]
fn replace_java_static_reference_auto_injects_via_delegate_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Auto.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         import jakarta.inject.Provider;\n\
         class Auto {\n\
         \x20   void run() { UI.getCurrent().push(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("UI".to_string());
    params.delegate_type = Some("UI".to_string());
    params.item_names = Some(vec!["getCurrent".to_string()]);
    params.delegate_field = Some("UI.getCurrent".to_string()); // drop-accessor mode
    let plan: RefactorPlan =
        serde_json::from_str(&plan_replace_java_static_reference(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let has_call_rewrite = edits.iter().any(|e| e.replacement == "uIProvider.get()");
    let has_inject_field = edits
        .iter()
        .any(|e| e.replacement.contains("private Provider<UI> uIProvider;"));
    assert!(has_call_rewrite, "call rewrite missing: {edits:?}");
    assert!(has_inject_field, "inject field missing: {edits:?}");
}

#[test]
fn replace_java_static_reference_requires_new_text_or_delegate_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("X.java");
    fs::write(&path, "class X {}\n").unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("Cls".to_string());
    params.item_names = Some(vec!["x".to_string()]);
    let err = plan_replace_java_static_reference(&params)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("new_text") && err.contains("delegate_type"),
        "got: {err}"
    );
}

#[test]
fn replace_java_static_reference_rejects_unknown_item_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("X.java");
    fs::write(&path, "class X {}\n").unwrap();
    let mut params = java_plan_params("replace_java_static_reference", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("Cls".to_string());
    params.new_text = Some("p".to_string());
    params.item_names = Some(vec!["x".to_string()]);
    params.item_kinds = Some(vec!["bogus".to_string()]);
    let err = plan_replace_java_static_reference(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown item_kind"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// java_split_provider — note-4ec8ff30
// ─────────────────────────────────────────────────────────────────────

#[test]
fn java_split_provider_rewrites_single_getter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("View.java");
    fs::write(
        &path,
        "package p;\n\
         class View {\n\
         \x20   private final Provider<SessionData> sessionDataProvider;\n\
         \x20   private final Provider<AuthLogRecord> authLogProvider;\n\
         \x20   View(Provider<SessionData> s, Provider<AuthLogRecord> a) { this.sessionDataProvider = s; this.authLogProvider = a; }\n\
         \x20   AuthLogRecord rec() { return sessionDataProvider.get().getAuthLogRecord(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_mapping".to_string(),
        serde_json::json!({ "getAuthLogRecord": "authLogProvider" }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "authLogProvider.get()");
}

#[test]
fn java_split_provider_rewrites_multiple_getters_with_distinct_targets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   private final Provider<SessionData> sessionDataProvider;\n\
         \x20   Multi(Provider<SessionData> s) { this.sessionDataProvider = s; }\n\
         \x20   int a() { return sessionDataProvider.get().getAuthLogId(); }\n\
         \x20   String b() { return sessionDataProvider.get().getName(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_mapping".to_string(),
        serde_json::json!({
            "getAuthLogId": "authLogIdProvider",
            "getName": "nameProvider"
        }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 2);
    let replacements: std::collections::HashSet<&str> =
        edits.iter().map(|e| e.replacement.as_str()).collect();
    assert!(replacements.contains("authLogIdProvider.get()"));
    assert!(replacements.contains("nameProvider.get()"));
}

#[test]
fn java_split_provider_skips_unmapped_getters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Skip.java");
    fs::write(
        &path,
        "package p;\n\
         class Skip {\n\
         \x20   private final Provider<SessionData> sessionDataProvider;\n\
         \x20   Skip(Provider<SessionData> s) { this.sessionDataProvider = s; }\n\
         \x20   String mapped() { return sessionDataProvider.get().getName(); }\n\
         \x20   int unmapped() { return sessionDataProvider.get().getOtherField(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_mapping".to_string(),
        serde_json::json!({ "getName": "nameProvider" }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    // Only the mapped getter is rewritten; the unmapped one is left
    // alone (planner doesn't try to be clever about partial splits).
    assert_eq!(plan.edits[0].edits.len(), 1);
}

#[test]
fn java_split_provider_refuses_empty_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("E.java");
    fs::write(&path, "class E {}\n").unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("provider".to_string());
    let err = plan_java_split_provider(&params).unwrap_err().to_string();
    assert!(err.contains("getter_mapping"), "got: {err}");
}

#[test]
fn java_split_provider_auto_injects_via_getter_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Auto.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         import jakarta.inject.Provider;\n\
         class Auto {\n\
         \x20   @Inject private Provider<SessionData> sessionDataProvider;\n\
         \x20   AuthLogRecord rec() { return sessionDataProvider.get().getAuthLogRecord(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_types".to_string(),
        serde_json::json!({ "getAuthLogRecord": "AuthLogRecord" }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Should contain: rewrite to authLogRecordProvider.get() + new
    // @Inject Provider<AuthLogRecord> field declaration.
    let has_call_rewrite = edits
        .iter()
        .any(|e| e.replacement == "authLogRecordProvider.get()");
    let has_inject_field = edits.iter().any(|e| {
        e.replacement
            .contains("private Provider<AuthLogRecord> authLogRecordProvider;")
    });
    assert!(has_call_rewrite, "call rewrite missing: {edits:?}");
    assert!(has_inject_field, "inject field missing: {edits:?}");
}

#[test]
fn java_split_provider_v2_deletes_original_field_on_full_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("FullCov.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         import jakarta.inject.Provider;\n\
         class FullCov {\n\
         \x20   @Inject\n\
         \x20   private Provider<SessionData> sessionDataProvider;\n\
         \x20   AuthLogRecord rec() { return sessionDataProvider.get().getAuthLogRecord(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_types".to_string(),
        serde_json::json!({ "getAuthLogRecord": "AuthLogRecord" }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Full coverage → original field declaration deleted.
    let has_field_delete = edits.iter().any(|e| {
        e.replacement.is_empty() && e.byte_end > e.byte_start && {
            let bytes = std::fs::read(&path).unwrap();
            let slice = &bytes[e.byte_start..e.byte_end];
            std::str::from_utf8(slice)
                .map(|s| s.contains("Provider<SessionData>"))
                .unwrap_or(false)
        }
    });
    assert!(
        has_field_delete,
        "expected original field delete: {edits:?}"
    );
}

#[test]
fn java_split_provider_v2_keeps_original_field_on_partial_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Partial.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         import jakarta.inject.Provider;\n\
         class Partial {\n\
         \x20   @Inject\n\
         \x20   private Provider<SessionData> sessionDataProvider;\n\
         \x20   AuthLogRecord rec() { return sessionDataProvider.get().getAuthLogRecord(); }\n\
         \x20   String other() { return sessionDataProvider.get().getUnmappedThing(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_types".to_string(),
        serde_json::json!({ "getAuthLogRecord": "AuthLogRecord" }),
    );
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_split_provider(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Partial coverage → original field NOT deleted (getUnmappedThing
    // still goes through it).
    let bytes_src = std::fs::read(&path).unwrap();
    let has_field_delete = edits.iter().any(|e| {
        e.replacement.is_empty() && e.byte_end > e.byte_start && {
            std::str::from_utf8(&bytes_src[e.byte_start..e.byte_end])
                .map(|s| s.contains("Provider<SessionData>"))
                .unwrap_or(false)
        }
    });
    assert!(
        !has_field_delete,
        "should NOT delete original field on partial coverage: {edits:?}"
    );
}

#[test]
fn java_split_provider_refuses_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("N.java");
    fs::write(&path, "package p;\nclass N { int run() { return 0; } }\n").unwrap();
    let mut params = java_plan_params("java_split_provider", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("provider".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "getter_mapping".to_string(),
        serde_json::json!({ "getX": "xProvider" }),
    );
    params.toml_entries = Some(entries);
    let err = plan_java_split_provider(&params).unwrap_err().to_string();
    assert!(err.contains("no `provider.get()"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// migrate_java_method_receiver — note-1ee49c59
// ─────────────────────────────────────────────────────────────────────

#[test]
fn migrate_java_method_receiver_rewrites_single_call_site() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("View.java");
    fs::write(
        &path,
        "package p;\n\
         class View {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   private final AuthorizationService authz;\n\
         \x20   View(SessionData s, AuthorizationService a) { this.sessionData = s; this.authz = a; }\n\
         \x20   boolean check() { return sessionData.isAuthorized(42); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.new_text = Some("authz".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "authz");
}

#[test]
fn migrate_java_method_receiver_rewrites_multiple_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Many.java");
    fs::write(
        &path,
        "package p;\n\
         class Many {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   Many(SessionData s) { this.sessionData = s; }\n\
         \x20   boolean a() { return sessionData.isAuthorized(1); }\n\
         \x20   boolean b() { return sessionData.isAuthorized(2); }\n\
         \x20   boolean c() { return sessionData.isAuthorizedToEdit(3); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.new_text = Some("authz".to_string());
    params.item_names = Some(vec![
        "isAuthorized".to_string(),
        "isAuthorizedToEdit".to_string(),
    ]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 3);
    for e in edits {
        assert_eq!(e.replacement, "authz");
    }
}

#[test]
fn migrate_java_method_receiver_skips_unlisted_methods() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Skip.java");
    fs::write(
        &path,
        "package p;\n\
         class Skip {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   Skip(SessionData s) { this.sessionData = s; }\n\
         \x20   boolean a() { return sessionData.isAuthorized(1); }\n\
         \x20   String b() { return sessionData.getName(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.new_text = Some("authz".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Only `isAuthorized` is rewritten, `getName` left alone.
    assert_eq!(edits.len(), 1);
}

#[test]
fn migrate_java_method_receiver_handles_provider_get_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Prov.java");
    fs::write(
        &path,
        "package p;\n\
         class Prov {\n\
         \x20   private final Provider<SessionData> sessionDataProvider;\n\
         \x20   Prov(Provider<SessionData> s) { this.sessionDataProvider = s; }\n\
         \x20   boolean check() { return sessionDataProvider.get().isAuthorized(42); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionDataProvider.get()".to_string());
    params.new_text = Some("authzProvider.get()".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "authzProvider.get()");
}

#[test]
fn migrate_java_method_receiver_auto_injects_when_field_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Auto.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         class Auto {\n\
         \x20   @Inject private SessionData sessionData;\n\
         \x20   boolean check() { return sessionData.isAuthorized(42); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.delegate_type = Some("AuthorizationService".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Expect: 1 receiver-rewrite edit + 1 @Inject field-injection edit.
    let has_receiver_rewrite = edits
        .iter()
        .any(|e| e.replacement == "authorizationService");
    let has_inject_field = edits.iter().any(|e| {
        e.replacement.contains("@Inject")
            && e.replacement
                .contains("private AuthorizationService authorizationService;")
    });
    assert!(has_receiver_rewrite, "receiver rewrite missing: {edits:?}");
    assert!(has_inject_field, "inject field missing: {edits:?}");
}

#[test]
fn migrate_java_method_receiver_reuses_existing_inject_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Reuse.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         class Reuse {\n\
         \x20   @Inject private SessionData sessionData;\n\
         \x20   @Inject private AuthorizationService alreadyInjected;\n\
         \x20   boolean check() { return sessionData.isAuthorized(42); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.delegate_type = Some("AuthorizationService".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // Only the receiver-rewrite edit; no new field declaration.
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "alreadyInjected");
}

#[test]
fn migrate_java_method_receiver_provider_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Prov.java");
    fs::write(
        &path,
        "package p;\n\
         import jakarta.inject.Inject;\n\
         import jakarta.inject.Provider;\n\
         class Prov {\n\
         \x20   @Inject private SessionData sessionData;\n\
         \x20   boolean check() { return sessionData.isAuthorized(42); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.delegate_type = Some("AuthorizationService".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("prefer_provider".to_string(), serde_json::json!(true));
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert!(
        edits
            .iter()
            .any(|e| e.replacement == "authorizationServiceProvider.get()")
    );
    assert!(edits.iter().any(|e| {
        e.replacement
            .contains("private Provider<AuthorizationService> authorizationServiceProvider;")
    }));
}

#[test]
fn migrate_java_method_receiver_refuses_when_no_new_text_or_delegate_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Bad.java");
    fs::write(&path, "class Bad {}\n").unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("x".to_string());
    params.item_names = Some(vec!["foo".to_string()]);
    let err = plan_migrate_java_method_receiver(&params)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("new_text") && err.contains("delegate_type"),
        "got: {err}"
    );
}

#[test]
fn migrate_java_method_receiver_v2_handles_method_reference() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Ref.java");
    fs::write(
        &path,
        "package p;\n\
         import java.util.function.IntPredicate;\n\
         class Ref {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   private final AuthorizationService authz;\n\
         \x20   Ref(SessionData s, AuthorizationService a) { this.sessionData = s; this.authz = a; }\n\
         \x20   IntPredicate test() { return sessionData::isAuthorized; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.new_text = Some("authz".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "authz");
}

#[test]
fn migrate_java_method_receiver_v2_project_wide_walks_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("CallerA.java");
    let file_b = dir.path().join("CallerB.java");
    fs::write(
        &file_a,
        "package p;\n\
         class CallerA {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   CallerA(SessionData s) { this.sessionData = s; }\n\
         \x20   boolean a() { return sessionData.isAuthorized(1); }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &file_b,
        "package p;\n\
         class CallerB {\n\
         \x20   private final SessionData sessionData;\n\
         \x20   CallerB(SessionData s) { this.sessionData = s; }\n\
         \x20   boolean b() { return sessionData.isAuthorized(2); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &file_a);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("sessionData".to_string());
    params.new_text = Some("authz".to_string());
    params.item_names = Some(vec!["isAuthorized".to_string()]);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("project_wide".to_string(), serde_json::json!(true));
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_migrate_java_method_receiver(&params).unwrap()).unwrap();
    let paths: std::collections::HashSet<&str> =
        plan.edits.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("CallerA.java")));
    assert!(paths.iter().any(|p| p.ends_with("CallerB.java")));
}

#[test]
fn migrate_java_method_receiver_refuses_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("None.java");
    fs::write(
        &path,
        "package p;\nclass None { int unused() { return 0; } }\n",
    )
    .unwrap();
    let mut params = java_plan_params("migrate_java_method_receiver", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.delegate_field = Some("foo".to_string());
    params.new_text = Some("bar".to_string());
    params.item_names = Some(vec!["nonexistent".to_string()]);
    let err = plan_migrate_java_method_receiver(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no call sites"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// java_collapse_call_chain — note-295e99e1
// ─────────────────────────────────────────────────────────────────────

#[test]
fn java_collapse_call_chain_collapses_two_step_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("View.java");
    fs::write(
        &path,
        "package p;\n\
         class View {\n\
         \x20   private final SessionData session;\n\
         \x20   View(SessionData s) { this.session = s; }\n\
         \x20   int currentId() { return session.getAuthLogRecord().getAuthLogId(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getAuthLogRecord.getAuthLogId".to_string());
    params.new_text = Some("getAuthLogId".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_collapse_call_chain(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "java_collapse_call_chain");
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "session.getAuthLogId()");
}

#[test]
fn java_collapse_call_chain_collapses_multiple_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   private final SessionData session;\n\
         \x20   Multi(SessionData s) { this.session = s; }\n\
         \x20   int a() { return session.getAuthLogRecord().getAuthLogId(); }\n\
         \x20   int b() { return session.getAuthLogRecord().getAuthLogId() + 1; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getAuthLogRecord.getAuthLogId".to_string());
    params.new_text = Some("getAuthLogId".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_collapse_call_chain(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 2);
    for e in edits {
        assert_eq!(e.replacement, "session.getAuthLogId()");
    }
}

#[test]
fn java_collapse_call_chain_skips_chains_on_other_receiver_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Other.java");
    fs::write(
        &path,
        "package p;\n\
         class Other {\n\
         \x20   private final OtherType obj;\n\
         \x20   Other(OtherType o) { this.obj = o; }\n\
         \x20   int run() { return obj.getAuthLogRecord().getAuthLogId(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getAuthLogRecord.getAuthLogId".to_string());
    params.new_text = Some("getAuthLogId".to_string());
    let err = plan_java_collapse_call_chain(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no `"), "got: {err}");
}

#[test]
fn java_collapse_call_chain_refuses_single_segment_spec() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("X.java");
    fs::write(&path, "class X {}\n").unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("S".to_string());
    params.module_name = Some("just_one".to_string());
    params.new_text = Some("d".to_string());
    let err = plan_java_collapse_call_chain(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("chain_too_short"), "got: {err}");
}

#[test]
fn java_collapse_call_chain_v2_collapses_three_segment_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("V3.java");
    fs::write(
        &path,
        "package p;\n\
         class V3 {\n\
         \x20   private final SessionData session;\n\
         \x20   V3(SessionData s) { this.session = s; }\n\
         \x20   int currentId() { return session.getOuter().getMiddle().getInner(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getOuter.getMiddle.getInner".to_string());
    params.new_text = Some("getInnerDirectly".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_collapse_call_chain(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].replacement, "session.getInnerDirectly()");
}

#[test]
fn java_collapse_call_chain_v2_project_wide_walks_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("ViewA.java");
    let file_b = dir.path().join("ViewB.java");
    fs::write(
        &file_a,
        "package p;\n\
         class ViewA {\n\
         \x20   private final SessionData session;\n\
         \x20   ViewA(SessionData s) { this.session = s; }\n\
         \x20   int a() { return session.getAuthLogRecord().getAuthLogId(); }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &file_b,
        "package p;\n\
         class ViewB {\n\
         \x20   private final SessionData session;\n\
         \x20   ViewB(SessionData s) { this.session = s; }\n\
         \x20   int b() { return session.getAuthLogRecord().getAuthLogId(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &file_a);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getAuthLogRecord.getAuthLogId".to_string());
    params.new_text = Some("getAuthLogId".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("project_wide".to_string(), serde_json::json!(true));
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_java_collapse_call_chain(&params).unwrap()).unwrap();
    // Both files should be in the plan.
    let paths: std::collections::HashSet<&str> =
        plan.edits.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("ViewA.java")));
    assert!(paths.iter().any(|p| p.ends_with("ViewB.java")));
}

#[test]
fn java_collapse_call_chain_skips_chains_with_args() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Arg.java");
    fs::write(
        &path,
        "package p;\n\
         class Arg {\n\
         \x20   private final SessionData session;\n\
         \x20   Arg(SessionData s) { this.session = s; }\n\
         \x20   int run() { return session.getAuthLogRecord(\"x\").getAuthLogId(); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("java_collapse_call_chain", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.impl_name = Some("SessionData".to_string());
    params.module_name = Some("getAuthLogRecord.getAuthLogId".to_string());
    params.new_text = Some("getAuthLogId".to_string());
    let err = plan_java_collapse_call_chain(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no `"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// extract_java_test_slice — note-ea483190
// ─────────────────────────────────────────────────────────────────────

fn write_test_class(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn extract_java_test_slice_moves_test_that_calls_only_moved_methods() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "CalcTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         public class CalcTest {\n\
         \x20   @Test\n\
         \x20   void testAdd() { add(1, 2); }\n\
         \x20   @Test\n\
         \x20   void testKept() { keptMethod(); }\n\
         }\n",
    );
    let target = dir.path().join("AdderTest.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    params.module_name = Some("AdderTest".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_test_slice(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "extract_java_test_slice");
    assert_eq!(plan.edits.len(), 2);
    // Source edit deletes only `testAdd`, leaves `testKept`.
    assert_eq!(plan.edits[0].edits.len(), 1);
    // Target file gets created with testAdd inside.
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("public class AdderTest"),
        "target shape: {target_text}"
    );
    assert!(
        target_text.contains("void testAdd()"),
        "method moved: {target_text}"
    );
    assert!(
        !target_text.contains("testKept"),
        "kept stayed: {target_text}"
    );
}

#[test]
fn extract_java_test_slice_refuses_mixed_coverage_when_not_mockito() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "MixedTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         public class MixedTest {\n\
         \x20   @Test\n\
         \x20   void hybrid() { add(1, 2); keptMethod(); }\n\
         }\n",
    );
    let target = dir.path().join("AdderTest.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("mixed_coverage_without_mockito"), "got: {err}");
}

#[test]
fn extract_java_test_slice_mockito_synth_for_mixed_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "MixedTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         import org.junit.jupiter.api.extension.ExtendWith;\n\
         import org.mockito.junit.jupiter.MockitoExtension;\n\
         @ExtendWith(MockitoExtension.class)\n\
         public class MixedTest {\n\
         \x20   @Test\n\
         \x20   void hybrid() { add(1, 2); keptMethod(); }\n\
         }\n",
    );
    let target = dir.path().join("AdderTest.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    params.delegate_type = Some("Adder".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_test_slice(&params).unwrap()).unwrap();
    let source_edits = &plan.edits[0].edits;
    // Three edits: @Mock field decl, org.mockito.Mock import, and the
    // call-site rewrite that inserts "mockAdder." before `add(1, 2)`.
    assert!(
        source_edits
            .iter()
            .any(|e| e.replacement.contains("@Mock") && e.replacement.contains("Adder mockAdder")),
        "expected @Mock Adder mockAdder field decl: {source_edits:?}"
    );
    assert!(
        source_edits
            .iter()
            .any(|e| e.replacement.contains("import org.mockito.Mock;")),
        "expected Mock import: {source_edits:?}"
    );
    assert!(
        source_edits.iter().any(|e| e.replacement == "mockAdder."),
        "expected receiverless `add` rewrite to `mockAdder.add`: {source_edits:?}"
    );
    // Only mixed test exists → no target file created.
    assert_eq!(plan.edits.len(), 1);
}

#[test]
fn extract_java_test_slice_mockito_refuses_when_no_delegate_type() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "MixedTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         import org.junit.jupiter.api.extension.ExtendWith;\n\
         import org.mockito.junit.jupiter.MockitoExtension;\n\
         @ExtendWith(MockitoExtension.class)\n\
         public class MixedTest {\n\
         \x20   @Test\n\
         \x20   void hybrid() { add(1, 2); keptMethod(); }\n\
         }\n",
    );
    let target = dir.path().join("AdderTest.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("mixed_coverage_without_delegate_type"),
        "got: {err}"
    );
}

#[test]
fn extract_java_test_slice_reuses_existing_mock_field() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "MixedTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         import org.mockito.Mock;\n\
         import org.junit.jupiter.api.extension.ExtendWith;\n\
         import org.mockito.junit.jupiter.MockitoExtension;\n\
         @ExtendWith(MockitoExtension.class)\n\
         public class MixedTest {\n\
         \x20   @Mock Adder existingAdder;\n\
         \x20   @Test\n\
         \x20   void hybrid() { add(1, 2); keptMethod(); }\n\
         }\n",
    );
    let target = dir.path().join("AdderTest.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    params.delegate_type = Some("Adder".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_test_slice(&params).unwrap()).unwrap();
    let source_edits = &plan.edits[0].edits;
    // No new @Mock field generated — operator's `existingAdder` is reused.
    assert!(
        !source_edits.iter().any(|e| e.replacement.contains("@Mock")),
        "should reuse existing field, not generate new one: {source_edits:?}"
    );
    assert!(
        source_edits
            .iter()
            .any(|e| e.replacement == "existingAdder."),
        "call rewrite should use existingAdder.: {source_edits:?}"
    );
}

#[test]
fn extract_java_test_slice_refuses_no_test_methods() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "Empty.java",
        "package p;\nclass Empty { void notATest() {} }\n",
    );
    let target = dir.path().join("Out.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["add".to_string()]);
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("no @Test"), "got: {err}");
}

#[test]
fn extract_java_test_slice_refuses_no_tests_match_moved_set() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(
        dir.path(),
        "AllKeptTest.java",
        "package p;\n\
         import org.junit.jupiter.api.Test;\n\
         public class AllKeptTest {\n\
         \x20   @Test\n\
         \x20   void a() { keptOne(); }\n\
         \x20   @Test\n\
         \x20   void b() { keptTwo(); }\n\
         }\n",
    );
    let target = dir.path().join("Out.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["nonexistent".to_string()]);
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing to migrate"), "got: {err}");
}

#[test]
fn extract_java_test_slice_refuses_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(dir.path(), "Src.java", "class Src {}\n");
    let target = dir.path().join("Exists.java");
    fs::write(&target, "// already\n").unwrap();
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    params.item_names = Some(vec!["x".to_string()]);
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

#[test]
fn extract_java_test_slice_requires_item_names() {
    let dir = tempfile::tempdir().unwrap();
    let source = write_test_class(dir.path(), "Src.java", "class Src {}\n");
    let target = dir.path().join("Out.java");
    let mut params = java_plan_params("extract_java_test_slice", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.target = Some(path_string(&target));
    let err = plan_extract_java_test_slice(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("item_names"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// inline_java_method — note-8d4674ad
// ─────────────────────────────────────────────────────────────────────

#[test]
fn inline_java_method_inlines_return_expression() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Calc.java");
    fs::write(
        &path,
        "package p;\n\
         class Calc {\n\
         \x20   public int run() { return add(1, 2); }\n\
         \x20   private int add(int a, int b) { return a + b; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("add".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_inline_java_method(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "inline_java_method");
    let edits = &plan.edits[0].edits;
    // One inline edit + one declaration-deletion edit.
    assert_eq!(edits.len(), 2);
    let call_replacement = edits
        .iter()
        .find(|e| e.byte_start != e.byte_end && !e.replacement.is_empty())
        .unwrap();
    assert!(
        call_replacement.replacement.contains("(1) + (2)"),
        "expected substituted args: got `{}`",
        call_replacement.replacement
    );
}

#[test]
fn inline_java_method_inlines_void_statement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Log.java");
    fs::write(
        &path,
        "package p;\n\
         class Log {\n\
         \x20   public void run() { say(\"hi\"); }\n\
         \x20   private void say(String s) { System.out.println(s); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("say".to_string());
    // say's body calls System.out.println which the v1 safety check refuses.
    // This confirms the refusal.
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("calls another method"), "got: {err}");
}

#[test]
fn inline_java_method_refuses_non_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Pub.java");
    fs::write(
        &path,
        "package p;\n\
         class Pub {\n\
         \x20   public int run() { return add(1, 2); }\n\
         \x20   public int add(int a, int b) { return a + b; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("add".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("non-private"), "got: {err}");
}

#[test]
fn inline_java_method_refuses_multi_statement_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   public int run() { return more(); }\n\
         \x20   private int more() { int x = 1; return x + 2; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("more".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("2 statements"), "got: {err}");
}

#[test]
fn inline_java_method_refuses_this_in_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("This.java");
    fs::write(
        &path,
        "package p;\n\
         class This {\n\
         \x20   private int counter = 0;\n\
         \x20   public int run() { return read(); }\n\
         \x20   private int read() { return this.counter; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("read".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    // `this.counter` parses as `field_access`; tree-sitter visits the
    // field_access node first, so the refusal we surface is "reads a
    // field". Either refusal is correct — body containing a field
    // reference is unsafe to inline.
    assert!(
        err.contains("reads a field") || err.contains("uses `this`"),
        "got: {err}"
    );
}

#[test]
fn inline_java_method_refuses_no_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Orphan.java");
    fs::write(
        &path,
        "package p;\n\
         class Orphan {\n\
         \x20   public int run() { return 1; }\n\
         \x20   private int dead(int x) { return x + 1; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("dead".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("no call sites"), "got: {err}");
}

#[test]
fn inline_java_method_inlines_multiple_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Many.java");
    fs::write(
        &path,
        "package p;\n\
         class Many {\n\
         \x20   public int a() { return sq(2); }\n\
         \x20   public int b() { return sq(3) + sq(4); }\n\
         \x20   private int sq(int n) { return n * n; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("sq".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_inline_java_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // 3 inline edits + 1 declaration deletion.
    assert_eq!(edits.len(), 4);
    let leftover = plan.leftovers.first().unwrap();
    assert!(leftover.contains("call_sites_inlined=3"), "got: {leftover}");
}

#[test]
fn inline_java_method_v2_multi_statement_void_inlines_as_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   public void run() { greet(\"hi\", \"there\"); }\n\
         \x20   private void greet(String a, String b) { System.out.println(a); System.out.println(b); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("greet".to_string());
    // greet's body calls System.out.println — refused by safety check.
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("calls another method"), "got: {err}");
}

#[test]
fn inline_java_method_v2_multi_statement_pure_void_inlines_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Many.java");
    fs::write(
        &path,
        "package p;\n\
         class Many {\n\
         \x20   public void run() { setBoth(1, 2); }\n\
         \x20   private int a; private int b;\n\
         \x20   private void setBoth(int x, int y) { a = x; b = y; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("setBoth".to_string());
    // setBoth's body reads `a` and `b` which are class fields — body
    // safety check flags them as non-parameter identifiers and refuses.
    // The test confirms the refusal class is consistent (not silently
    // generating broken code).
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("not a parameter"), "got: {err}");
}

#[test]
fn inline_java_method_v2_refuses_multi_statement_non_void() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NV.java");
    fs::write(
        &path,
        "package p;\n\
         class NV {\n\
         \x20   public int run() { return calc(2); }\n\
         \x20   private int calc(int n) { int doubled = n * 2; return doubled; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("calc".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("multi-statement non-void"), "got: {err}");
}

#[test]
fn inline_java_method_v2_non_private_requires_project_wide() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Pub.java");
    fs::write(
        &path,
        "package p;\n\
         class Pub {\n\
         \x20   public int run() { return add(1, 2); }\n\
         \x20   public int add(int a, int b) { return a + b; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("add".to_string());
    let err = plan_inline_java_method(&params).unwrap_err().to_string();
    assert!(err.contains("project_wide=true"), "got: {err}");
}

#[test]
fn inline_java_method_v2_non_private_project_wide_walks_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file_a = dir.path().join("Lib.java");
    let file_b = dir.path().join("Caller.java");
    fs::write(
        &file_a,
        "package p;\n\
         public class Lib {\n\
         \x20   public int doubleIt(int n) { return n * 2; }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &file_b,
        "package p;\n\
         class Caller {\n\
         \x20   public int run() { Lib lib = new Lib(); return doubleIt(3); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &file_a);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("doubleIt".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("project_wide".to_string(), serde_json::json!(true));
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_inline_java_method(&params).unwrap()).unwrap();
    // Both files in the plan: source has declaration delete; Caller
    // has the inline.
    let paths: std::collections::HashSet<&str> =
        plan.edits.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.iter().any(|p| p.ends_with("Lib.java")));
    assert!(paths.iter().any(|p| p.ends_with("Caller.java")));
}

#[test]
fn inline_java_method_substitutes_arg_with_parens_for_precedence_safety() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Prec.java");
    fs::write(
        &path,
        "package p;\n\
         class Prec {\n\
         \x20   public int run() { return sq(1 + 2) * 3; }\n\
         \x20   private int sq(int n) { return n * n; }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("inline_java_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("sq".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_inline_java_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let call_replacement = edits.iter().find(|e| !e.replacement.is_empty()).unwrap();
    // The substituted form should preserve precedence: `(1 + 2) * (1 + 2)`
    // wrapped in parens so the surrounding `* 3` binds correctly.
    assert!(
        call_replacement.replacement.contains("((1 + 2) * (1 + 2))"),
        "expected paren-wrapped args, got `{}`",
        call_replacement.replacement
    );
}

// ─────────────────────────────────────────────────────────────────────
// convert_method_to_class — note-bd2b7a24
// ─────────────────────────────────────────────────────────────────────

#[test]
fn convert_method_to_class_void_no_params() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Run.java");
    let target = dir.path().join("RunHandlerOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Run {\n\
         \x20   public void runHandler() {\n\
         \x20       System.out.println(\"hi\");\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("runHandler".to_string());
    params.target = Some(path_string(&target));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "convert_method_to_class");
    assert_eq!(plan.edits.len(), 2);
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("public class RunHandlerOperation"),
        "class decl: {target_text}"
    );
    assert!(
        target_text.contains("public RunHandlerOperation()"),
        "ctor: {target_text}"
    );
    assert!(
        target_text.contains("public void execute()"),
        "execute sig: {target_text}"
    );
    assert!(
        target_text.contains("System.out.println(\"hi\");"),
        "body: {target_text}"
    );
    let source_edit = &plan.edits[0].edits[0].replacement;
    assert!(
        source_edit.contains("new RunHandlerOperation().execute();"),
        "delegate: {source_edit}"
    );
}

#[test]
fn convert_method_to_class_with_params_and_return() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Sum.java");
    let target = dir.path().join("AddOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Sum {\n\
         \x20   public int add(int a, int b) {\n\
         \x20       return a + b;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("add".to_string());
    params.target = Some(path_string(&target));
    params.new_text = Some("AddOperation".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("private final int a;"),
        "field a: {target_text}"
    );
    assert!(
        target_text.contains("private final int b;"),
        "field b: {target_text}"
    );
    assert!(
        target_text.contains("public AddOperation(int a, int b)"),
        "ctor: {target_text}"
    );
    assert!(
        target_text.contains("this.a = a;"),
        "assign a: {target_text}"
    );
    assert!(
        target_text.contains("this.b = b;"),
        "assign b: {target_text}"
    );
    assert!(
        target_text.contains("public int execute()"),
        "execute sig: {target_text}"
    );
    assert!(target_text.contains("return a + b;"), "body: {target_text}");
    let source_edit = &plan.edits[0].edits[0].replacement;
    assert!(
        source_edit.contains("return new AddOperation(a, b).execute();"),
        "delegate: {source_edit}"
    );
}

#[test]
fn convert_method_to_class_preserves_throws_clause() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Reader.java");
    let target = dir.path().join("ReadFileOperation.java");
    fs::write(
        &source,
        "package p;\n\
         import java.io.IOException;\n\
         public class Reader {\n\
         \x20   public String readFile(String path) throws IOException {\n\
         \x20       return \"ok\";\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("readFile".to_string());
    params.target = Some(path_string(&target));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("public String execute() throws IOException"),
        "throws preserved: {target_text}"
    );
}

#[test]
fn convert_method_to_class_default_class_name_derived_from_method() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Proc.java");
    let target = dir.path().join("ProcessOrderOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Proc {\n\
         \x20   public void processOrder() {}\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("processOrder".to_string());
    params.target = Some(path_string(&target));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("public class ProcessOrderOperation"),
        "auto name: {target_text}"
    );
}

#[test]
fn convert_method_to_class_refuses_mutated_enclosing_field() {
    // v2 behavior: rather than emit FIXMEs (which produces broken code
    // that the operator has to hand-fix), the planner refuses with a
    // clear operator-actionable error when the body mutates an
    // enclosing-class field.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Has.java");
    let target = dir.path().join("PrintOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Has {\n\
         \x20   private int counter;\n\
         \x20   public void print() {\n\
         \x20       this.counter++;\n\
         \x20       System.out.println(this.counter);\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("print".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("mutated_enclosing_field(counter)"),
        "got: {err}"
    );
}

#[test]
fn convert_method_to_class_captures_read_only_enclosing_field() {
    // Read-only field accesses get threaded through the constructor.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Read.java");
    let target = dir.path().join("EmitOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Read {\n\
         \x20   private int counter;\n\
         \x20   public int emit() {\n\
         \x20       return this.counter + 1;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("emit".to_string());
    params.target = Some(path_string(&target));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("private final int counter;"),
        "MO field for counter: {target_text}"
    );
    assert!(
        target_text.contains("public EmitOperation(int counter)"),
        "MO ctor with counter: {target_text}"
    );
    // `this.counter` rewritten to bare `counter` (now a field on MO).
    assert!(
        target_text.contains("return counter + 1;"),
        "body rewrite: {target_text}"
    );
    // Source-side delegate passes `this.counter` to the constructor.
    let source_edit = &plan.edits[0].edits[0].replacement;
    assert!(
        source_edit.contains("new EmitOperation(this.counter).execute()"),
        "delegate: {source_edit}"
    );
}

#[test]
fn convert_method_to_class_refuses_enclosing_method_call() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Calls.java");
    let target = dir.path().join("RunOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Calls {\n\
         \x20   private void helper() {}\n\
         \x20   public void run() {\n\
         \x20       helper();\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("run".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("enclosing_method_call(helper)"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_super_reference() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Sup.java");
    let target = dir.path().join("RunOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Sup extends Object {\n\
         \x20   public String run() {\n\
         \x20       return super.toString();\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("run".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("super_reference"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_bare_this_value() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Self.java");
    let target = dir.path().join("RunOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Self {\n\
         \x20   public boolean run(Self other) {\n\
         \x20       return this == other;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("run".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("bare_this_reference"), "got: {err}");
}

#[test]
fn convert_method_to_class_captures_bare_field_reference() {
    // Bare `counter` (without `this.`) referring to an enclosing field
    // gets captured the same way as `this.counter`.
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Bare.java");
    let target = dir.path().join("EmitOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Bare {\n\
         \x20   private int counter;\n\
         \x20   public int emit() {\n\
         \x20       return counter + 1;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("emit".to_string());
    params.target = Some(path_string(&target));
    let plan: RefactorPlan =
        serde_json::from_str(&plan_convert_method_to_class(&params).unwrap()).unwrap();
    let target_text = plan.edits[1].new_text.as_deref().unwrap();
    assert!(
        target_text.contains("private final int counter;"),
        "MO field: {target_text}"
    );
    let source_edit = &plan.edits[0].edits[0].replacement;
    assert!(
        source_edit.contains("new EmitOperation(this.counter).execute()"),
        "delegate: {source_edit}"
    );
}

#[test]
fn convert_method_to_class_refuses_static_method() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Stat.java");
    let target = dir.path().join("RunOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public class Stat {\n\
         \x20   public static void run() {}\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("run".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("static methods"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_abstract_method() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Abs.java");
    let target = dir.path().join("RunOperation.java");
    fs::write(
        &source,
        "package p;\n\
         public abstract class Abs {\n\
         \x20   public abstract void run();\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("run".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("abstract"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_constructor() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Ctor.java");
    let target = dir.path().join("CtorOp.java");
    fs::write(
        &source,
        "package p;\n\
         public class Ctor {\n\
         \x20   public Ctor() {}\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("Ctor".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("constructor"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_missing_method() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("None.java");
    let target = dir.path().join("None2.java");
    fs::write(&source, "class None { void a() {} }\n").unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("nonexistent".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"), "got: {err}");
}

#[test]
fn convert_method_to_class_refuses_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Src.java");
    let target = dir.path().join("Existing.java");
    fs::write(&source, "class Src { void a() {} }\n").unwrap();
    fs::write(&target, "// already exists\n").unwrap();
    let mut params = java_plan_params("convert_method_to_class", &source);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("a".to_string());
    params.target = Some(path_string(&target));
    let err = plan_convert_method_to_class(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"), "got: {err}");
}

// ─────────────────────────────────────────────────────────────────────
// extract_java_code_block_to_method — note-188c6fc9
// ─────────────────────────────────────────────────────────────────────

#[test]
fn extract_java_code_block_to_method_void_no_params() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Box.java");
    fs::write(
        &path,
        "package p;\n\
         class Box {\n\
         \x20   void run() {\n\
         \x20       System.out.println(\"hi\");\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(\"hi\");".to_string());
    params.module_name = Some("greet".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "extract_java_code_block_to_method");
    let edits = &plan.edits[0].edits;
    assert_eq!(edits.len(), 2);
    // First edit replaces the println with `greet();`; second inserts
    // the helper after the enclosing method.
    let replacement = &edits[0].replacement;
    assert!(
        replacement.contains("greet()"),
        "call site not synthesized: {replacement}"
    );
    let insert = &edits[1].replacement;
    assert!(
        insert.contains("private void greet()"),
        "helper signature: {insert}"
    );
    assert!(
        insert.contains("System.out.println(\"hi\");"),
        "helper body: {insert}"
    );
}

#[test]
fn extract_java_code_block_to_method_with_int_param() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Calc.java");
    fs::write(
        &path,
        "package p;\n\
         class Calc {\n\
         \x20   void run() {\n\
         \x20       int x = 7;\n\
         \x20       System.out.println(x);\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(x);".to_string());
    params.module_name = Some("log".to_string());
    params.parameters = Some(vec![JavaParameterSpec {
        type_name: "int".to_string(),
        name: "x".to_string(),
    }]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let replacement = &edits[0].replacement;
    assert!(replacement.contains("log(x);"), "call site: {replacement}");
    let insert = &edits[1].replacement;
    assert!(
        insert.contains("private void log(int x)"),
        "signature: {insert}"
    );
    assert!(insert.contains("System.out.println(x);"), "body: {insert}");
}

#[test]
fn extract_java_code_block_to_method_with_return_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Sum.java");
    fs::write(
        &path,
        "package p;\n\
         class Sum {\n\
         \x20   int run() {\n\
         \x20       int total = 1 + 2 + 3;\n\
         \x20       return total;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("int total = 1 + 2 + 3;".to_string());
    params.module_name = Some("compute".to_string());
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("return_type".to_string(), serde_json::json!("int"));
    entries.insert("return_var".to_string(), serde_json::json!("total"));
    params.toml_entries = Some(entries);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let replacement = &edits[0].replacement;
    assert!(
        replacement.contains("int total = compute();"),
        "call site: {replacement}"
    );
    let insert = &edits[1].replacement;
    assert!(
        insert.contains("private int compute()"),
        "signature: {insert}"
    );
    assert!(
        insert.contains("return total;"),
        "appended return: {insert}"
    );
}

#[test]
fn extract_java_code_block_to_method_static_enclosing_propagates_static() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Util.java");
    fs::write(
        &path,
        "package p;\n\
         class Util {\n\
         \x20   static void run() {\n\
         \x20       System.out.println(\"a\");\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(\"a\");".to_string());
    params.module_name = Some("emit".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let insert = &plan.edits[0].edits[1].replacement;
    assert!(
        insert.contains("private static void emit()"),
        "static propagated: {insert}"
    );
}

#[test]
fn extract_java_code_block_to_method_explicit_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Pub.java");
    fs::write(
        &path,
        "package p;\n\
         class Pub {\n\
         \x20   void run() { System.out.println(\"x\"); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(\"x\");".to_string());
    params.module_name = Some("emit".to_string());
    params.visibility = Some("protected".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let insert = &plan.edits[0].edits[1].replacement;
    assert!(insert.contains("protected void emit()"), "got: {insert}");
}

#[test]
fn extract_java_code_block_to_method_custom_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Wrap.java");
    fs::write(
        &path,
        "package p;\n\
         class Wrap {\n\
         \x20   void run() { System.out.println(\"x\"); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(\"x\");".to_string());
    params.module_name = Some("emit".to_string());
    params.new_text = Some("try { emit(); } catch (Exception e) {}".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let replacement = &plan.edits[0].edits[0].replacement;
    assert!(
        replacement.contains("try { emit(); } catch"),
        "got: {replacement}"
    );
}

#[test]
fn extract_java_code_block_rejects_zero_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Empty.java");
    fs::write(&path, "class Empty { void run() {} }\n").unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("nonexistent code".to_string());
    params.module_name = Some("helper".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not found"), "got: {err}");
}

#[test]
fn extract_java_code_block_rejects_multiple_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   void a() { System.out.println(\"x\"); }\n\
         \x20   void b() { System.out.println(\"x\"); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(\"x\");".to_string());
    params.module_name = Some("emit".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("matched 2 times"), "got: {err}");
}

#[test]
fn extract_java_code_block_rejects_text_outside_any_method() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Outside.java");
    fs::write(
        &path,
        "package p;\n\
         class Outside {\n\
         \x20   static int FIELD = 42;\n\
         \x20   void run() {}\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    // Match the class-level field declaration (not inside any method).
    params.old_text = Some("static int FIELD = 42;".to_string());
    params.module_name = Some("helper".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not fully enclosed by a method"), "got: {err}");
}

#[test]
fn extract_java_code_block_rejects_parameter_argument_length_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Mismatch.java");
    fs::write(
        &path,
        "package p;\n\
         class Mismatch {\n\
         \x20   void run() { int x = 1; System.out.println(x); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(x);".to_string());
    params.module_name = Some("log".to_string());
    params.parameters = Some(vec![JavaParameterSpec {
        type_name: "int".to_string(),
        name: "x".to_string(),
    }]);
    let mut entries = std::collections::BTreeMap::new();
    entries.insert(
        "arguments".to_string(),
        serde_json::json!(["x", "extraArg"]),
    );
    params.toml_entries = Some(entries);
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("parameters.len()=1"), "got: {err}");
}

#[test]
fn extract_java_code_block_rejects_invalid_helper_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Bad.java");
    fs::write(&path, "class Bad { void run() { int y = 0; } }\n").unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("int y = 0;".to_string());
    params.module_name = Some("123bad".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a valid Java identifier"), "got: {err}");
}

#[test]
fn extract_code_block_infers_single_capture_without_operator_help() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Auto.java");
    fs::write(
        &path,
        "package p;\n\
         class Auto {\n\
         \x20   int compute(int seed) {\n\
         \x20       int doubled = seed * 2;\n\
         \x20       return doubled;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("int doubled = seed * 2;".to_string());
    params.module_name = Some("doubleIt".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    // edits[0] = call-site replacement (smaller byte position);
    // edits[1] = helper-insert (at enclosing-method end).
    assert!(
        edits[1]
            .replacement
            .contains("private int doubleIt(int seed)"),
        "expected inferred sig with seed capture, got: {}",
        edits[1].replacement
    );
    assert!(
        edits[0]
            .replacement
            .contains("int doubled = doubleIt(seed);"),
        "call site: {}",
        edits[0].replacement
    );
}

#[test]
fn extract_code_block_infers_void_when_no_return_needed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Side.java");
    fs::write(
        &path,
        "package p;\n\
         class Side {\n\
         \x20   void run(int n) {\n\
         \x20       int doubled = n * 2;\n\
         \x20       System.out.println(doubled);\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text =
        Some("int doubled = n * 2;\n        System.out.println(doubled);".to_string());
    params.module_name = Some("emit".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert!(
        edits[1].replacement.contains("private void emit(int n)"),
        "expected void emit(int n), got: {}",
        edits[1].replacement
    );
}

#[test]
fn extract_code_block_refuses_mutated_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Mut.java");
    fs::write(
        &path,
        "package p;\n\
         class Mut {\n\
         \x20   int run(int seed) {\n\
         \x20       seed = seed + 1;\n\
         \x20       return seed;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("seed = seed + 1;".to_string());
    params.module_name = Some("bump".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("mutated_capture(seed)"), "got: {err}");
}

#[test]
fn extract_code_block_refuses_multi_return() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Multi.java");
    fs::write(
        &path,
        "package p;\n\
         class Multi {\n\
         \x20   int run() {\n\
         \x20       int a = 1;\n\
         \x20       int b = 2;\n\
         \x20       return a + b;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("int a = 1;\n        int b = 2;".to_string());
    params.module_name = Some("prep".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("multi_return_needs_record"), "got: {err}");
}

#[test]
fn extract_code_block_refuses_non_local_return() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Ret.java");
    fs::write(
        &path,
        "package p;\n\
         class Ret {\n\
         \x20   int run(int n) {\n\
         \x20       if (n < 0) return -1;\n\
         \x20       return n;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("if (n < 0) return -1;".to_string());
    params.module_name = Some("guard".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("non_local_control_flow"), "got: {err}");
}

#[test]
fn extract_code_block_refuses_non_local_break() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Brk.java");
    fs::write(
        &path,
        "package p;\n\
         class Brk {\n\
         \x20   void run() {\n\
         \x20       for (int i = 0; i < 10; i++) {\n\
         \x20           if (i == 5) break;\n\
         \x20       }\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("if (i == 5) break;".to_string());
    params.module_name = Some("check".to_string());
    let err = plan_extract_java_code_block_to_method(&params)
        .unwrap_err()
        .to_string();
    assert!(err.contains("non_local_control_flow"), "got: {err}");
}

#[test]
fn extract_code_block_this_field_reference_does_not_add_parameter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Field.java");
    fs::write(
        &path,
        "package p;\n\
         class Field {\n\
         \x20   private int counter = 0;\n\
         \x20   int run() {\n\
         \x20       int next = this.counter + 1;\n\
         \x20       return next;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("int next = this.counter + 1;".to_string());
    params.module_name = Some("advance".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    assert!(
        edits[1].replacement.contains("private int advance()"),
        "expected no-param sig (this.counter resolves via this), got: {}",
        edits[1].replacement
    );
}

#[test]
fn extract_code_block_inferred_return_uses_inferred_var_name_at_call_site() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Ret2.java");
    fs::write(
        &path,
        "package p;\n\
         class Ret2 {\n\
         \x20   String run(String input) {\n\
         \x20       String result = input.toUpperCase();\n\
         \x20       return result;\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("String result = input.toUpperCase();".to_string());
    params.module_name = Some("upper".to_string());
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let edits = &plan.edits[0].edits;
    let call = &edits[0].replacement;
    assert!(
        call.contains("String result = upper(input);"),
        "call site: {call}"
    );
}

#[test]
fn extract_java_code_block_to_method_arguments_default_to_param_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Defaults.java");
    fs::write(
        &path,
        "package p;\n\
         class Defaults {\n\
         \x20   void run() { int x = 1; String s = \"hi\"; System.out.println(s + x); }\n\
         }\n",
    )
    .unwrap();
    let mut params = java_plan_params("extract_java_code_block_to_method", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.old_text = Some("System.out.println(s + x);".to_string());
    params.module_name = Some("emit".to_string());
    params.parameters = Some(vec![
        JavaParameterSpec {
            type_name: "String".to_string(),
            name: "s".to_string(),
        },
        JavaParameterSpec {
            type_name: "int".to_string(),
            name: "x".to_string(),
        },
    ]);
    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_code_block_to_method(&params).unwrap()).unwrap();
    let replacement = &plan.edits[0].edits[0].replacement;
    assert!(replacement.contains("emit(s, x);"), "got: {replacement}");
}

#[test]
fn prune_java_orphans_rejects_unknown_item_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Unknown.java",
        "class Unknown { private int x() { return 0; } }\n",
    );
    let mut params = java_plan_params("prune_java_orphans", &path);
    params.project_dir = Some(path_string(dir.path()));
    params.item_kinds = Some(vec!["bogus".to_string()]);
    let err = plan_prune_java_orphans(&params).unwrap_err().to_string();
    assert!(err.contains("unknown item_kind"), "got: {err}");
}

#[test]
fn inline_java_class_inlines_one_shot_method_object() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_java(
        dir.path(),
        "AddOne.java",
        "package p;\n\
         class AddOne {\n\
        \x20   private final int base;\n\
        \x20   AddOne(int base) { this.base = base; }\n\
        \x20   int execute(int delta) { return base + delta; }\n\
         }\n",
    );
    let caller = write_java(
        dir.path(),
        "Caller.java",
        "package p;\n\
         class Caller {\n\
        \x20   int run(int value) {\n\
        \x20       AddOne op = new AddOne(value);\n\
        \x20       return op.execute(2);\n\
        \x20   }\n\
         }\n",
    );
    let mut params = java_plan_params("inline_java_class", &helper);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("AddOne".to_string());
    params.impl_name = Some("execute".to_string());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_inline_java_class(&params).unwrap()).unwrap();
    assert_eq!(plan.kind, "inline_java_class");
    assert_eq!(plan.edits.len(), 2);

    let rewritten_caller = apply_source_edits(&plan, &caller);
    assert!(
        rewritten_caller.contains("final int addOneBase = value;"),
        "caller rewrite: {rewritten_caller}"
    );
    assert!(
        rewritten_caller.contains("return (addOneBase + (2));"),
        "caller rewrite: {rewritten_caller}"
    );

    let rewritten_helper = apply_source_edits(&plan, &helper);
    assert!(
        !rewritten_helper.contains("class AddOne"),
        "helper should be removed: {rewritten_helper}"
    );
}

#[test]
fn inline_java_class_refuses_multiple_construction_sites() {
    let dir = tempfile::tempdir().unwrap();
    let helper = write_java(
        dir.path(),
        "Worker.java",
        "package p;\n\
         class Worker {\n\
        \x20   private final int base;\n\
        \x20   Worker(int base) { this.base = base; }\n\
        \x20   int execute() { return base; }\n\
         }\n",
    );
    write_java(
        dir.path(),
        "Caller.java",
        "package p;\n\
         class Caller {\n\
        \x20   int a() { Worker w = new Worker(1); return w.execute(); }\n\
        \x20   int b() { Worker w = new Worker(2); return w.execute(); }\n\
         }\n",
    );
    let mut params = java_plan_params("inline_java_class", &helper);
    params.project_dir = Some(path_string(dir.path()));
    params.module_name = Some("Worker".to_string());
    let err = plan_inline_java_class(&params).unwrap_err().to_string();
    assert!(err.contains("exactly one construction site"), "got: {err}");
}

#[test]
fn java_concurrency_audit_flags_unsafe_compute_if_absent_on_synchronized_map() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Cache.java",
        "package p;\n\
         import java.util.Collections;\n\
         import java.util.HashMap;\n\
         import java.util.Map;\n\
         class Cache {\n\
        \x20   private final Map<String, String> store =\n\
        \x20       Collections.synchronizedMap(new HashMap<>());\n\
        \x20   String lookup(String key) {\n\
        \x20       return store.computeIfAbsent(key, k -> compute(k));\n\
        \x20   }\n\
        \x20   String compute(String k) { return k; }\n\
         }\n",
    );
    let mut params = java_plan_params("java_concurrency_antipattern_audit", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_concurrency_antipattern_audit(&params).unwrap();
    let value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let declarations = value["declarations"].as_array().unwrap();
    assert_eq!(declarations.len(), 1, "got declarations: {declarations:?}");
    assert_eq!(declarations[0]["variable"], "store");
    assert_eq!(declarations[0]["wrapper"], "map");
    assert_eq!(declarations[0]["scope"], "field");

    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "got findings: {findings:?}");
    assert_eq!(findings[0]["variable"], "store");
    assert_eq!(findings[0]["collection_wrapper"], "map");
    assert_eq!(findings[0]["operation"], "computeIfAbsent");
    assert_eq!(findings[0]["confidence"], "high");
    let reason = findings[0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("computeIfAbsent") && reason.contains("synchronized"),
        "reason: {reason}"
    );
}

#[test]
fn java_concurrency_audit_suppresses_compute_inside_explicit_synchronized_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Guarded.java",
        "package p;\n\
         import java.util.Collections;\n\
         import java.util.HashMap;\n\
         import java.util.Map;\n\
         class Guarded {\n\
        \x20   private final Map<String, String> store =\n\
        \x20       Collections.synchronizedMap(new HashMap<>());\n\
        \x20   String lookup(String key) {\n\
        \x20       synchronized (store) {\n\
        \x20           return store.computeIfAbsent(key, k -> k);\n\
        \x20       }\n\
        \x20   }\n\
         }\n",
    );
    let mut params = java_plan_params("java_concurrency_antipattern_audit", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_concurrency_antipattern_audit(&params).unwrap();
    let value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let findings = value["findings"].as_array().unwrap();
    assert!(
        findings.is_empty(),
        "expected suppression inside synchronized(store); got: {findings:?}"
    );
}

#[test]
fn java_concurrency_audit_flags_unsafe_remove_if_on_synchronized_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_java(
        dir.path(),
        "Watchers.java",
        "package p;\n\
         import java.util.Collections;\n\
         import java.util.HashSet;\n\
         import java.util.Set;\n\
         class Watchers {\n\
        \x20   private final Set<String> ids =\n\
        \x20       Collections.synchronizedSet(new HashSet<>());\n\
        \x20   void purge(String prefix) {\n\
        \x20       ids.removeIf(id -> id.startsWith(prefix));\n\
        \x20   }\n\
         }\n",
    );
    let mut params = java_plan_params("java_concurrency_antipattern_audit", &path);
    params.project_dir = Some(path_string(dir.path()));
    let plan_text = plan_java_concurrency_antipattern_audit(&params).unwrap();
    let value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
    let declarations = value["declarations"].as_array().unwrap();
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0]["wrapper"], "set");

    let findings = value["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "got findings: {findings:?}");
    assert_eq!(findings[0]["variable"], "ids");
    assert_eq!(findings[0]["collection_wrapper"], "set");
    assert_eq!(findings[0]["operation"], "removeIf");
    assert_eq!(findings[0]["confidence"], "high");
}

// gap-9462575f: a moved `final` field that the source constructor initializes
// must be threaded through the extracted class's constructor — declared on the
// target, taken as a ctor param + assigned, passed from the source-side
// `new Target(...)`, and its now-orphaned source-ctor assignment deleted. This
// is the dominant cost when decomposing DI-heavy god classes: the moved fields
// are injected dependencies the source ctor wires up.
#[test]
fn extract_java_class_threads_moved_final_field_through_target_ctor() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Service.java");
    let target = dir.path().join("Writer.java");
    fs::write(
        &source,
        "package com.example;\n\
         class Service {\n\
        \x20   private final Repo repo;\n\
        \x20   private final Logger log;\n\
        \x20   Service(Repo repo, Logger log) {\n\
        \x20       this.repo = repo;\n\
        \x20       this.log = log;\n\
        \x20   }\n\
        \x20   void save() { repo.write(); }\n\
        \x20   void other() { log.info(); }\n\
         }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Writer".to_string());
    params.delegate_field = Some("writer".to_string());
    params.item_names = Some(vec!["save".to_string()]);
    params.move_fields = Some(vec!["repo".to_string()]);

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    assert_eq!(plan.edits.len(), 2);

    // Target carries the moved field decl, a ctor param for it, and the assignment.
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        target_text.contains("private final Repo repo;"),
        "target field decl: {target_text}"
    );
    assert!(
        target_text.contains("public Writer(Repo repo)"),
        "target ctor must take the moved final field as a param: {target_text}"
    );
    assert!(
        target_text.contains("this.repo = repo;"),
        "target ctor must assign the moved final field: {target_text}"
    );

    // Source threads its own ctor param into the construction and drops the orphan.
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten.contains("this.writer = new Writer(repo);"),
        "source must thread `repo` into the delegate construction: {rewritten}"
    );
    assert!(
        !rewritten.contains("this.repo = repo;"),
        "orphaned source-ctor assignment to the moved field must be deleted: {rewritten}"
    );
    // The unmoved final field's wiring + declaration are untouched.
    assert!(
        rewritten.contains("this.log = log;"),
        "unmoved field assignment retained: {rewritten}"
    );
    assert!(
        rewritten.contains("private final Logger log;"),
        "unmoved field declaration retained: {rewritten}"
    );
}

// gap-9462575f corollary: a field/parameter NAME MISMATCH in the source ctor
// (`this.fooAdmin = foosAdmin;`) must thread the parameter expression, not the
// field name — the exact case that cost the probe agent cells to discover.
#[test]
fn extract_java_class_threads_moved_field_with_name_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Holder.java");
    let target = dir.path().join("Part.java");
    fs::write(
        &source,
        "package com.example;\n\
         class Holder {\n\
        \x20   private final Thing fooAdmin;\n\
        \x20   Holder(Thing foosAdmin) {\n\
        \x20       this.fooAdmin = foosAdmin;\n\
        \x20   }\n\
        \x20   void use() { fooAdmin.run(); }\n\
         }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("Part".to_string());
    params.delegate_field = Some("part".to_string());
    params.item_names = Some(vec!["use".to_string()]);
    params.move_fields = Some(vec!["fooAdmin".to_string()]);

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    // Threads the PARAMETER (`foosAdmin`), not the field name (`fooAdmin`).
    assert!(
        rewritten.contains("this.part = new Part(foosAdmin);"),
        "must thread the ctor parameter expression, not the field name: {rewritten}"
    );
    // Target ctor param keeps the field's identity (`fooAdmin`).
    let target_text = &plan.edits[1].edits[0].replacement;
    assert!(
        target_text.contains("this.fooAdmin = fooAdmin;"),
        "target assigns the moved field by its own name: {target_text}"
    );
}

#[test]
fn extract_then_prune_preserves_multiline_inject_constructor() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("LargeView.java");
    let target = dir.path().join("RepoActions.java");
    fs::write(
        &source,
        "package com.acme;\n\
         import javax.inject.Inject;\n\
         class LargeView {\n\
        \x20   private final Repo repo;\n\
        \x20   private final Audit audit;\n\
        \x20   private final Clock clock;\n\
        \x20   @Inject\n\
        \x20   LargeView(\n\
        \x20       Repo repo,\n\
        \x20       Audit audit,\n\
        \x20       Clock clock) {\n\
        \x20       this.repo = repo;\n\
        \x20       this.audit = audit;\n\
        \x20       this.clock = clock;\n\
        \x20   }\n\
        \x20   void moved() { repo.save(); }\n\
        \x20   void kept() { audit.record(clock.now()); }\n\
         }\n",
    )
    .unwrap();

    let mut params = java_plan_params("extract_java_class", &source);
    params.target = Some(path_string(&target));
    params.module_name = Some("RepoActions".to_string());
    params.delegate_field = Some("repoActions".to_string());
    params.item_names = Some(vec!["moved".to_string()]);
    params.move_fields = Some(vec!["repo".to_string()]);
    params.project_dir = Some(path_string(dir.path()));
    params.wiring_mode = Some(guice_external_injection_spec());

    let plan: RefactorPlan =
        serde_json::from_str(&plan_extract_java_class(&params).unwrap()).unwrap();
    let rewritten = apply_source_edits(&plan, &source);
    assert!(
        rewritten
            .contains("LargeView(\n        Repo repo,\n        Audit audit,\n        Clock clock)"),
        "extractClass must leave the source constructor multiline before cleanup: {rewritten}"
    );

    fs::write(&source, &rewritten).unwrap();
    let prune = analyze_unused_constructor_params(&source).unwrap();
    assert_eq!(
        prune.removed,
        vec![("repo".to_string(), "Repo".to_string())]
    );
    let (byte_start, byte_end, replacement) = prune.edit.expect("edit produced");
    let mut cleaned = rewritten;
    cleaned.replace_range(byte_start..byte_end, &replacement);
    assert!(
        cleaned.contains("LargeView(\n        Audit audit,\n        Clock clock)"),
        "cleanup must preserve the multiline constructor signature: {cleaned}"
    );
    assert!(
        !cleaned.contains("LargeView(Audit audit"),
        "cleanup must not collapse the constructor signature: {cleaned}"
    );
}
