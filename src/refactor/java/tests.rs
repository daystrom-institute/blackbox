    use super::*;
    use std::fs;

    fn project_record(path: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: "test-project".to_string(),
            repo_id: None,
            canonical_path: fs::canonicalize(path)
                .unwrap()
                .display()
                .to_string(),
            registered_at: "2026-05-09T00:00:00Z".to_string(),
            is_git_repo: false,
            languages: Default::default(),
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
            boolean_getter_strategy: None,
            callback_externals: None,
            output_path: None,
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
        assert!(parsed.items.iter().any(|item| {
            item.kind == "method_declaration" && item.name.as_deref() == Some("run")
        }));
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
            plan.captured_variables
                .iter()
                .all(|c| c.name != "badgeId"),
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
        let plan =
            extract_dependency_plan(dir.path(), &source, &target, &["createSamplePointStatusBadge"]);
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
        let plan =
            extract_dependency_plan(dir.path(), &source, &target, &["createSamplePointStatusBadge"]);
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
            plan.external_calls
                .iter()
                .all(|c| c.method != "helper"),
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
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("private final Dep dep;"));

        let mut constructor = java_plan_params("add_java_constructor", &target);
        constructor.parameters = Some(vec![JavaParameterSpec {
            type_name: "Dep".to_string(),
            name: "dep".to_string(),
        }]);
        constructor.assign_to_fields = Some(true);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_add_java_constructor(&constructor).unwrap()).unwrap();
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("this.dep = dep;"));

        let mut callers = java_plan_params("update_java_callers", &source);
        callers.delegate_field = Some("target".to_string());
        callers.item_names = Some(vec!["refresh".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_update_java_callers(&callers).unwrap()).unwrap();
        assert_eq!(plan.edits[0].edits.len(), 2);
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "target."));

        let mut move_field = java_plan_params("move_java_field", &source);
        move_field.target = Some(path_string(&target));
        move_field.item_names = Some(vec!["grid".to_string()]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_move_java_field(&move_field).unwrap()).unwrap();
        assert_eq!(plan.edits.len(), 2);
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;"));

        let mut delegate = java_plan_params("add_java_delegate_field", &source);
        delegate.delegate_field = Some("target".to_string());
        delegate.delegate_type = Some("Target".to_string());
        delegate.parameters = Some(vec![JavaParameterSpec {
            type_name: "Dep".to_string(),
            name: "dep".to_string(),
        }]);
        let plan: RefactorPlan =
            serde_json::from_str(&plan_add_java_delegate_field(&delegate).unwrap()).unwrap();
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("private final Target target;")));
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement.contains("this.target = new Target(dep);")));
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
        assert!(plan
            .captured_variables
            .iter()
            .any(|capture| { capture.name == "admin" && capture.source_type == "Admin" }));
        assert!(plan.edits[0].edits.iter().any(|edit| edit
            .replacement
            .contains("private final ExtractedGrid extractedGrid;")));
        assert!(plan.edits[0].edits.iter().any(|edit| edit
            .replacement
            .contains("this.extractedGrid = new ExtractedGrid(admin);")));
        assert!(plan.edits[0]
            .edits
            .iter()
            .any(|edit| edit.replacement == "extractedGrid."));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("public class ExtractedGrid"));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private final Admin admin;"));
        assert!(plan.edits[1].edits[0]
            .replacement
            .contains("private Grid grid;"));
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
            after.lines().take(4).any(|l| l.trim_start() == "applyFilters();"),
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
        params.item_names = Some(vec![
            "createMeterGrid".to_string(),
            "getLogger".to_string(),
        ]);
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
        params.item_names = Some(vec![
            "createMeterGrid".to_string(),
            "getLogger".to_string(),
        ]);
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
            target_text.contains("// FIXME: target now implements HasLogger but does not satisfy method"),
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
            bytes.splice(
                edit.byte_start..edit.byte_end,
                edit.replacement.bytes(),
            );
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
            bytes.splice(
                edit.byte_start..edit.byte_end,
                edit.replacement.bytes(),
            );
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

        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert_eq!(plan.kind, "java_lsp_organize_imports");
        assert!(plan.edits[0].edits[0]
            .replacement
            .contains("import com.example.model.FooThing;"));
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
        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
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
        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
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
        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
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
        let plan_text =
            plan_java_lsp_organize_imports(&params, &PlanContext::default()).unwrap();
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
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_NOT_OK = \"OUT OF DATE\";"));
        assert!(target_text.contains(
            "private static final String SAMPLE_STATUS_NO_DATASOURCE = \"NONE ASSIGNED\";"
        ));
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
        assert!(target_text
            .contains("private static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
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
        assert!(source_edits.iter().all(|edit| {
            !(edit.byte_start <= other_pos && other_pos < edit.byte_end)
        }));
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
        assert!(target_text
            .contains("public static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"));
    }

    fn move_field_plan_for(
        source_text: &str,
        target_text: &str,
        field_names: &[&str],
    ) -> RefactorPlan {
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
        let writes = report
            .accesses
            .iter()
            .filter(|a| a.kind == "write")
            .count();
        let reads = report.accesses.iter().filter(|a| a.kind == "read").count();
        // 3 writes: `counter =`, `counter +=`, `counter++`.
        // 3 reads: rhs of `counter + 1`, log(counter), and (debatable) the
        // read embedded in `+=`. We only require classification of the LHS
        // positions reported as `write`, not the synthetic read of compound
        // assignment.
        assert!(writes >= 3, "expected >= 3 writes, got {writes} ({reads} reads)");
        assert!(reads >= 2, "expected >= 2 reads, got {reads} ({writes} writes)");
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
                assert!(err.to_string().contains("no Java import organization edits needed"));
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
        assert!(idx.top_level.get("Outer").is_some());
        // Top-level set must NOT include the inner names.
        assert!(idx.top_level.get("Inner").is_none());
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
            target_replacement
                .contains("static final String SAMPLE_STATUS_OK = \"UP TO DATE\";"),
            "target should keep static final + initializer: {target_replacement}"
        );
        assert!(
            !target_replacement
                .contains("private static final String SAMPLE_STATUS_OK"),
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

    fn promote_params(source: &std::path::Path, target: &std::path::Path, name: &str) -> RefactorPlanParams {
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
            target_replacement
                .contains("static final String LABEL = \"ok\";"),
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
        assert!(grid_report
            .accesses
            .iter()
            .any(|a| a.context.contains("view.add(grid)")));
        assert!(grid_report
            .accesses
            .iter()
            .any(|a| a.context.contains("grid.refresh()")));

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
            !rewritten.matches("Dialog.PROTREND").next().is_some(),
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
            plan.captured_variables.iter().map(|c| &c.name).collect::<Vec<_>>(),
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

    /// Apply the in-memory plan to the source text (single-file Phase 1
    /// helper) and return the rewritten source. Mirrors how
    /// `apply()` would write the file at byte offsets, sorted descending
    /// so earlier offsets stay valid.
    fn apply_plan_to_source(plan_text: &str, source_path: &Path) -> String {
        let plan: RefactorPlan = serde_json::from_str(plan_text).unwrap();
        assert_eq!(plan.edits.len(), 1, "Phase 1 emits one FileEdit");
        let mut text = fs::read_to_string(source_path).unwrap();
        let mut edits = plan.edits[0].edits.clone();
        edits.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
        for e in edits {
            text.replace_range(e.byte_start..e.byte_end, &e.replacement);
        }
        text
    }

    #[test]
    fn lombokify_full_coverage_emits_class_level_getter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Pair.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Pair<F, S> {\n\
            \x20   private F first;\n\
            \x20   private S second;\n\
            \n\
            \x20   public Pair(final F first, final S second) {\n\
            \x20       this.first = first;\n\
            \x20       this.second = second;\n\
            \x20   }\n\
            \n\
            \x20   public F getFirst() {\n\
            \x20       return first;\n\
            \x20   }\n\
            \n\
            \x20   public S getSecond() {\n\
            \x20       return second;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            rewritten.contains("import lombok.Getter;"),
            "expected lombok import:\n{rewritten}"
        );
        assert!(rewritten.contains("import lombok.AllArgsConstructor;"));
        // Phase 4 also picks up the canonical all-args constructor:
        // expect class-level @Getter and @AllArgsConstructor stacked.
        assert!(
            rewritten.contains("@Getter\n@AllArgsConstructor\npublic class Pair"),
            "expected stacked @Getter + @AllArgsConstructor:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("public F getFirst()"),
            "getFirst should be removed:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("public S getSecond()"),
            "getSecond should be removed:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("public Pair(final F first"),
            "canonical all-args ctor should be dropped:\n{rewritten}"
        );
        assert!(rewritten.contains("private F first;"));
    }

    #[test]
    fn lombokify_partial_coverage_emits_per_field_getter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Mixed.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Mixed {\n\
            \x20   private String name;\n\
            \x20   private int count;\n\
            \n\
            \x20   public String getName() {\n\
            \x20       return name;\n\
            \x20   }\n\
            \n\
            \x20   public int getCount() {\n\
            \x20       return count == 0 ? -1 : count;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(rewritten.contains("import lombok.Getter;"));
        // Per-field annotation on `name` only; non-trivial getCount kept.
        assert!(
            rewritten.contains("@Getter\n    private String name;"),
            "expected per-field @Getter on name:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("@Getter\npublic class"),
            "should NOT emit class-level @Getter when coverage is partial:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("public String getName()"),
            "getName removed:\n{rewritten}"
        );
        assert!(
            rewritten.contains("public int getCount()"),
            "non-trivial getCount preserved:\n{rewritten}"
        );
    }

    #[test]
    fn lombokify_skips_javadoc_bearing_getter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Doc.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Doc {\n\
            \x20   private String name;\n\
            \n\
            \x20   /** Returns the name. */\n\
            \x20   public String getName() {\n\
            \x20       return name;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("no lombokifiable boilerplate"),
            "expected refusal explaining no trivial getters; got: {err}"
        );
    }

    #[test]
    fn lombokify_boolean_field_accepts_is_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flag.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Flag {\n\
            \x20   private boolean active;\n\
            \n\
            \x20   public boolean isActive() {\n\
            \x20       return active;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(rewritten.contains("@Getter\npublic class Flag"));
        assert!(!rewritten.contains("public boolean isActive()"));
    }

    #[test]
    fn lombokify_skips_non_trivial_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Lazy.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Lazy {\n\
            \x20   private String name;\n\
            \n\
            \x20   public String getName() {\n\
            \x20       if (name == null) name = \"\";\n\
            \x20       return name;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("no lombokifiable boilerplate"),
            "expected refusal; got: {err}"
        );
    }

    #[test]
    fn lombokify_preserves_existing_lombok_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Pre.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import lombok.Getter;\n\
             \n\
             public class Pre {\n\
            \x20   private String name;\n\
            \n\
            \x20   public String getName() {\n\
            \x20       return this.name;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Exactly one Getter import line, not duplicated.
        assert_eq!(
            rewritten.matches("import lombok.Getter;").count(),
            1,
            "import should not be duplicated:\n{rewritten}"
        );
        assert!(rewritten.contains("@Getter\npublic class Pre"));
    }

    #[test]
    fn lombokify_full_coverage_emits_class_level_getter_and_setter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Pair.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Pair<F, S> {\n\
            \x20   private F first;\n\
            \x20   private S second;\n\
            \n\
            \x20   public Pair(final F first, final S second) {\n\
            \x20       this.first = first;\n\
            \x20       this.second = second;\n\
            \x20   }\n\
            \n\
            \x20   public void setFirst(final F first) {\n\
            \x20       this.first = first;\n\
            \x20   }\n\
            \n\
            \x20   public void setSecond(final S second) {\n\
            \x20       this.second = second;\n\
            \x20   }\n\
            \n\
            \x20   public F getFirst() {\n\
            \x20       return first;\n\
            \x20   }\n\
            \n\
            \x20   public S getSecond() {\n\
            \x20       return second;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(rewritten.contains("import lombok.Getter;"));
        assert!(rewritten.contains("import lombok.Setter;"));
        assert!(rewritten.contains("import lombok.AllArgsConstructor;"));
        assert!(
            rewritten.contains(
                "@Getter\n@Setter\n@AllArgsConstructor\npublic class Pair"
            ),
            "expected class-level @Getter + @Setter + @AllArgsConstructor:\n{rewritten}"
        );
        // All four accessors and the canonical ctor removed.
        assert!(!rewritten.contains("public F getFirst()"));
        assert!(!rewritten.contains("public S getSecond()"));
        assert!(!rewritten.contains("public void setFirst("));
        assert!(!rewritten.contains("public void setSecond("));
        assert!(
            !rewritten.contains("public Pair(final F first"),
            "canonical all-args ctor should be dropped:\n{rewritten}"
        );
    }

    #[test]
    fn lombokify_skips_setter_with_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Validated.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Validated {\n\
            \x20   private String name;\n\
            \n\
            \x20   public void setName(String name) {\n\
            \x20       if (name == null) throw new IllegalArgumentException();\n\
            \x20       this.name = name;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("no lombokifiable boilerplate"),
            "expected refusal; got: {err}"
        );
    }

    #[test]
    fn lombokify_skips_setter_with_fluent_return() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Fluent.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Fluent {\n\
            \x20   private String name;\n\
            \n\
            \x20   public Fluent setName(String name) {\n\
            \x20       this.name = name;\n\
            \x20       return this;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        // Fluent setter has non-void return AND multi-statement body — both
        // disqualify it. We expect refusal because there's nothing else to
        // lombokify in this class.
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("no lombokifiable boilerplate"),
            "expected refusal; got: {err}"
        );
    }

    #[test]
    fn lombokify_skips_setter_for_final_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Frozen.java");
        // Java accepts a setter for a final field at parse time only if the
        // field is non-final OR the field is initialized in the setter
        // (which would be reassignment of a final and won't compile). To
        // keep the test source parseable we use a non-final field with a
        // matching setter and a final field WITHOUT a setter, then verify
        // the planner only matches the non-final field.
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Frozen {\n\
            \x20   private final String id;\n\
            \x20   private String label;\n\
            \n\
            \x20   public Frozen(final String id) {\n\
            \x20       this.id = id;\n\
            \x20   }\n\
            \n\
            \x20   public void setLabel(final String label) {\n\
            \x20       this.label = label;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // class-level @Setter is correct — every NON-FINAL instance field
        // (label) has a deletable setter; final fields are excluded from
        // coverage by definition. Phase 4 also picks up the canonical
        // single-final-arg ctor → @RequiredArgsConstructor.
        assert!(
            rewritten.contains(
                "@Setter\n@RequiredArgsConstructor\npublic class Frozen"
            ),
            "expected stacked @Setter + @RequiredArgsConstructor:\n{rewritten}"
        );
        assert!(rewritten.contains("import lombok.Setter;"));
        assert!(rewritten.contains("import lombok.RequiredArgsConstructor;"));
        assert!(!rewritten.contains("public void setLabel("));
        assert!(!rewritten.contains("public Frozen(final String id)"));
    }

    #[test]
    fn lombokify_partial_setter_coverage_emits_per_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("MixedSetters.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class MixedSetters {\n\
            \x20   private String name;\n\
            \x20   private int count;\n\
            \n\
            \x20   public void setName(String name) {\n\
            \x20       this.name = name;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Per-field — only `name` has a setter; `count` doesn't, so
        // class-level @Setter would generate an unwanted setCount().
        assert!(
            rewritten.contains("@Setter\n    private String name;"),
            "expected per-field @Setter on name:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("@Setter\npublic class"),
            "should NOT emit class-level @Setter:\n{rewritten}"
        );
        assert!(!rewritten.contains("public void setName("));
    }

    #[test]
    fn lombokify_stacks_per_field_getter_and_setter() {
        // When neither @Getter nor @Setter qualifies for class-level
        // placement (e.g., one field has setter only, another has getter
        // only), each field gets its own annotation stack.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Asymmetric.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Asymmetric {\n\
            \x20   private String readOnly;\n\
            \x20   private String writeOnly;\n\
            \n\
            \x20   public String getReadOnly() {\n\
            \x20       return readOnly;\n\
            \x20   }\n\
            \n\
            \x20   public void setWriteOnly(String writeOnly) {\n\
            \x20       this.writeOnly = writeOnly;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(rewritten.contains("import lombok.Getter;"));
        assert!(rewritten.contains("import lombok.Setter;"));
        assert!(
            rewritten.contains("@Getter\n    private String readOnly;"),
            "expected per-field @Getter on readOnly:\n{rewritten}"
        );
        assert!(
            rewritten.contains("@Setter\n    private String writeOnly;"),
            "expected per-field @Setter on writeOnly:\n{rewritten}"
        );
        assert!(!rewritten.contains("public String getReadOnly()"));
        assert!(!rewritten.contains("public void setWriteOnly("));
    }

    #[test]
    fn lombokify_apache_equals_hashcode_tostring() {
        // Mirrors the planglobal idiom (Apache Commons Lang3 builders).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Input.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.apache.commons.lang3.builder.EqualsBuilder;\n\
             import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
             import org.apache.commons.lang3.builder.ToStringBuilder;\n\
             \n\
             public class Input {\n\
            \x20   private String triggeredAt;\n\
            \x20   private String triggeredBy;\n\
            \n\
            \x20   public String getTriggeredAt() { return triggeredAt; }\n\
            \x20   public String getTriggeredBy() { return triggeredBy; }\n\
            \n\
            \x20   @Override\n\
            \x20   public boolean equals(final Object other) {\n\
            \x20       if (this == other) return true;\n\
            \x20       if (other == null || getClass() != other.getClass()) return false;\n\
            \x20       final Input otherCasted = (Input) other;\n\
            \x20       return new EqualsBuilder()\n\
            \x20               .append(getTriggeredAt(), otherCasted.getTriggeredAt())\n\
            \x20               .append(getTriggeredBy(), otherCasted.getTriggeredBy())\n\
            \x20               .isEquals();\n\
            \x20   }\n\
            \n\
            \x20   @Override\n\
            \x20   public int hashCode() {\n\
            \x20       return new HashCodeBuilder(17, 37)\n\
            \x20               .append(getTriggeredAt())\n\
            \x20               .append(getTriggeredBy())\n\
            \x20               .toHashCode();\n\
            \x20   }\n\
            \n\
            \x20   @Override\n\
            \x20   public String toString() {\n\
            \x20       return new ToStringBuilder(this)\n\
            \x20               .append(\"triggeredAt\", triggeredAt)\n\
            \x20               .append(\"triggeredBy\", triggeredBy)\n\
            \x20               .toString();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Class-level @Getter (full coverage) + @EqualsAndHashCode + @ToString.
        assert!(
            rewritten.contains("@Getter\n@EqualsAndHashCode\n@ToString\npublic class Input"),
            "expected stacked class-level annotations:\n{rewritten}"
        );
        assert!(rewritten.contains("import lombok.Getter;"));
        assert!(rewritten.contains("import lombok.EqualsAndHashCode;"));
        assert!(rewritten.contains("import lombok.ToString;"));
        // All four method bodies removed.
        assert!(!rewritten.contains("public boolean equals("));
        assert!(!rewritten.contains("public int hashCode()"));
        assert!(!rewritten.contains("public String toString()"));
        assert!(!rewritten.contains("public String getTriggeredAt()"));
        // Apache imports still there (we don't touch them; user can run
        // organize_imports separately to drop unused).
        assert!(rewritten.contains("EqualsBuilder"));
    }

    #[test]
    fn lombokify_skips_equals_with_subset_of_fields() {
        // equals references only `name`, not `count` — Lombok @EqualsAndHashCode
        // would generate equality over BOTH fields, changing semantics.
        // Detector must refuse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Subset.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.apache.commons.lang3.builder.EqualsBuilder;\n\
             import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
             \n\
             public class Subset {\n\
            \x20   private String name;\n\
            \x20   private int count;\n\
            \n\
            \x20   public String getName() { return name; }\n\
            \n\
            \x20   public boolean equals(Object other) {\n\
            \x20       if (this == other) return true;\n\
            \x20       if (other == null || getClass() != other.getClass()) return false;\n\
            \x20       Subset that = (Subset) other;\n\
            \x20       return new EqualsBuilder().append(name, that.name).isEquals();\n\
            \x20   }\n\
            \n\
            \x20   public int hashCode() {\n\
            \x20       return new HashCodeBuilder().append(name).toHashCode();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Getter for `name` lombokified per-field; equals/hashCode preserved
        // because they only cover a subset of fields.
        assert!(
            rewritten.contains("@Getter\n    private String name;"),
            "expected per-field @Getter:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("@EqualsAndHashCode"),
            "should NOT lombokify subset-coverage equals/hashCode:\n{rewritten}"
        );
        assert!(rewritten.contains("public boolean equals(Object other)"));
        assert!(rewritten.contains("public int hashCode()"));
    }

    #[test]
    fn lombokify_keeps_unpaired_equals_or_hashcode() {
        // hashCode present, equals absent — @EqualsAndHashCode would generate
        // BOTH so dropping just hashCode would leave Lombok's auto-generated
        // equals colliding with nothing (fine) but breaking the user's
        // current behavior (where equals defaults to identity but is now
        // synthesized over fields). Detector refuses to touch it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Solo.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
             \n\
             public class Solo {\n\
            \x20   private String name;\n\
            \n\
            \x20   public int hashCode() {\n\
            \x20       return new HashCodeBuilder().append(name).toHashCode();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let result = plan_lombokify_java_class(&params);
        // No accessors and unpaired hashCode → bail.
        assert!(result.is_err(), "expected refusal; got: {result:?}");
    }

    #[test]
    fn lombokify_noargs_ctor_emits_noargsconstructor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Empty.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Empty {\n\
            \x20   private String name;\n\
            \n\
            \x20   public Empty() {}\n\
            \n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(rewritten.contains("import lombok.NoArgsConstructor;"));
        assert!(rewritten.contains("@Getter\n@NoArgsConstructor\npublic class Empty"));
        assert!(!rewritten.contains("public Empty() {}"));
    }

    #[test]
    fn lombokify_skips_ctor_with_validation() {
        // Constructor body has more than the canonical N assignments —
        // detector refuses. A no-arg getter still lombokifies.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Validating.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Validating {\n\
            \x20   private String name;\n\
            \n\
            \x20   public Validating(String name) {\n\
            \x20       if (name == null) throw new IllegalArgumentException();\n\
            \x20       this.name = name;\n\
            \x20   }\n\
            \n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Getter lombokified; ctor preserved.
        assert!(rewritten.contains("@Getter\npublic class Validating"));
        assert!(
            rewritten.contains("public Validating(String name)"),
            "validation-bearing ctor must be preserved:\n{rewritten}"
        );
        assert!(!rewritten.contains("@AllArgsConstructor"));
    }

    #[test]
    fn lombokify_skips_ctor_with_wrong_param_order() {
        // Params don't match field declaration order → not canonical.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Reordered.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Reordered {\n\
            \x20   private String first;\n\
            \x20   private String second;\n\
            \n\
            \x20   public Reordered(String second, String first) {\n\
            \x20       this.first = first;\n\
            \x20       this.second = second;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        // Type signature still matches (both are String), but the body
        // assignment order doesn't match parameter order — body iterates
        // `this.first = first` (first param is `second` → mismatch).
        // Detector refuses.
        let result = plan_lombokify_java_class(&params);
        assert!(
            result.is_err(),
            "wrong-order ctor should refuse; got: {result:?}"
        );
    }

    #[test]
    fn lombokify_collapses_to_data() {
        // Mutable POJO with full @Getter+@Setter+@EqualsAndHashCode+@ToString
        // + (matching) @RequiredArgsConstructor (which on a class with NO
        // final fields means a no-arg ctor) → collapse to @Data.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Bean.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.apache.commons.lang3.builder.EqualsBuilder;\n\
             import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
             import org.apache.commons.lang3.builder.ToStringBuilder;\n\
             \n\
             public class Bean {\n\
            \x20   private String name;\n\
            \x20   private int count;\n\
            \n\
            \x20   public Bean() {}\n\
            \n\
            \x20   public String getName() { return name; }\n\
            \x20   public int getCount() { return count; }\n\
            \x20   public void setName(String name) { this.name = name; }\n\
            \x20   public void setCount(int count) { this.count = count; }\n\
            \n\
            \x20   public boolean equals(Object other) {\n\
            \x20       if (this == other) return true;\n\
            \x20       if (other == null || getClass() != other.getClass()) return false;\n\
            \x20       Bean that = (Bean) other;\n\
            \x20       return new EqualsBuilder()\n\
            \x20               .append(name, that.name)\n\
            \x20               .append(count, that.count)\n\
            \x20               .isEquals();\n\
            \x20   }\n\
            \n\
            \x20   public int hashCode() {\n\
            \x20       return new HashCodeBuilder().append(name).append(count).toHashCode();\n\
            \x20   }\n\
            \n\
            \x20   public String toString() {\n\
            \x20       return new ToStringBuilder(this).append(\"name\", name).append(\"count\", count).toString();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Single @Data annotation, not the five individual ones.
        assert!(
            rewritten.contains("@Data\npublic class Bean"),
            "expected collapsed @Data:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("@Getter"),
            "individual annotations should be elided:\n{rewritten}"
        );
        assert!(!rewritten.contains("@Setter"));
        assert!(!rewritten.contains("@EqualsAndHashCode"));
        assert!(!rewritten.contains("@ToString"));
        assert!(!rewritten.contains("@NoArgsConstructor"));
        assert!(!rewritten.contains("@RequiredArgsConstructor"));
        // Lone import for @Data, not five separate.
        assert!(rewritten.contains("import lombok.Data;"));
        assert!(!rewritten.contains("import lombok.Getter;"));
        assert!(!rewritten.contains("import lombok.Setter;"));
        // All accessors and ctor + e/h/ts dropped.
        assert!(!rewritten.contains("public String getName()"));
        assert!(!rewritten.contains("public Bean()"));
        assert!(!rewritten.contains("public boolean equals("));
    }

    #[test]
    fn lombokify_collapses_to_value() {
        // Immutable POJO: every field final, all-args ctor, getters,
        // equals/hashCode/toString. No setters.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Immutable.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.apache.commons.lang3.builder.EqualsBuilder;\n\
             import org.apache.commons.lang3.builder.HashCodeBuilder;\n\
             import org.apache.commons.lang3.builder.ToStringBuilder;\n\
             \n\
             public class Immutable {\n\
            \x20   private final String name;\n\
            \x20   private final int count;\n\
            \n\
            \x20   public Immutable(String name, int count) {\n\
            \x20       this.name = name;\n\
            \x20       this.count = count;\n\
            \x20   }\n\
            \n\
            \x20   public String getName() { return name; }\n\
            \x20   public int getCount() { return count; }\n\
            \n\
            \x20   public boolean equals(Object other) {\n\
            \x20       if (this == other) return true;\n\
            \x20       if (other == null || getClass() != other.getClass()) return false;\n\
            \x20       Immutable that = (Immutable) other;\n\
            \x20       return new EqualsBuilder().append(name, that.name).append(count, that.count).isEquals();\n\
            \x20   }\n\
            \n\
            \x20   public int hashCode() {\n\
            \x20       return new HashCodeBuilder().append(name).append(count).toHashCode();\n\
            \x20   }\n\
            \n\
            \x20   public String toString() {\n\
            \x20       return new ToStringBuilder(this).append(\"name\", name).append(\"count\", count).toString();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            rewritten.contains("@Value\npublic class Immutable"),
            "expected collapsed @Value:\n{rewritten}"
        );
        assert!(rewritten.contains("import lombok.Value;"));
        assert!(!rewritten.contains("@Getter"));
        assert!(!rewritten.contains("@AllArgsConstructor"));
    }

    #[test]
    fn lombokify_detects_slf4j_logger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Loud.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.slf4j.Logger;\n\
             import org.slf4j.LoggerFactory;\n\
             \n\
             public class Loud {\n\
            \x20   private static final Logger log = LoggerFactory.getLogger(Loud.class);\n\
             \n\
            \x20   private String name;\n\
             \n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            rewritten.contains("@Slf4j"),
            "expected @Slf4j:\n{rewritten}"
        );
        assert!(rewritten.contains("import lombok.extern.slf4j.Slf4j;"));
        assert!(
            !rewritten.contains("private static final Logger log"),
            "logger field should be dropped:\n{rewritten}"
        );
    }

    #[test]
    fn lombokify_skips_slf4j_with_wrong_field_name() {
        // Lombok @Slf4j requires field name `log` exactly. Field named
        // `logger` doesn't match — preserved.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WrongName.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             import org.slf4j.Logger;\n\
             import org.slf4j.LoggerFactory;\n\
             \n\
             public class WrongName {\n\
            \x20   private static final Logger logger = LoggerFactory.getLogger(WrongName.class);\n\
             \n\
            \x20   private String name;\n\
             \n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            !rewritten.contains("@Slf4j"),
            "should NOT detect logger named `logger`:\n{rewritten}"
        );
        assert!(rewritten.contains("private static final Logger logger"));
    }

    #[test]
    fn lombokify_tree_walks_directory_and_aggregates() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("src/com/example");
        fs::create_dir_all(&src).unwrap();
        fs::write(
            src.join("Pair.java"),
            "package com.example;\n\
             public class Pair {\n\
            \x20   private String first;\n\
            \x20   private String second;\n\
            \x20   public Pair(String first, String second) { this.first = first; this.second = second; }\n\
            \x20   public String getFirst() { return first; }\n\
            \x20   public String getSecond() { return second; }\n\
             }\n",
        )
        .unwrap();
        fs::write(
            src.join("Single.java"),
            "package com.example;\n\
             public class Single {\n\
            \x20   private String name;\n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        // A file with nothing to lombokify: should land in `leftovers`.
        fs::write(
            src.join("Service.java"),
            "package com.example;\n\
             public class Service {\n\
            \x20   public void run() { /* no boilerplate */ }\n\
             }\n",
        )
        .unwrap();
        // A non-Java file (must not be picked up at all).
        fs::write(src.join("README.md"), "ignore me\n").unwrap();
        // A `target/` directory must be skipped.
        let target = root.path().join("target/classes");
        fs::create_dir_all(&target).unwrap();
        fs::write(
            target.join("Generated.java"),
            "package com.example;\n\
             public class Generated {\n\
            \x20   private String t;\n\
            \x20   public String getT() { return t; }\n\
             }\n",
        )
        .unwrap();

        let mut params = java_plan_params("lombokify_java_class", root.path());
        params.source = path_string(root.path());
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        assert!(
            plan.title.starts_with("Lombokify 2/3"),
            "title should report 2/3 conversions (Pair + Single, Service skipped, target/ filtered): {}",
            plan.title
        );
        // Two FileEdits — one per converted file.
        assert_eq!(plan.edits.len(), 2);
        let pair_edit = plan
            .edits
            .iter()
            .find(|e| e.path.ends_with("Pair.java"))
            .expect("Pair edit");
        let single_edit = plan
            .edits
            .iter()
            .find(|e| e.path.ends_with("Single.java"))
            .expect("Single edit");
        // Service should be in leftovers with the bail message.
        assert!(
            plan.leftovers
                .iter()
                .any(|s| s.contains("Service.java")
                    && (s.contains("no lombokifiable")
                        || s.contains("no instance fields"))),
            "Service.java should be in leftovers: {:?}",
            plan.leftovers
        );
        // target/ tree must NOT have leaked in.
        assert!(
            !plan
                .leftovers
                .iter()
                .any(|s| s.contains("Generated.java")),
            "Generated.java under target/ must be filtered out: {:?}",
            plan.leftovers
        );
        // Each FileEdit's hash should match the file at plan time.
        assert!(!pair_edit.original_sha256.is_empty());
        assert!(!single_edit.original_sha256.is_empty());
    }

    #[test]
    fn lombokify_tree_bails_when_no_candidates() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("src/Empty.java"),
            "package com.example;\n\
             public class Empty {\n\
            \x20   public void doNothing() {}\n\
             }\n",
        )
        .unwrap();
        let mut params = java_plan_params("lombokify_java_class", root.path());
        params.source = path_string(root.path());
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("no lombokifiable classes found under"),
            "expected tree-mode bail; got: {err}"
        );
    }

    /// Practice-run probe against a real repo. Set `LOMBOKIFY_PRACTICE_DIR`
    /// to a directory of Java sources; the test reports how many classes
    /// the planner can convert. Skipped unless the env var is set so CI
    /// doesn't depend on external paths.
    #[test]
    fn lombokify_practice_run() {
        let Ok(dir) = std::env::var("LOMBOKIFY_PRACTICE_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        if !dir.is_dir() {
            return;
        }
        let mut params = java_plan_params("lombokify_java_class", &dir);
        params.source = path_string(&dir);
        let plan_text = match plan_lombokify_java_class(&params) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("PRACTICE RUN: planner refused: {e}");
                return;
            }
        };
        let plan: RefactorPlan = serde_json::from_str(&plan_text).unwrap();
        eprintln!("PRACTICE RUN against {}", dir.display());
        eprintln!("  title: {}", plan.title);
        eprintln!("  files converted: {}", plan.edits.len());
        eprintln!("  files skipped:   {}", plan.leftovers.len());
        // Sample 5 conversions and 5 skips.
        for edit in plan.edits.iter().take(5) {
            eprintln!("    + {}: {} edits", edit.path, edit.edits.len());
        }
        for skip in plan.leftovers.iter().take(5) {
            eprintln!("    - {skip}");
        }
    }

    #[test]
    fn lombokify_boolean_with_get_prefix_skipped_by_default() {
        // Gap 1 case: `boolean showColumn` with hand-rolled `getShowColumn()`.
        // Lombok @Getter would generate `isShowColumn()`, so dropping the
        // hand-rolled getter would silently break callers. Default
        // boolean_getter_strategy=skip preserves the original method and
        // refuses class-level @Getter (since one field would be uncovered).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Report.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Report {\n\
            \x20   private boolean showColumn;\n\
            \x20   private String name;\n\
            \n\
            \x20   public boolean getShowColumn() { return showColumn; }\n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // showColumn's hand-rolled `getShowColumn()` survives.
        assert!(
            rewritten.contains("public boolean getShowColumn()"),
            "boolean getter with get-prefix must be preserved by default:\n{rewritten}"
        );
        // name still gets per-field @Getter.
        assert!(
            rewritten.contains("@Getter\n    private String name;"),
            "non-conflicting getter still lombokifies per-field:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("@Getter\npublic class Report"),
            "class-level @Getter must NOT fire when one field would mismatch:\n{rewritten}"
        );
        assert!(!rewritten.contains("public String getName()"));
    }

    #[test]
    fn lombokify_boolean_get_prefix_bridge_strategy() {
        // boolean_getter_strategy=bridge: drop original, emit bridge so
        // callers using `getShowColumn()` still compile alongside Lombok's
        // `isShowColumn()`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Report.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Report {\n\
            \x20   private boolean showColumn;\n\
            \n\
            \x20   public boolean getShowColumn() { return showColumn; }\n\
             }\n",
        )
        .unwrap();
        let mut params = java_plan_params("lombokify_java_class", &path);
        params.boolean_getter_strategy = Some("bridge".to_string());
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        // Class-level @Getter fires (full coverage now); the original
        // method body is replaced with a bridge that delegates to
        // Lombok's generated `isShowColumn()`.
        assert!(
            rewritten.contains("@Getter\npublic class Report"),
            "class-level @Getter expected:\n{rewritten}"
        );
        assert!(
            rewritten.contains("public boolean getShowColumn() {")
                && rewritten.contains("return isShowColumn();"),
            "bridge method must preserve get-prefix name and call lombok form:\n{rewritten}"
        );
    }

    #[test]
    fn lombokify_boolean_get_prefix_rename_strategy() {
        // boolean_getter_strategy=rename: drop original, accept Lombok's
        // name change. Caller-breaking but explicit.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Report.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Report {\n\
            \x20   private boolean showColumn;\n\
            \n\
            \x20   public boolean getShowColumn() { return showColumn; }\n\
             }\n",
        )
        .unwrap();
        let mut params = java_plan_params("lombokify_java_class", &path);
        params.boolean_getter_strategy = Some("rename".to_string());
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            rewritten.contains("@Getter\npublic class Report"),
            "expected class-level @Getter:\n{rewritten}"
        );
        // No bridge — original is gone, Lombok's generated isShowColumn() will surface.
        assert!(
            !rewritten.contains("public boolean getShowColumn()"),
            "rename strategy drops the original; got:\n{rewritten}"
        );
        assert!(
            !rewritten.contains("return isShowColumn();"),
            "rename strategy must NOT emit bridge:\n{rewritten}"
        );
    }

    #[test]
    fn lombokify_boolean_with_is_prefix_field_no_mismatch() {
        // Field name `isActive` with getter `isActive()` — Lombok would
        // also generate `isActive()` (no double-prefix), so no API
        // mismatch. Default strategy still applies normally.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flag.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Flag {\n\
            \x20   private boolean isActive;\n\
            \n\
            \x20   public boolean isActive() { return isActive; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let rewritten = apply_plan_to_source(&plan_text, &path);
        assert!(
            rewritten.contains("@Getter\npublic class Flag"),
            "no-mismatch case lombokifies normally:\n{rewritten}"
        );
        assert!(!rewritten.contains("public boolean isActive()"));
    }

    #[test]
    fn lombokify_invalid_strategy_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("X.java");
        fs::write(
            &path,
            "package com.example;\n\
             public class X { private String name; public String getName() { return name; } }\n",
        )
        .unwrap();
        let mut params = java_plan_params("lombokify_java_class", &path);
        params.boolean_getter_strategy = Some("nonsense".to_string());
        let err = plan_lombokify_java_class(&params).unwrap_err();
        assert!(
            err.to_string().contains("boolean_getter_strategy"),
            "expected param-validation error: {err}"
        );
    }

    #[test]
    fn lombokify_output_path_writes_plan_and_returns_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Pair.java");
        fs::write(
            &path,
            "package com.example;\n\
             public class Pair {\n\
            \x20   private String first;\n\
            \x20   private String second;\n\
            \x20   public String getFirst() { return first; }\n\
            \x20   public String getSecond() { return second; }\n\
             }\n",
        )
        .unwrap();
        // Point BLACKBOX_STATE_DIR at a temp dir so the slot resolves there.
        let state_dir = tempfile::tempdir().unwrap();
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", state_dir.path()) };
        let mut params = java_plan_params("lombokify_java_class", &path);
        params.output_path = Some("pair-plan.json".to_string());
        let response_text = plan(&params).unwrap();
        unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };
        // Response is a summary, not a full RefactorPlan.
        let summary: RefactorPlanSummary = serde_json::from_str(&response_text).unwrap();
        assert_eq!(summary.status, "ok");
        assert_eq!(summary.kind, "lombokify_java_class");
        assert_eq!(summary.file_count, 1);
        assert!(summary.total_edits > 0);
        assert_eq!(summary.files.len(), 1);
        assert!(summary.files[0].path.ends_with("Pair.java"));
        // Plan file written to the slot.
        let plan_path = std::path::Path::new(&summary.plan_path);
        assert!(plan_path.is_file(), "plan file should be written to disk");
        assert!(
            plan_path.starts_with(state_dir.path()),
            "plan should be inside the slot: {}",
            plan_path.display()
        );
        let written = fs::read_to_string(plan_path).unwrap();
        let on_disk: RefactorPlan = serde_json::from_str(&written).unwrap();
        assert_eq!(on_disk.kind, "lombokify_java_class");
        assert_eq!(on_disk.edits.len(), 1);
    }

    #[test]
    fn lombokify_apply_via_plan_path_roundtrip() {
        // End-to-end: plan with output_path → apply with plan_path.
        // This is the workflow that solves Gap 3 (large plans break the
        // MCP transport when serialized inline).
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("Bean.java");
        fs::write(
            &src,
            "package com.example;\n\
             public class Bean {\n\
            \x20   private String name;\n\
            \x20   public String getName() { return name; }\n\
             }\n",
        )
        .unwrap();
        // Point BLACKBOX_STATE_DIR at a temp dir so both plan + apply use the slot.
        let state_dir = tempfile::tempdir().unwrap();
        let _lock = crate::util::test_env_lock();
        unsafe { std::env::set_var("BLACKBOX_STATE_DIR", state_dir.path()) };
        let mut params = java_plan_params("lombokify_java_class", &src);
        params.output_path = Some("bean-plan.json".to_string());
        let summary_text = plan(&params).unwrap();
        let summary: RefactorPlanSummary = serde_json::from_str(&summary_text).unwrap();
        // Derive relative plan_path from summary for apply (or use the slot filename).
        let plan_filename = std::path::Path::new(&summary.plan_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap()
            .to_string();
        // Apply by reading the plan from the slot.
        let response = apply(
            &RefactorApplyParams {
                plan: serde_json::Value::Null,
                plan_path: Some(plan_filename),
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        unsafe { std::env::remove_var("BLACKBOX_STATE_DIR") };
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok");
        assert!(
            applied.validations.iter().all(|v| !v.has_error),
            "rewritten file must parse cleanly: {response}"
        );
        let final_text = fs::read_to_string(&src).unwrap();
        assert!(final_text.contains("@Getter\npublic class Bean"));
    }

    #[test]
    fn lombokify_apply_rejects_missing_plan_and_path() {
        let err = apply(
            &RefactorApplyParams {
                plan: serde_json::Value::Null,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: None,
                allow_unregistered_paths: None,
            },
            &[],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("plan") && err.to_string().contains("plan_path"),
            "expected refusal naming both options: {err}"
        );
    }

    /// End-to-end probe: copies LOMBOKIFY_PROBE_FILE into a tempdir,
    /// runs the planner+apply pipeline, and confirms the rewritten file
    /// parses cleanly (no syntax errors). Skipped unless env var is set.
    #[test]
    fn lombokify_probe_apply_clean_parse() {
        let Ok(path_str) = std::env::var("LOMBOKIFY_PROBE_FILE") else {
            return;
        };
        let src = PathBuf::from(&path_str);
        if !src.is_file() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(src.file_name().unwrap());
        fs::copy(&src, &dest).unwrap();
        let mut params = java_plan_params("lombokify_java_class", &dest);
        params.source = path_string(&dest);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        eprintln!("APPLY RESULT: {applied:?}");
        assert_eq!(applied.status, "ok", "apply should succeed");
        assert!(
            applied.validations.iter().all(|v| !v.has_error),
            "rewritten file must parse cleanly: {response}"
        );
        eprintln!("=== rewritten ===");
        eprintln!("{}", fs::read_to_string(&dest).unwrap());
    }

    /// Single-file probe: set `LOMBOKIFY_PROBE_FILE` to a path; the test
    /// runs the planner and prints the rewritten source. Used for visual
    /// verification of real-world conversions.
    #[test]
    fn lombokify_probe_single_file() {
        let Ok(path_str) = std::env::var("LOMBOKIFY_PROBE_FILE") else {
            return;
        };
        let path = PathBuf::from(&path_str);
        if !path.is_file() {
            return;
        }
        let mut params = java_plan_params("lombokify_java_class", &path);
        params.source = path_string(&path);
        match plan_lombokify_java_class(&params) {
            Ok(plan_text) => {
                let rewritten = apply_plan_to_source(&plan_text, &path);
                eprintln!("=== {} ===", path.display());
                eprintln!("--- before ---");
                eprintln!("{}", fs::read_to_string(&path).unwrap());
                eprintln!("--- after ---");
                eprintln!("{rewritten}");
            }
            Err(e) => eprintln!("PROBE: {e}"),
        }
    }

    #[test]
    fn lombokify_apply_writes_clean_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Pair.java");
        fs::write(
            &path,
            "package com.example;\n\
             \n\
             public class Pair {\n\
            \x20   private String first;\n\
            \x20   private String second;\n\
            \n\
            \x20   public String getFirst() { return first; }\n\
            \x20   public String getSecond() { return second; }\n\
             }\n",
        )
        .unwrap();
        let params = java_plan_params("lombokify_java_class", &path);
        let plan_text = plan_lombokify_java_class(&params).unwrap();
        let plan_value: serde_json::Value = serde_json::from_str(&plan_text).unwrap();
        let response = apply(
            &RefactorApplyParams {
                plan: plan_value,
                plan_path: None,
                confirm: Some(true),
                allow_dirty_worktree: Some(true),
                allow_unregistered_paths: Some(true),
            },
            &[project_record(dir.path())],
        )
        .unwrap();
        let applied: RefactorApplyResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(applied.status, "ok", "apply should succeed: {response}");
        assert!(
            applied.validations.iter().all(|v| !v.has_error),
            "rewritten file must parse cleanly: {response}"
        );
        let final_text = fs::read_to_string(&path).unwrap();
        assert!(final_text.contains("@Getter\npublic class Pair"));
        assert!(final_text.contains("import lombok.Getter;"));
        assert!(!final_text.contains("getFirst()"));
        assert!(!final_text.contains("getSecond()"));
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
            target_text.contains("final class Readings")
                || target_text.contains("class Readings"),
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
            !target_text.contains("private")
                && !target_text.contains("static class Readings"),
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
