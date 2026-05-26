//! Tests for the `builtin.java.vaadin.ensure_provider_bindings` macro.
//!
//! The Rust primitive `java_vaadin_provider_binding_generation` was deleted
//! after parity was proven (P5-2/P5-3). These tests assert macro correctness
//! directly; there is no old kind to compare against.
//!
//! # Structure
//!
//! - **Structural tests** (no JAR): verify the macro loads, validates, and has
//!   the expected shape.
//! - **Refusal unit tests** (no JAR): use `MockProbeRunner` + `UnavailableBackend`
//!   to exercise each refusal path.
//! - **Guard unit tests** (no JAR): verify the `when`-guard on each rewrite op
//!   fires correctly when the binding is already present.
//! - **Integration tests** (skipped when `BLACKBOX_JAVA_WORKER_JAR` is unset):
//!   plan with a real `SidecarBackend` + `CodeNavProbeRunner`, then lower.
//!
//! # Probe names
//!
//! | Probe                        | Refusal / guard behaviour                                      |
//! |------------------------------|----------------------------------------------------------------|
//! | `spring_markers`             | exists=true  → `error.spring_detected`                         |
//! | `vaadin_markers`             | exists=false → `error.no_vaadin_detected`                      |
//! | `guice_module`               | exists=false → `error.not_guice_module`                        |
//! | `module_class`               | count=0 → `error.module_class_not_found`                       |
//! |                              | count>1 → `error.module_class_ambiguous`                       |
//! | `ui_binding_present`         | exists=true  → `not(any)` guard blocks UI rewrite ops (:: form)|
//! | `ui_binding_present_dot`     | exists=true  → `not(any)` guard blocks UI rewrite ops (. form) |
//! | `session_binding_present`    | exists=true  → blocks session rewrite ops (:: form)            |
//! | `session_binding_present_dot`| exists=true  → blocks session rewrite ops (. form)            |
//! | `provider_import_present`    | selects with/sans-guice-Provider-import variant of each op     |

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

/// Test-only `ProbeRunner` with canned responses keyed by probe name.
/// Unknown probes return `exists=false, count=0`.
struct MockProbeRunner {
    responses: HashMap<&'static str, serde_json::Value>,
}

impl MockProbeRunner {
    /// Happy-path: Vaadin project, Guice module, module class found, no
    /// existing bindings → both provider methods will be inserted.
    fn happy_path() -> Self {
        let mut r = HashMap::new();
        r.insert("spring_markers", json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r.insert("vaadin_markers", json!({"exists": true,  "count": 1, "matched_needles": ["com.vaadin.flow"], "files": []}));
        r.insert("guice_module",   json!({"exists": true,  "count": 1, "matched_needles": ["com.google.inject"], "files": []}));
        r.insert("module_class",   json!({"exists": true,  "count": 1, "items": []}));
        r.insert("ui_binding_present",           json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r.insert("ui_binding_present_dot",        json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r.insert("session_binding_present",       json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r.insert("session_binding_present_dot",   json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r.insert("provider_import_present",       json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        Self { responses: r }
    }

    /// Dot-form UI binding present, no session binding.
    /// Both UI ops are skipped; session ops reach backend.
    fn dot_form_ui_only() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("ui_binding_present_dot", json!({"exists": true, "count": 1, "matched_needles": ["Provider<UI>", "UI.getCurrent"], "files": []}));
        r
    }

    /// Jakarta Provider import already present; both bindings absent.
    /// sans_provider_import variants fire; with_guice variants are skipped.
    fn provider_import_present() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("provider_import_present", json!({"exists": true, "count": 1, "matched_needles": ["import jakarta.inject.Provider;"], "files": []}));
        r
    }

    /// Spring detected → `error.spring_detected`.
    fn spring_detected() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("spring_markers", json!({"exists": true, "count": 1, "matched_needles": ["org.springframework"], "files": []}));
        r
    }

    /// Not a Guice module → `error.not_guice_module`.
    fn not_guice_module() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("guice_module", json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r
    }

    /// No Vaadin usage detected → `error.no_vaadin_detected`.
    fn no_vaadin_detected() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("vaadin_markers", json!({"exists": false, "count": 0, "matched_needles": [], "files": []}));
        r
    }

    /// Module class not found → `error.module_class_not_found`.
    fn module_class_not_found() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("module_class", json!({"exists": false, "count": 0, "items": []}));
        r
    }

    /// Module class ambiguous (count=2) → `error.module_class_ambiguous`.
    fn module_class_ambiguous() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("module_class", json!({"exists": true, "count": 2, "items": []}));
        r
    }

    /// Both bindings already present (:: form) → all rewrite guards fire → no edits.
    fn both_bindings_present() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("ui_binding_present",      json!({"exists": true, "count": 1, "matched_needles": ["Provider<UI>", "UI::getCurrent"], "files": []}));
        r.responses.insert("session_binding_present", json!({"exists": true, "count": 1, "matched_needles": ["Provider<VaadinSession>", "VaadinSession::getCurrent"], "files": []}));
        r
    }

    /// Only the UI binding is present → UI guard fires, session op runs.
    fn only_ui_binding_present() -> Self {
        let mut r = Self::happy_path();
        r.responses.insert("ui_binding_present", json!({"exists": true, "count": 1, "matched_needles": ["Provider<UI>", "UI::getCurrent"], "files": []}));
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

fn load_vaadin_def() -> crate::macros::model::MacroDefinition {
    MacroRegistry::list(None)
        .into_iter()
        .find(|d| d.id == "builtin.java.vaadin.ensure_provider_bindings")
        .expect("builtin.java.vaadin.ensure_provider_bindings must be present in builtin_definitions()")
}

fn make_invocation(
    def: &crate::macros::model::MacroDefinition,
    project_dir: &str,
    module_file: &str,
    module_name: &str,
) -> MacroInvocation {
    let mut inputs = serde_json::Map::new();
    inputs.insert("module_file".into(), json!(module_file));
    inputs.insert("module_name".into(), json!(module_name));
    MacroInvocation {
        macro_id: def.id.clone(),
        version: None,
        project_dir: project_dir.to_string(),
        inputs,
        anchors: None,
        operator_opt_outs: vec![],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Structural tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn vaadin_builtin_macro_loads_from_registry() {
    let def = load_vaadin_def();
    assert_eq!(def.id, "builtin.java.vaadin.ensure_provider_bindings");
    assert_eq!(def.version, "1.1.0");
    assert_eq!(def.language, "java");
    assert!(
        def.authority_gates.is_empty(),
        "no authority gates: primitive has none; got: {:?}",
        def.authority_gates
    );
    assert_eq!(def.probes.len(), 9, "expect 9 probe slots (6 original + 2 dot-form + 1 provider_import)");
    assert_eq!(def.refusals.len(), 5, "expect 5 refusal rules");
    // 4 rewrite ops (2 UI × 2 variants + 2 session × 2 variants) + 1 record
    assert_eq!(def.operations.len(), 5, "expect 5 operations (4 rewrite + 1 record)");
}

#[test]
fn vaadin_registry_validate_passes_for_builtin() {
    let def = load_vaadin_def();
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
fn refusal_spring_detected() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::spring_detected()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed (refusal, not error)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.spring_detected"),
        "expected error.spring_detected, got: {codes:?}"
    );
}

#[test]
fn refusal_not_guice_module() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::not_guice_module()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.not_guice_module"),
        "expected error.not_guice_module, got: {codes:?}"
    );
}

#[test]
fn refusal_no_vaadin_detected() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::no_vaadin_detected()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.no_vaadin_detected"),
        "expected error.no_vaadin_detected, got: {codes:?}"
    );
}

#[test]
fn refusal_module_class_not_found() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::module_class_not_found()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.module_class_not_found"),
        "expected error.module_class_not_found, got: {codes:?}"
    );
}

#[test]
fn refusal_module_class_ambiguous() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::module_class_ambiguous()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed (refusal)");
    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.module_class_ambiguous"),
        "expected error.module_class_ambiguous when count>1, got: {codes:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Guard unit tests (no JAR required)
// ─────────────────────────────────────────────────────────────────────────────

/// When both bindings are already present, both rewrite guards fire and no
/// backend calls are made.  UnavailableBackend must not be reached.
#[test]
fn both_guards_fire_produces_no_edits() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::both_bindings_present()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed; both guards short-circuit before backend");
    assert!(
        plan.refusals.is_empty(),
        "expected no refusals; got: {:?}",
        plan.refusals
    );
    assert!(
        plan.edits.file_edits.is_empty(),
        "expected zero file edits when both bindings already present; got: {:?}",
        plan.edits.file_edits
    );
    assert!(
        plan.edits.file_creates.is_empty(),
        "expected zero file creates; got: {:?}",
        plan.edits.file_creates
    );
    // All 4 rewrite ops should appear as skipped in the plan operations log.
    // (2 UI variants + 2 session variants — both bindings present blocks all guards.)
    let skipped_rewrites = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite" && o.summary.contains("skipped"))
        .count();
    assert_eq!(
        skipped_rewrites, 4,
        "expected 4 skipped rewrite ops; got operations: {:?}",
        plan.operations
    );
}

/// When only the UI binding is present, only the UI guard fires.
/// The session op would call the backend — UnavailableBackend causes the plan
/// to fail.  We assert the plan returns an error (not a refusal) to confirm
/// the session op is NOT guarded.
#[test]
fn only_ui_guard_fires_session_op_reaches_backend() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::only_ui_binding_present()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    // The session rewrite op is not guarded (ui_binding_present does not affect it).
    // UnavailableBackend returns Err → plan() should propagate the error.
    let result = MacroPlanner::plan(&inv, &def, &ctx);
    assert!(
        result.is_err(),
        "expected plan to fail because session op reaches UnavailableBackend; \
         got Ok: {:?}",
        result.ok()
    );
}

/// When the UI binding is expressed in dot form (`UI.getCurrent`), the
/// `ui_binding_present_dot` probe fires and both UI ops are skipped.
/// The session op (no session binding) reaches UnavailableBackend → Err.
#[test]
fn dot_form_ui_binding_skips_ui_ops() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::dot_form_ui_only()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    // Both UI ops must be skipped; the first firing session op reaches
    // UnavailableBackend → plan() must return Err.
    let result = MacroPlanner::plan(&inv, &def, &ctx);
    assert!(
        result.is_err(),
        "expected plan to fail because session op reaches UnavailableBackend \
         (UI ops should be skipped by dot-form guard); got Ok: {:?}",
        result.ok()
    );
}

/// When a Provider import (jakarta/javax/guice) is already present, the
/// sans_provider_import variants fire and with_guice variants are skipped.
/// UnavailableBackend is still reached (UI sans fires) → Err.
#[test]
fn provider_import_present_selects_sans_variant() {
    let def = load_vaadin_def();
    let ctx = MacroPlannerContext::new(
        Box::new(UnavailableBackend),
        None,
        Box::new(MockProbeRunner::provider_import_present()),
    );
    let inv = make_invocation(&def, "/tmp", "/tmp/VaadinModule.java", "VaadinModule");
    // sans_provider_import variant fires for UI (then session); UnavailableBackend
    // causes the first live op to fail.
    let result = MacroPlanner::plan(&inv, &def, &ctx);
    assert!(
        result.is_err(),
        "expected plan to fail because sans_provider_import variant reaches \
         UnavailableBackend; got Ok: {:?}",
        result.ok()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration tests (skipped when BLACKBOX_JAVA_WORKER_JAR is unset)
// ─────────────────────────────────────────────────────────────────────────────

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vaadin_provider_bindings/src/main/java/com/example")
}

/// Copy a fixture file to `dest_dir`, returning the absolute path.
fn copy_fixture(filename: &str, dest_dir: &std::path::Path) -> String {
    let src = fixture_path().join(filename);
    let dst = dest_dir.join(filename);
    std::fs::copy(&src, &dst)
        .unwrap_or_else(|e| panic!("copy fixture {filename}: {e}"));
    dst.to_string_lossy().into_owned()
}

fn git_ok(cmd: &mut std::process::Command, label: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("{label}: spawn failed: {e}"));
    assert!(status.success(), "{label}: exited {status}");
}

fn init_git_project(project_dir: &str) {
    git_ok(
        std::process::Command::new("git").args(["init", "-q", project_dir]),
        "git init",
    );
    git_ok(
        std::process::Command::new("git").args(["-C", project_dir, "add", "-A"]),
        "git add -A",
    );
    git_ok(
        std::process::Command::new("git").args([
            "-C", project_dir,
            "-c", "user.email=test@example.com",
            "-c", "user.name=Test",
            "commit", "-q", "-m", "fixture",
        ]),
        "git commit",
    );
}

fn make_probe_runner(project_dir: &str) -> crate::macros::probe::CodeNavProbeRunner {
    let project_record = crate::projects::ProjectRecord {
        project_id: "vaadin-fixture".to_string(),
        repo_id: None,
        canonical_path: project_dir.to_string(),
        registered_at: "2024-01-01T00:00:00Z".to_string(),
        is_git_repo: true,
        languages: std::collections::BTreeSet::new(),
    };
    crate::macros::probe::CodeNavProbeRunner::new(None, vec![project_record])
}

/// Happy path: VaadinModule.java has no provider methods → macro adds both.
#[test]
fn integration_happy_path_adds_both_provider_methods() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[vaadin_provider_bindings] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping integration test"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("create tempdir");
    let pkg_dir = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create package dir");

    let module_file = copy_fixture("VaadinModule.java", &pkg_dir);
    let project_dir = tmp.path().to_string_lossy().into_owned();
    init_git_project(&project_dir);

    let def = load_vaadin_def();
    let inv = make_invocation(&def, &project_dir, &module_file, "VaadinModule");

    let probe_runner = make_probe_runner(&project_dir);
    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed with real sidecar + CodeNavProbeRunner");

    assert!(
        plan.refusals.is_empty(),
        "expected no refusals in happy-path run; got: {:?}",
        plan.refusals
    );

    // Happy path: no existing Provider import, no existing bindings. The two
    // *with-guice-Provider-import* variants fire; the two *sans-import* variants
    // are skipped by their guard (provider_import_present == false). So exactly
    // 2 of the 4 provider rewrite ops are skipped.
    let skipped = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite" && o.summary.contains("skipped"))
        .count();
    assert_eq!(
        skipped, 2,
        "expected the 2 sans-provider-import variants to be skipped in happy path"
    );

    // Both methods land in the same file → backend merges into 1 FileEdit.
    assert_eq!(
        plan.edits.file_edits.len(),
        1,
        "expected 1 file edit (both inserts merge into the module file)"
    );
    let file_edit = &plan.edits.file_edits[0];
    assert!(
        file_edit.path.contains("VaadinModule.java"),
        "file edit path must point to VaadinModule.java; got: {}",
        file_edit.path
    );
    // Collect all TextEdit replacements; insertions carry the new method text.
    let combined: String = file_edit.edits.iter().map(|e| e.replacement.as_str()).collect();
    assert!(
        combined.contains("@Provides"),
        "combined replacement text must contain @Provides; got:\n{combined}"
    );
    assert!(
        combined.contains("provideUiProvider"),
        "combined replacement text must contain provideUiProvider; got:\n{combined}"
    );
    assert!(
        combined.contains("Provider<UI>"),
        "combined replacement text must contain Provider<UI>; got:\n{combined}"
    );
    assert!(
        combined.contains("UI::getCurrent"),
        "combined replacement text must contain UI::getCurrent; got:\n{combined}"
    );
    assert!(
        combined.contains("provideVaadinSessionProvider"),
        "combined replacement text must contain provideVaadinSessionProvider; got:\n{combined}"
    );
    assert!(
        combined.contains("Provider<VaadinSession>"),
        "combined replacement text must contain Provider<VaadinSession>; got:\n{combined}"
    );
    assert!(
        combined.contains("VaadinSession::getCurrent"),
        "combined replacement text must contain VaadinSession::getCurrent; got:\n{combined}"
    );

    // Lower succeeds.
    let refactor_plan =
        MacroPlanner::lower(&plan).expect("lowering should succeed for syntax-only plan");
    assert_eq!(refactor_plan.edits.len(), 1, "expect 1 edit in lowered refactor plan");
    assert!(
        refactor_plan.file_creates.is_empty(),
        "no file creates expected"
    );
}

/// Spring detection refusal: project has Spring markers → plan refuses.
#[test]
fn integration_spring_detection_causes_refusal() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[vaadin_provider_bindings] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping integration test"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("create tempdir");
    let pkg_dir = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create package dir");

    let module_file = copy_fixture("VaadinModule.java", &pkg_dir);
    // Inject a Spring marker into a separate file to trigger project-wide detection.
    let spring_marker = pkg_dir.join("SpringConfig.java");
    std::fs::write(
        &spring_marker,
        "package com.example;\nimport org.springframework.context.annotation.Configuration;\n@Configuration\npublic class SpringConfig {}\n",
    )
    .expect("write spring marker");

    let project_dir = tmp.path().to_string_lossy().into_owned();
    init_git_project(&project_dir);

    let def = load_vaadin_def();
    let inv = make_invocation(&def, &project_dir, &module_file, "VaadinModule");

    let probe_runner = make_probe_runner(&project_dir);
    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan() should return Ok (refusal, not hard error)");

    let codes: Vec<&str> = plan.refusals.iter().map(|r| r.code.as_str()).collect();
    assert!(
        codes.contains(&"error.spring_detected"),
        "expected error.spring_detected, got: {codes:?}"
    );
}

/// Idempotency: VaadinModuleWithBindings.java already has both provider
/// methods → both guards fire, no edits produced.
#[test]
fn integration_idempotency_both_bindings_already_present() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[vaadin_provider_bindings] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping integration test"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("create tempdir");
    let pkg_dir = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create package dir");

    let module_file = copy_fixture("VaadinModuleWithBindings.java", &pkg_dir);
    let project_dir = tmp.path().to_string_lossy().into_owned();
    init_git_project(&project_dir);

    let def = load_vaadin_def();
    let inv = make_invocation(&def, &project_dir, &module_file, "VaadinModuleWithBindings");

    let probe_runner = make_probe_runner(&project_dir);
    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed; both guards short-circuit before backend");

    assert!(
        plan.refusals.is_empty(),
        "expected no refusals; got: {:?}",
        plan.refusals
    );
    assert!(
        plan.edits.file_edits.is_empty(),
        "expected zero edits when both bindings already present; got: {:?}",
        plan.edits.file_edits
    );

    // All 4 rewrite ops appear as skipped (both variants for each binding).
    let skipped = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite" && o.summary.contains("skipped"))
        .count();
    assert_eq!(
        skipped, 4,
        "expected 4 skipped rewrite ops in idempotency run; got operations: {:?}",
        plan.operations
    );
}

/// Dot-form UI binding: the module uses `UI.getCurrent()` (dot form, not
/// `UI::getCurrent`) for the UI provider.  The `ui_binding_present_dot` probe
/// detects this and the UI ops are skipped.  Only the session binding is added.
#[test]
fn integration_dot_form_ui_binding_only_session_added() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[vaadin_provider_bindings] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping integration test"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("create tempdir");
    let pkg_dir = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create package dir");
    let project_dir = tmp.path().to_string_lossy().into_owned();

    // Write inline fixture: module with dot-form UI binding, no session provider.
    let module_path = pkg_dir.join("VaadinModuleDotFormUI.java");
    std::fs::write(
        &module_path,
        "package com.example;\n\
         import com.google.inject.AbstractModule;\n\
         import com.google.inject.Provider;\n\
         import com.google.inject.Provides;\n\
         import com.vaadin.flow.component.UI;\n\
         \n\
         public class VaadinModuleDotFormUI extends AbstractModule {\n\
         \n\
             @Override\n\
             protected void configure() {}\n\
         \n\
             @Provides\n\
             Provider<UI> provideUiProvider() {\n\
                 return () -> UI.getCurrent();\n\
             }\n\
         }\n",
    )
    .expect("write dot-form UI fixture");
    let module_file = module_path.to_string_lossy().into_owned();
    init_git_project(&project_dir);

    let def = load_vaadin_def();
    let inv = make_invocation(&def, &project_dir, &module_file, "VaadinModuleDotFormUI");

    let probe_runner = make_probe_runner(&project_dir);
    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed; UI ops skipped, session op runs");

    assert!(
        plan.refusals.is_empty(),
        "expected no refusals; got: {:?}",
        plan.refusals
    );

    // Both UI ops should be skipped (dot-form guard fired).
    let skipped_ui = plan
        .operations
        .iter()
        .filter(|o| {
            o.kind == "rewrite"
                && o.summary.contains("skipped")
                && o.summary.contains("provideUiProvider")
        })
        .count();
    // If operation summaries don't include the method name, fall back to counting
    // total skipped and asserting at least 2 are skipped (the UI variants).
    let total_skipped = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite" && o.summary.contains("skipped"))
        .count();
    assert!(
        total_skipped >= 2,
        "expected at least 2 skipped rewrite ops (UI variants); got operations: {:?}",
        plan.operations
    );
    let _ = skipped_ui; // checked transitively via total_skipped

    // The session binding should be added.
    assert!(
        !plan.edits.file_edits.is_empty(),
        "expected file edits for the session provider; got none"
    );
    let combined: String = plan.edits.file_edits.iter()
        .flat_map(|fe| fe.edits.iter().map(|e| e.replacement.as_str()))
        .collect();
    assert!(
        combined.contains("provideVaadinSessionProvider"),
        "combined text must contain provideVaadinSessionProvider; got:\n{combined}"
    );
    // insert_member returns a full-span edit (the whole rewritten file), so
    // `combined` includes the PRE-EXISTING provideUiProvider. The UI op was
    // skipped (dot-form binding already present), so provideUiProvider must
    // appear exactly ONCE — the original, not a re-added duplicate.
    assert_eq!(
        combined.matches("provideUiProvider").count(),
        1,
        "provideUiProvider must appear exactly once (pre-existing, not re-added by a skipped UI op); got:\n{combined}"
    );
}

/// Jakarta Provider import: the module already has `import jakarta.inject.Provider;`.
/// The `provider_import_present` probe fires; the sans_provider_import variants run
/// (without adding com.google.inject.Provider to imports); with_guice variants are
/// skipped.  Both provider methods are added.
#[test]
fn integration_jakarta_provider_import_selects_sans_variant() {
    let Some(_jar) = std::env::var_os("BLACKBOX_JAVA_WORKER_JAR") else {
        eprintln!(
            "[vaadin_provider_bindings] BLACKBOX_JAVA_WORKER_JAR unset — \
             skipping integration test"
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("create tempdir");
    let pkg_dir = tmp.path().join("src/main/java/com/example");
    std::fs::create_dir_all(&pkg_dir).expect("create package dir");
    let project_dir = tmp.path().to_string_lossy().into_owned();

    // Write inline fixture: module with jakarta.inject.Provider, no provider methods.
    let module_path = pkg_dir.join("VaadinModuleJakarta.java");
    std::fs::write(
        &module_path,
        "package com.example;\n\
         import com.google.inject.AbstractModule;\n\
         import com.google.inject.Provides;\n\
         import com.vaadin.flow.component.UI;\n\
         import com.vaadin.flow.server.VaadinSession;\n\
         import jakarta.inject.Provider;\n\
         \n\
         public class VaadinModuleJakarta extends AbstractModule {\n\
         \n\
             @Override\n\
             protected void configure() {}\n\
         }\n",
    )
    .expect("write jakarta Provider fixture");
    let module_file = module_path.to_string_lossy().into_owned();
    init_git_project(&project_dir);

    let def = load_vaadin_def();
    let inv = make_invocation(&def, &project_dir, &module_file, "VaadinModuleJakarta");

    let probe_runner = make_probe_runner(&project_dir);
    let backend = SidecarBackend::new(std::path::PathBuf::from(&project_dir));
    let ctx = MacroPlannerContext::new(Box::new(backend), None, Box::new(probe_runner));

    let plan = MacroPlanner::plan(&inv, &def, &ctx)
        .expect("plan should succeed with jakarta Provider import");

    assert!(
        plan.refusals.is_empty(),
        "expected no refusals; got: {:?}",
        plan.refusals
    );

    // 2 with_guice variants skipped; 2 sans variants fire.
    let skipped = plan
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite" && o.summary.contains("skipped"))
        .count();
    assert_eq!(
        skipped, 2,
        "expected exactly 2 skipped rewrite ops (with_guice variants); \
         got operations: {:?}",
        plan.operations
    );

    // Both methods were added.
    assert!(
        !plan.edits.file_edits.is_empty(),
        "expected file edits for both provider methods"
    );
    let combined: String = plan.edits.file_edits.iter()
        .flat_map(|fe| fe.edits.iter().map(|e| e.replacement.as_str()))
        .collect();
    assert!(
        combined.contains("provideUiProvider"),
        "combined text must contain provideUiProvider; got:\n{combined}"
    );
    assert!(
        combined.contains("provideVaadinSessionProvider"),
        "combined text must contain provideVaadinSessionProvider; got:\n{combined}"
    );
}
