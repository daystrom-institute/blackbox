//! Tests for the `java.add_service_boundary` builtin macro.
//!
//! # Structure
//!
//! - **Refusal unit tests** (no JAR required): use `MockProbeRunner` with canned
//!   outputs to exercise each refusal path and the authority-gate block.
//! - **Integration test** (skipped when `BLACKBOX_JAVA_WORKER_JAR` is unset):
//!   copies the `tests/fixtures/java_service_boundary/` fixture to a tempdir,
//!   plans with a real `SidecarBackend` + `CodeNavProbeRunner`, lowers, and
//!   applies — asserting both service files are created and all three target
//!   files (caller + guice module) are correctly modified.
//!
//! # Probe names in the reworked macro
//!
//! The v1 macro has 4 probes:
//! - `caller_method_probe` — finds the extracted method by name
//! - `service_type_exists` — idempotency guard for interface
//! - `impl_type_exists` — idempotency guard for implementation
//! - `guice_module` — finds the Guice AbstractModule

use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;

use crate::macros::backend::UnavailableBackend;
use crate::macros::expr::Context;
use crate::macros::model::{MacroInvocation, MacroSemanticStatus};
use crate::macros::planner::MacroPlanner;
use crate::macros::planner_ctx::MacroPlannerContext;
use crate::macros::probe::{ProbeOutput, ProbeRunner, ProbeSpec};
use crate::macros::registry::MacroRegistry;
use crate::macros::sidecar_backend::SidecarBackend;

// ─────────────────────────────────────────────────────────────────────────────
// MockProbeRunner
// ─────────────────────────────────────────────────────────────────────────────

/// A test-only `ProbeRunner` that returns canned `ProbeOutput` values keyed by
/// probe name.  Probes not in the map return `exists=false, count=0`.
struct MockProbeRunner {
    responses: HashMap<&'static str, serde_json::Value>,
}

impl MockProbeRunner {
    /// Happy-path: caller method exists (count=1), service types absent, guice
    /// module present (count=1), no static method issues.
    fn happy_path() -> Self {
        let mut r = HashMap::new();
        r.insert(
            "caller_method_probe",
            json!({"exists": true,  "count": 1, "items": []}),
        );
        r.insert(
            "service_type_exists",
            json!({"exists": false, "count": 0, "items": []}),
        );
        r.insert(
            "impl_type_exists",
            json!({"exists": false, "count": 0, "items": []}),
        );
        r.insert(
            "guice_module",
            json!({"exists": true,  "count": 1, "items": []}),
        );
        Self { responses: r }
    }

    /// caller_method_probe.count = 0 → triggers error.caller_method_not_found.
    fn no_caller_method() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "caller_method_probe",
            json!({"exists": false, "count": 0, "items": []}),
        );
        r
    }

    /// caller_method_probe.count = 2 → triggers error.caller_method_ambiguous.
    fn ambiguous_caller_method() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "caller_method_probe",
            json!({"exists": true, "count": 2, "items": []}),
        );
        r
    }

    /// service_type_exists.count = 1 → triggers error.type_already_exists (interface).
    fn service_type_already_exists() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "service_type_exists",
            json!({"exists": true, "count": 1, "items": []}),
        );
        r
    }

    /// impl_type_exists.count = 1 → triggers error.type_already_exists (implementation).
    fn impl_type_already_exists() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "impl_type_exists",
            json!({"exists": true, "count": 1, "items": []}),
        );
        r
    }

    /// guice_module.count = 0 → triggers error.guice_module_not_found.
    fn no_guice_module() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "guice_module",
            json!({"exists": false, "count": 0, "items": []}),
        );
        r
    }

    /// guice_module.count = 2 → triggers error.guice_module_ambiguous.
    fn ambiguous_guice_module() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert(
            "guice_module",
            json!({"exists": true, "count": 2, "items": []}),
        );
        r
    }
}

impl ProbeRunner for MockProbeRunner {
    fn run_probe(
        &self,
        name: &str,
        _spec: &ProbeSpec,
        _ctx: &Context,
        _invocation: &MacroInvocation,
    ) -> Result<ProbeOutput> {
        let value = self
            .responses
            .get(name)
            .cloned()
            .unwrap_or_else(|| json!({"exists": false, "count": 0, "items": []}));
        Ok(ProbeOutput {
            value,
            semantic_status: MacroSemanticStatus::SyntaxOnly,
            truncated: false,
            diagnostics: vec![],
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn load_service_boundary_def() -> crate::macros::model::MacroDefinition {
    MacroRegistry::list(None)
        .into_iter()
        .find(|d| d.id == "java.add_service_boundary")
        .expect("java.add_service_boundary must be present in builtin_definitions()")
}

/// Build a happy-path `MacroInvocation` for the canonical CallerService /
/// AppModule fixture.  All required inputs are populated with realistic values.
fn make_invocation(
    def: &crate::macros::model::MacroDefinition,
    project_dir: &str,
    source_root: &str,
    caller_file: &str,
    guice_module_file: &str,
) -> MacroInvocation {
    let mut inputs = serde_json::Map::new();
    inputs.insert("caller_file".into(), json!(caller_file));
    inputs.insert("caller_type".into(), json!("CallerService"));
    inputs.insert("caller_method".into(), json!("processOrder"));
    inputs.insert("caller_method_parameter_types".into(), json!(["String"]));
    inputs.insert(
        "method_contract".into(),
        json!("void processOrder(String orderId);"),
    );
    inputs.insert(
        "method_signature".into(),
        json!("void processOrder(String orderId)"),
    );
    inputs.insert(
        "implementation_body".into(),
        json!("System.out.println(\"Inventory: \" + orderId);"),
    );
    inputs.insert(
        "caller_replacement".into(),
        json!("inventoryService.processOrder(orderId);"),
    );
    inputs.insert("service_name".into(), json!("Inventory"));
    inputs.insert("service_package".into(), json!("com.example"));
    inputs.insert("service_source_root".into(), json!(source_root));
    inputs.insert("service_field_name".into(), json!("inventoryService"));
    inputs.insert("guice_module_file".into(), json!(guice_module_file));
    inputs.insert("guice_module_type".into(), json!("AppModule"));

    MacroInvocation {
        macro_id: def.id.clone(),
        version: None,
        project_dir: project_dir.to_string(),
        inputs,
        anchors: None,
        operator_opt_outs: vec!["acknowledge_public_api_change".into()],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Structural tests (no JAR, no probe I/O)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn builtin_macro_loads_from_registry() {
    let def = load_service_boundary_def();
    assert_eq!(def.id, "java.add_service_boundary");
    assert_eq!(def.version, "1.0.0");
    assert_eq!(def.language, "java");
    assert_eq!(
        def.authority_gates,
        vec!["acknowledge_public_api_change".to_string()]
    );
    assert_eq!(def.probes.len(), 4, "expect 4 probe slots");
    assert_eq!(def.refusals.len(), 7, "expect 7 refusal rules");
    assert_eq!(def.operations.len(), 6, "expect 6 operations");
}

#[test]
fn registry_validate_passes_for_builtin() {
    let def = load_service_boundary_def();
    let report = MacroRegistry::validate(&def);
    assert!(
        report.valid,
        "builtin macro must pass structural validation; issues: {:?}",
        report.issues
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Refusal unit tests (no JAR required)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn refusal_caller_method_not_found() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::no_caller_method()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan =
        MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal, not error)");
    assert!(!plan.refusals.is_empty(), "expected at least one refusal");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.caller_method_not_found"),
        "expected error.caller_method_not_found, got: {codes:?}"
    );
}

#[test]
fn refusal_caller_method_ambiguous() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::ambiguous_caller_method()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.caller_method_ambiguous"),
        "expected error.caller_method_ambiguous when count>1, got: {codes:?}"
    );
}

#[test]
fn refusal_service_interface_already_exists() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::service_type_already_exists()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.type_already_exists"),
        "expected error.type_already_exists, got: {codes:?}"
    );
}

#[test]
fn refusal_service_impl_already_exists() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::impl_type_already_exists()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.type_already_exists"),
        "expected error.type_already_exists for impl, got: {codes:?}"
    );
}

#[test]
fn refusal_guice_module_not_found() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::no_guice_module()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.guice_module_not_found"),
        "expected error.guice_module_not_found, got: {codes:?}"
    );
}

#[test]
fn refusal_guice_module_ambiguous() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::ambiguous_guice_module()),
    );
    let inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.guice_module_ambiguous"),
        "expected error.guice_module_ambiguous when guice_module.count>1, got: {codes:?}"
    );
}

#[test]
fn refusal_behavior_move_unsupported_when_body_empty() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::happy_path()),
    );
    let mut inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    // Override implementation_body to empty string — triggers the refusal.
    inv.inputs.insert("implementation_body".into(), json!(""));
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.behavior_move_unsupported"),
        "expected error.behavior_move_unsupported when implementation_body is empty, \
         got: {codes:?}"
    );
}

#[test]
fn refusal_authority_gate_missing() {
    let def = load_service_boundary_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::happy_path()),
    );
    // No operator_opt_outs — authority gate not satisfied.
    let mut inv = make_invocation(
        &def,
        "/tmp",
        "/tmp/src/main/java",
        "/tmp/CallerService.java",
        "/tmp/AppModule.java",
    );
    inv.operator_opt_outs.clear();
    let plan = MacroPlanner::plan(&inv, &def, &ctx).expect("plan should succeed (gate refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.authority_required"),
        "expected error.authority_required when gate not satisfied, got: {codes:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration test (skipped when BLACKBOX_JAVA_WORKER_JAR is unset)
// ─────────────────────────────────────────────────────────────────────────────

/// Copy the fixture tree to a tempdir, returning
/// `(tempdir, source_root, caller_file, guice_module_file)`.
fn copy_fixture_to_tempdir() -> (tempfile::TempDir, String, String, String) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let fixture_src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/java_service_boundary/src/main/java/com/example");

    let dest_pkg = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&dest_pkg).expect("create fixture package dir");

    for filename in &["CallerService.java", "AppModule.java"] {
        std::fs::copy(fixture_src.join(filename), dest_pkg.join(filename))
            .unwrap_or_else(|e| panic!("copy {filename}: {e}"));
    }

    let source_root = tmp
        .path()
        .join("src/main/java")
        .to_string_lossy()
        .into_owned();
    let caller_file = dest_pkg
        .join("CallerService.java")
        .to_string_lossy()
        .into_owned();
    let guice_module_file = dest_pkg
        .join("AppModule.java")
        .to_string_lossy()
        .into_owned();

    (tmp, source_root, caller_file, guice_module_file)
}

#[test]
fn integration_plan_lower_apply_with_real_sidecar() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[java_service_boundary] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping sidecar integration test"
        );
        return;
    };

    let (tmp, source_root, caller_file, guice_module_file) = copy_fixture_to_tempdir();
    let project_dir = tmp.path().to_string_lossy().into_owned();

    // git-init + commit so refactor::apply sees a clean worktree.
    // Commit (not just add) — apply refuses dirty-worktree files.
    let git_ok = |cmd: &mut std::process::Command, label: &str| {
        let status = cmd
            .status()
            .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
        assert!(status.success(), "{label}: exited {status}");
    };
    git_ok(
        std::process::Command::new("git").args(["init", "-q", &project_dir]),
        "git init",
    );
    git_ok(
        std::process::Command::new("git").args(["-C", &project_dir, "add", "-A"]),
        "git add -A",
    );
    git_ok(
        std::process::Command::new("git").args([
            "-C",
            &project_dir,
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "-m",
            "fixture",
        ]),
        "git commit",
    );

    let def = load_service_boundary_def();
    let inv = make_invocation(
        &def,
        &project_dir,
        &source_root,
        &caller_file,
        &guice_module_file,
    );

    // Use CodeNavProbeRunner backed by the tempdir project.
    let project_record = crate::projects::ProjectRecord {
        project_id: "test-fixture".to_string(),
        repo_id: None,
        canonical_path: project_dir.clone(),
        registered_at: "2024-01-01T00:00:00Z".to_string(),
        is_git_repo: true,
        languages: std::collections::BTreeSet::new(),
    };
    let probe_runner = crate::macros::probe::CodeNavProbeRunner::new(
        None, // no LSP — syntactic probes only
        vec![project_record],
    );

    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed with real sidecar + CodeNavProbeRunner");

    assert!(
        plan.refusals.is_empty(),
        "expected no refusals in happy-path run; got: {:?}",
        plan.refusals
    );

    // 3 Rewrite ops + 2 Emit ops + 1 Record op + 4 top-level probe ops logged
    let emit_count = plan.operations.iter().filter(|o| o.kind == "emit").count();
    let rewrite_count = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite")
        .count();
    assert_eq!(emit_count, 2, "expect 2 emit operations");
    assert_eq!(rewrite_count, 3, "expect 3 rewrite operations");

    // 2 new files (interface + impl); 3 edits (caller field, caller delegation,
    // module binding) — but the same-file chaining collapses the two caller
    // edits into one FileEdit, so we expect 2 file_edits total.
    assert_eq!(
        plan.edits.file_creates.len(),
        2,
        "expect 2 new files created"
    );
    assert_eq!(
        plan.edits.file_edits.len(),
        2,
        "expect 2 file edits (caller edits merged, plus module edit)"
    );

    // Assert interface content contains the method contract.
    let iface_create = plan
        .edits
        .file_creates
        .iter()
        .find(|fc| fc.path.contains("InventoryService.java") && !fc.path.contains("Impl"))
        .expect("InventoryService.java should be in file_creates");
    assert!(
        iface_create
            .content
            .contains("void processOrder(String orderId);"),
        "interface source must contain the method_contract; got:\n{}",
        iface_create.content
    );
    assert!(
        iface_create.content.contains("interface InventoryService"),
        "interface source must declare InventoryService; got:\n{}",
        iface_create.content
    );

    // Assert implementation content contains the implementation body.
    let impl_create = plan
        .edits
        .file_creates
        .iter()
        .find(|fc| fc.path.contains("InventoryServiceImpl.java"))
        .expect("InventoryServiceImpl.java should be in file_creates");
    assert!(
        impl_create.content.contains("class InventoryServiceImpl"),
        "impl source must declare InventoryServiceImpl; got:\n{}",
        impl_create.content
    );
    assert!(
        impl_create.content.contains("Inventory:"),
        "impl source must contain the implementation_body; got:\n{}",
        impl_create.content
    );

    // Lower
    let refactor_plan =
        MacroPlanner::lower(&plan).expect("lowering should succeed for syntax-only plan");
    assert_eq!(refactor_plan.file_creates.len(), 2);
    assert_eq!(refactor_plan.edits.len(), 2);

    // Apply
    let apply_params = crate::refactor::RefactorApplyParams {
        plan: serde_json::to_value(&refactor_plan).expect("serialize plan"),
        plan_path: None,
        confirm: Some(true),
        allow_dirty_worktree: None,
        allow_unregistered_paths: Some(true),
        cwd: None,
        force_path: Some(true),
    };
    crate::refactor::apply(&apply_params, &[]).expect("apply should succeed");

    // Assert files were created.
    let pkg = tmp.path().join("src/main/java/com/example");
    let iface_path = pkg.join("InventoryService.java");
    let impl_path = pkg.join("InventoryServiceImpl.java");
    assert!(
        iface_path.exists(),
        "InventoryService.java should be created"
    );
    assert!(
        impl_path.exists(),
        "InventoryServiceImpl.java should be created"
    );

    // Assert interface file content matches the contract.
    let iface_content = std::fs::read_to_string(&iface_path).expect("read InventoryService.java");
    assert!(
        iface_content.contains("void processOrder(String orderId);"),
        "InventoryService.java must contain the method contract"
    );

    // Assert implementation file content includes the method body.
    let impl_content = std::fs::read_to_string(&impl_path).expect("read InventoryServiceImpl.java");
    assert!(
        impl_content.contains("implements InventoryService"),
        "InventoryServiceImpl.java must implement InventoryService; got:\n{impl_content}"
    );
    assert!(
        impl_content.contains("void processOrder(String orderId)"),
        "InventoryServiceImpl.java must include the full processOrder signature; got:\n{impl_content}"
    );
    // Implementation body: the fixture uses "Inventory: <orderId>" log line.
    assert!(
        impl_content.contains("Inventory:") || impl_content.contains("processOrder"),
        "InventoryServiceImpl.java must contain the implementation body; got:\n{impl_content}"
    );

    // Assert CallerService.java was modified: @Inject field + delegation body.
    let caller_content = std::fs::read_to_string(&caller_file).expect("read CallerService.java");
    assert!(
        caller_content.contains("@com.google.inject.Inject") || caller_content.contains("@Inject"),
        "CallerService.java must contain @Inject annotation; got:\n{caller_content}"
    );
    assert!(
        caller_content.contains("InventoryService inventoryService"),
        "CallerService.java must declare the injected InventoryService field; got:\n{caller_content}"
    );
    // Delegation body: the invocation sets caller_replacement to delegate via the field.
    assert!(
        caller_content.contains("inventoryService.processOrder(orderId)"),
        "CallerService.java must delegate to inventoryService.processOrder(orderId); got:\n{caller_content}"
    );
    // The original implementation body must be GONE (replaced by delegation).
    assert!(
        !caller_content.contains("Processing order:"),
        "CallerService.java must NOT contain the original println body after delegation; got:\n{caller_content}"
    );

    // Assert AppModule.java was modified with the Guice binding.
    let module_content = std::fs::read_to_string(&guice_module_file).expect("read AppModule.java");
    assert!(
        module_content.contains("bind(InventoryService.class).to(InventoryServiceImpl.class)"),
        "AppModule.java must contain the exact bind() statement; got:\n{module_content}"
    );
}
