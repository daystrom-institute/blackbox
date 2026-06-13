//! `java.*` — the mechanical Java toolbox exposed as transform bindings
//! (design/bro-harness/refactor-v2-pressure-test.md §6.5).
//!
//! A *transform binding* is the `lsp.rename` shape generalized: an authority
//! that runs a hard Rust analysis + templated edit synthesis and returns
//! hash-anchored `{changes, creates, findings}` for the edits algebra — it
//! never writes. The v1 planners (`bbox_refactor`'s Java catalog) already
//! compute exactly this; the port strips the MCP envelope and the plan/apply
//! orchestration, keeps the analysis and synthesis verbatim. Selection
//! (which class, which methods) lives in the cell; refusals come back as
//! operator-actionable errors naming the exact fix (e.g. fields to add to
//! `moveFields`).
//!
//! Surface economics (§6.5): the namespace description is a compact index —
//! one line per transform — and `java.describe` returns the full contract
//! (params, findings vocabulary, an example) at runtime, values staying in
//! the isolate. Provenance: tree-sitter-backed transforms author at the
//! `syntax_only` tier (no ledger issuance — that tier is the floor anyway);
//! `lsp_verified` Java kinds wait on jdtls in bro-lsp (v2 §7's named gate).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// DI policy lives in this binding (the layer above the engine), not in
/// `bbox_refactor`'s extract synthesis, which stays framework-neutral by
/// charter. If the source class is Guice-managed (uses `@Inject`), the
/// extracted delegate should ALSO be container-constructed so it remains
/// interceptable by Guice AOP (`bindInterceptor`) — a `new`-ed delegate is
/// invisible to method interception. Returns the `@Inject` annotation FQN to
/// thread onto the generated target ctor + delegate field, matching the flavor
/// the source already imports. `None` ⇒ not DI-managed (stays own_construction).
fn detect_inject_fqn(source: &str) -> Option<String> {
    if !source.contains("@Inject") {
        return None;
    }
    for fqn in [
        "com.google.inject.Inject",
        "jakarta.inject.Inject",
        "javax.inject.Inject",
    ] {
        if source.contains(&format!("import {fqn};")) {
            return Some(fqn.to_string());
        }
    }
    // `@Inject` present but no recognized single-type import (wildcard import,
    // or the annotation arrives via a star import) — default to the Guice flavor.
    Some("com.google.inject.Inject".to_string())
}

/// Workspace-relative form of a plan-emitted absolute path, tolerant of the
/// canonicalized-root mismatch on symlinked tempdirs (same fallback as
/// lsp_facts).
fn relativize(root: &Path, path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if let Ok(rel) = p.strip_prefix(root) {
        return Ok(rel.to_string_lossy().to_string());
    }
    if let Ok(canon) = root.canonicalize()
        && let Ok(rel) = p.strip_prefix(&canon)
    {
        return Ok(rel.to_string_lossy().to_string());
    }
    Err(format!("plan touches `{path}` outside the worktree root"))
}

/// `java.extractClass` — extract methods/fields from a Java class into a new
/// delegate class, with capture analysis and source-side wiring.
pub struct JavaExtractClass;

#[derive(Deserialize)]
struct ExtractClassParams {
    file: String,
    target: String,
    #[serde(rename = "delegateField", alias = "delegate_field")]
    delegate_field: String,
    methods: Vec<String>,
    #[serde(default, rename = "moveFields", alias = "move_fields")]
    move_fields: Option<Vec<String>>,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
    /// "own_construction" (default) | "external_injection" | "none".
    #[serde(default)]
    wiring: Option<String>,
    /// Keep thin delegating wrappers on the source for the moved methods, so
    /// external callers keep compiling (v1 `source_delegate_wrappers`).
    #[serde(default, alias = "keepPublicApi")]
    wrappers: Option<bool>,
}

#[async_trait]
impl Tool for JavaExtractClass {
    fn name(&self) -> &str {
        "java.extractClass"
    }
    fn description(&self) -> &str {
        "Extract named methods (and optionally fields) from the first class in a Java file into a new delegate class. Runs capture/external-call analysis and synthesizes both sides (new class file + source-side delegate wiring). Returns hash-anchored {changes, creates, findings} for the edits algebra — never writes. Refusals (e.g. extracted code writing a mutable un-moved field) are errors naming the exact fix."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative Java source file holding the class to extract from." },
                "target": { "type": "string", "description": "Workspace-relative path for the NEW class file (must not exist)." },
                "delegateField": { "type": "string", "description": "Field name for the delegate instance on the source class." },
                "methods": { "type": "array", "items": { "type": "string" }, "description": "Method names to move to the new class." },
                "moveFields": { "type": "array", "items": { "type": "string" }, "description": "Field names to move with the methods (mutable fields written by extracted code MUST be listed here)." },
                "className": { "type": "string", "description": "Name for the new class (default: derived from target filename)." },
                "wiring": { "type": "string", "enum": ["own_construction", "external_injection", "none"], "description": "How the source obtains the delegate. AUTO-SELECTED from the source — leave unset: a Guice/DI source (@Inject) gets external_injection (delegate stays container-managed + AOP-interceptable); a non-DI source gets own_construction. Set only to force a choice." },
                "wrappers": { "type": "boolean", "description": "Keep thin delegating wrappers for the moved methods on the source class, preserving its public API. Pass true whenever callers OUTSIDE this file use the moved methods." }
            },
            "required": ["file", "target", "delegateField", "methods"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "extractClass".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ExtractClassParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.extractClass: bad input — expected {{ file, target, delegateField, methods: string[], moveFields?, className?, wiring? }}; {e}"
                ));
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let mut plan_input = json!({
                "kind": "extract_java_class",
                "source": params.file,
                "target": params.target,
                "project_dir": root.to_string_lossy(),
                "item_names": params.methods,
                "delegate_field": params.delegate_field,
            });
            if let Some(fields) = &params.move_fields {
                plan_input["move_fields"] = json!(fields);
            }
            if let Some(name) = &params.class_name {
                plan_input["module_name"] = json!(name);
            }
            // Wiring policy. A Guice-managed source defaults to
            // external_injection so the delegate is itself a container-
            // constructed (and therefore AOP-interceptable) bean; non-DI
            // sources keep own_construction. An explicit `wiring` always wins.
            // Only genuine injected dependencies become the target's @Inject
            // ctor params — the engine threads ONLY moved fields initialized
            // from a surviving ctor parameter, so mutable view-state fields and
            // constants move as plain fields, never as bogus injection points.
            // The delegate is left UNSCOPED (no @Singleton): Guice JIT-binds a
            // concrete @Inject class fresh per injection point, matching the
            // source view's per-instance lifecycle so moved mutable state never
            // leaks across instances.
            let source_text =
                std::fs::read_to_string(root.join(&params.file)).unwrap_or_default();
            let inject_fqn = detect_inject_fqn(&source_text);
            let effective_wiring = params.wiring.clone().unwrap_or_else(|| {
                if inject_fqn.is_some() {
                    "external_injection".to_string()
                } else {
                    "own_construction".to_string()
                }
            });
            match effective_wiring.as_str() {
                "external_injection" => {
                    let inject =
                        inject_fqn.unwrap_or_else(|| "com.google.inject.Inject".to_string());
                    plan_input["wiring_mode"] = json!({
                        "strategy": "external_injection",
                        "target_constructor_annotations": ["@Inject"],
                        "target_constructor_annotation_imports": [inject],
                        "delegate_field_annotations": ["@Inject"],
                        "delegate_field_modifiers": ["private"],
                        "delegate_field_annotation_imports": [inject],
                    });
                }
                other => {
                    plan_input["wiring_mode"] = json!({ "strategy": other });
                }
            }
            if let Some(wrappers) = params.wrappers {
                plan_input["source_delegate_wrappers"] = json!(wrappers);
            }
            let plan_params: bbox_refactor::RefactorPlanParams =
                match serde_json::from_value(plan_input) {
                    Ok(p) => p,
                    Err(e) => return err(format!("java.extractClass: internal param shape: {e}")),
                };
            // The v1 planner verbatim: analysis + synthesis, no LSP context,
            // no writes. Refusals surface as operator-actionable errors.
            let plan_json = match bbox_refactor::plan(&plan_params) {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("{e:#}");
                    // probe-pg-1: a re-call after a successful apply hits the
                    // planner's target-exists refusal; without a hint the
                    // agent shell-deletes the created file and loops.
                    let hint = if msg.contains("missing or empty target") {
                        " — if a prior cell already applied this extraction, the work is DONE (verify with code.items on the source file); re-calling the transform is only valid against a clean target. store() the transform result when you need it in later cells."
                    } else {
                        ""
                    };
                    return err(format!("java.extractClass: {msg}{hint}"));
                }
            };
            let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
                Ok(p) => p,
                Err(e) => return err(format!("java.extractClass: plan decode: {e}")),
            };
            if plan.plan_status != bbox_refactor::PlanStatus::Planned {
                return err(format!(
                    "java.extractClass: planner returned {:?} — {}",
                    plan.plan_status,
                    plan.leftovers.join("; ")
                ));
            }

            // FileEdits → hash-anchored span changes (the edits.merge shape).
            // The v1 planner emits NEW files as whole-content inserts against
            // the empty-file hash (its apply created missing files); the
            // algebra's stale_span check would bounce those, so they convert
            // to creates — the shape edits.createFile consumes.
            let empty_sha = bbox_refactor::sha256_hex(&[]);
            let mut changes: Vec<Value> = Vec::new();
            let mut creates: Vec<Value> = Vec::new();
            for file_edit in &plan.edits {
                let rel = match relativize(&root, &file_edit.path) {
                    Ok(r) => r,
                    Err(e) => return err(format!("java.extractClass: {e}")),
                };
                let is_new_file = file_edit.original_sha256 == empty_sha
                    && file_edit
                        .edits
                        .iter()
                        .all(|e| e.byte_start == 0 && e.byte_end == 0);
                if is_new_file {
                    let content: String = file_edit
                        .edits
                        .iter()
                        .map(|e| e.replacement.as_str())
                        .collect();
                    creates.push(json!({ "path": rel, "content": content }));
                    continue;
                }
                for edit in &file_edit.edits {
                    changes.push(json!({
                        "span": {
                            "file": rel,
                            "byte_start": edit.byte_start,
                            "byte_end": edit.byte_end,
                            "content_sha256": file_edit.original_sha256,
                        },
                        "new_text": edit.replacement,
                    }));
                }
            }
            for create in &plan.file_creates {
                let rel = match relativize(&root, &create.path) {
                    Ok(r) => r,
                    Err(e) => return err(format!("java.extractClass: {e}")),
                };
                creates.push(json!({ "path": rel, "content": create.content }));
            }

            // The v1 analysis structs ARE the findings vocabulary
            // (pressure-test §4) — re-keyed under one array, fields verbatim.
            let mut findings: Vec<Value> = Vec::new();
            for c in &plan.captured_variables {
                let mut f = serde_json::to_value(c).unwrap_or_default();
                f["finding"] = json!("captured_variable");
                findings.push(f);
            }
            for c in &plan.external_calls {
                let mut f = serde_json::to_value(c).unwrap_or_default();
                f["finding"] = json!("external_call");
                findings.push(f);
            }
            for c in &plan.inherited_dependencies {
                let mut f = serde_json::to_value(c).unwrap_or_default();
                f["finding"] = json!("inherited_dependency");
                findings.push(f);
            }
            for c in &plan.remaining_source_accessors {
                let mut f = serde_json::to_value(c).unwrap_or_default();
                f["finding"] = json!("remaining_source_accessor");
                findings.push(f);
            }
            for note in &plan.leftovers {
                findings.push(json!({ "finding": "note", "detail": note }));
            }

            let fixme_count = plan
                .fixme_count
                .as_ref()
                .map(|f| f.plan_only + f.warning)
                .unwrap_or(0);
            ToolResult::Json(json!({
                "title": plan.title,
                "changes": changes,
                "creates": creates,
                "findings": findings,
                "fixme_count": fixme_count,
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

/// `java.describe` — depth-on-demand contract for one transform (§6.5
/// surface economics: the namespace index stays one line per transform;
/// the full contract lives here, in the isolate, not in the exec prompt).
pub struct JavaDescribe;

const EXTRACT_CLASS_CONTRACT: &str = r#"java.extractClass — extract methods/fields from a Java class into a new delegate class.

PARAMS
  file: string            workspace-relative .java file; the FIRST class declaration is the source class
  target: string          workspace-relative path for the new class file (bounces at apply if it exists)
  delegateField: string   delegate field name added to the source class
  methods: string[]       method names to move (selection is yours — inspect with code.items/code.query first)
  moveFields?: string[]   fields to move with them. REQUIRED for any mutable field the moved code WRITES
  className?: string      new class name (default: target filename)
  wiring?: "own_construction" | "external_injection" | "none"
                          AUTO-SELECTED from the source — usually LEAVE UNSET. A Guice/DI-managed
                          source (uses @Inject) defaults to external_injection so the delegate is
                          itself a container-constructed, @Inject, UNSCOPED bean — it stays
                          interceptable by Guice AOP (a `new`-ed delegate is not). A non-DI source
                          defaults to own_construction. Set explicitly only to force a choice:
                          own_construction: private final field + `new <Class>(...)` in the source ctor
                                            (delegate is NOT container-managed — loses Guice AOP)
                          external_injection: @Inject delegate field; the container constructs it and
                                            injects the moved deps as the delegate's @Inject ctor params
                          none: no source-side wiring at all
  wrappers?: boolean      keep thin delegating wrappers for the moved methods on the source class,
                          preserving its public API. SURVEY CALLERS FIRST: if any file outside the
                          source calls a moved method, pass wrappers: true or their compile breaks.
                          Caller survey is one call: code.query({ files: (await code.files({ language: "java" })).files.map(f => f.file),
                          query: "(method_invocation name: (identifier) @call)" }) then filter @call by method name.

RETURNS { title, changes, creates, findings, fixme_count, provenance }
  changes:  hash-anchored {span, new_text}[] → edits.merge
  creates:  {path, content}[]               → edits.createFile (one call each)
  findings: analysis facts, each tagged with `finding`:
    captured_variable     source fields the moved code reads — non-moved ones become constructor params;
                          source_mutable/source_static_final classify the promotion
    external_call         calls to source-class methods NOT in the moved set; recommended_resolution is one of
                          cross_class_static_call | add_to_item_names | add_to_callback_externals |
                          inject_source_instance | drop_the_call
    inherited_dependency  calls resolving to a superclass/interface method
    remaining_source_accessor  source-side accesses to moved fields that survive extraction
    note                  planner prose (synthesis decisions, conservative refusal context)
  fixme_count: number of FIXME markers in the synthesized text — 0 means clean synthesis

ERRORS (operator-actionable, fix and re-call)
  mutable_capture_with_write: extracted code writes mutable source field(s) — add them to moveFields
  invalid selection: a named method/field does not exist in the source class
  target file exists: a prior cell already applied this extraction — the work is done; verify with
                      code.items instead of re-calling. The transform is NOT idempotent over its own output.

RECIPE (one cell; locals do NOT survive across cells — store() anything you need later)
  const r = await java.extractClass({ file, target, delegateField: "pricing",
                                      methods: ["price", "discount"], wrappers: true });
  store("xc", { findings: r.findings, files: r.creates.map(c => c.path) });  // survives cell death
  const es = await edits.begin();
  for (const c of r.creates) await edits.createFile({ es, path: c.path, content: c.content });
  await edits.merge({ es, changes: r.changes });
  const applied = await edits.apply({ es });   // tree-sitter validates both files; bounces roll back
  // then compile-gate via shell (e.g. ./gradlew :module:compileJava) and report"#;

#[async_trait]
impl Tool for JavaDescribe {
    fn name(&self) -> &str {
        "java.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one java.* transform (params, findings vocabulary, recipe). The namespace index lists transforms one line each; call this before first use of a transform."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "transform": { "type": "string", "description": "Transform name, e.g. \"extractClass\"." }
            },
            "required": ["transform"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "describe".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let transform = input
            .get("transform")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match transform {
            "extractClass" => ToolResult::Json(json!({ "contract": EXTRACT_CLASS_CONTRACT })),
            "removeUnusedConstructorParams" => {
                ToolResult::Json(json!({ "contract": REMOVE_UNUSED_PARAMS_CONTRACT }))
            }
            other => err(format!(
                "java.describe: unknown transform `{other}` (available: extractClass, removeUnusedConstructorParams)"
            )),
        }
    }
}

const REMOVE_UNUSED_PARAMS_CONTRACT: &str = r#"java.removeUnusedConstructorParams — drop dead @Inject constructor parameters (move the injection point).

WHAT IT DOES
  Finds parameters of the first class's @Inject constructor that have ZERO references
  in the constructor body, and returns ONE change replacing the parameter list with
  the kept params. This is the cleanup that fully MOVES the injection point: after
  extractClass relocates a dependency's field + usage to a delegate, the dependency's
  ctor parameter is left dead on the source — this drops it.

WHY @Inject only
  A parameter is scoped to the ctor body, so "unused" is decided locally (no whole-class
  scan). Dropping a param is safe ONLY for a container-constructed (@Inject) ctor — it has
  no manual `new Source(...)` callers to break. A non-@Inject ctor is refused with a note.

ORDERING (important)
  Run this AFTER you have APPLIED the extract. The orphaned `this.dep = dep` assignment
  must already be gone, otherwise the param still reads as referenced and is kept. The
  composition is: extractClass → edits.apply → removeUnusedConstructorParams → edits.apply.

PARAMS  { file: string }   workspace-relative .java file
RETURNS { changes, ctor_is_inject, removed: string[], kept: string[], findings, note, provenance }
  changes: [] when nothing is removable (see note); otherwise one span→new_text → edits.merge
  removed/kept: parameter names; findings: { finding:"removed_param", name, type } each
  note: present when no edit (e.g. "no @Inject constructor", "no unused constructor parameters")

RECIPE
  // after the extract has been applied to `file`:
  const r = await java.removeUnusedConstructorParams({ file });
  if (r.changes.length) {
    const es = await edits.begin();
    await edits.merge({ es, changes: r.changes });
    await edits.apply({ es });
  } else { text(r.note); }"#;

/// `java.removeUnusedConstructorParams` — drop `@Inject` constructor parameters
/// left dead by a structural move (the injection-point cleanup that composes
/// after extractClass). Returns hash-anchored `{changes}` for the edits algebra.
pub struct JavaRemoveUnusedCtorParams;

#[derive(Deserialize)]
struct RemoveUnusedParams {
    file: String,
}

#[async_trait]
impl Tool for JavaRemoveUnusedCtorParams {
    fn name(&self) -> &str {
        "java.removeUnusedConstructorParams"
    }
    fn description(&self) -> &str {
        "Drop constructor parameters that are no longer referenced in the @Inject constructor body — the cleanup that fully MOVES the injection point after an extract strands a dependency's parameter. Returns one hash-anchored change replacing the parameter list (→ edits.merge); never writes. Only an @Inject (container-constructed) ctor is eligible — a manually-called ctor's `new` callers would break, so it refuses with a note. Run it AFTER applying the extract (the orphaned `this.dep = dep` must already be gone for the param to read as unused)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative Java file whose first class's @Inject constructor to prune." }
            },
            "required": ["file"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some((
            "java".to_string(),
            "removeUnusedConstructorParams".to_string(),
        ))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: RemoveUnusedParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.removeUnusedConstructorParams: bad input — expected {{ file: string }}; {e}"
                ));
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let abs = root.join(&params.file);
            let plan = match bbox_refactor::analyze_unused_constructor_params(&abs) {
                Ok(p) => p,
                Err(e) => return err(format!("java.removeUnusedConstructorParams: {e:#}")),
            };
            let mut changes: Vec<Value> = Vec::new();
            if let Some((byte_start, byte_end, replacement)) = &plan.edit {
                changes.push(json!({
                    "span": {
                        "file": params.file,
                        "byte_start": byte_start,
                        "byte_end": byte_end,
                        "content_sha256": plan.source_sha256,
                    },
                    "new_text": replacement,
                }));
            }
            let findings: Vec<Value> = plan
                .removed
                .iter()
                .map(|(name, type_name)| {
                    json!({ "finding": "removed_param", "name": name, "type": type_name })
                })
                .collect();
            ToolResult::Json(json!({
                "changes": changes,
                "ctor_is_inject": plan.ctor_is_inject,
                "removed": plan.removed.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                "kept": plan.kept,
                "findings": findings,
                "note": plan.note,
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

/// The `java.*` binding set.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(JavaExtractClass) as Arc<dyn Tool>,
        Arc::new(JavaRemoveUnusedCtorParams) as Arc<dyn Tool>,
        Arc::new(JavaDescribe) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
/// Deliberately a compact INDEX (§6.5 surface economics): one line per
/// transform; depth on demand via java.describe.
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "java".to_string(),
        description: "Java transform authorities (tree-sitter-backed; provenance syntax_only). Each transform runs real capture/wiring analysis host-side and returns {changes, creates, findings} for the edits algebra — never writes. Call java.describe({transform}) for the full contract before first use. Transforms: extractClass — move methods/fields from a class into a new delegate class with source-side wiring (DI sources auto-wire external_injection so the delegate stays AOP-interceptable); removeUnusedConstructorParams — drop dead @Inject ctor params after an extract (move the injection point); composes after extractClass+apply."
            .to_string(),
        declarations: r#"type JavaTransformResult = { title: string; changes: SpanChange[]; creates: { path: string; content: string }[]; findings: ({ finding: string } & Record<string, unknown>)[]; fixme_count: number; provenance: "syntax_only" };
declare const java: {
  /** Full contract (params, findings vocabulary, recipe) for one transform. Call before first use. */
  describe(args: { transform: string }): Promise<{ contract: string }>;
  /** Extract methods/fields into a new delegate class. changes → edits.merge, creates → edits.createFile, then edits.apply. Pass wrappers: true to keep delegating stubs on the source (REQUIRED when callers outside the file use the moved methods — survey first). `wiring` auto-selects (Guice/DI source → external_injection, AOP-interceptable) — leave unset. Refusals are errors naming the exact fix. */
  extractClass(args: { file: string; target: string; delegateField: string; methods: string[]; moveFields?: string[]; className?: string; wiring?: "own_construction" | "external_injection" | "none"; wrappers?: boolean }): Promise<JavaTransformResult>;
  /** Drop dead @Inject ctor params left by an extract (move the injection point). Returns {changes} → edits.merge. Run AFTER applying the extract. @Inject ctors only; refuses others with a note. */
  removeUnusedConstructorParams(args: { file: string }): Promise<{ changes: SpanChange[]; ctor_is_inject: boolean; removed: string[]; kept: string[]; findings: ({ finding: string } & Record<string, unknown>)[]; note: string | null; provenance: "syntax_only" }>;
};"#
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as StdBTreeMap;
    use std::sync::Mutex as StdMutex;

    fn cx_in(dir: &Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(StdMutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(StdMutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(StdMutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(StdBTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    const FIXTURE: &str = r#"package com.acme;

public class OrderService {
    private final double taxRate;
    private int counter;

    public OrderService(double taxRate) {
        this.taxRate = taxRate;
        this.counter = 0;
    }

    public double price(double base) {
        return base * (1.0 + taxRate);
    }

    public double discount(double base, double pct) {
        return price(base) * (1.0 - pct);
    }

    public void track() {
        counter += 1;
    }

    public int counted() {
        return counter;
    }
}
"#;

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_class_returns_changes_creates_and_findings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(root.join("src/com/acme/OrderService.java"), FIXTURE).unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/com/acme/OrderService.java",
                        "target": "src/com/acme/OrderPricing.java",
                        "delegateField": "pricing",
                        "methods": ["price", "discount"],
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["provenance"], "syntax_only", "{result}");
        assert!(
            !result["changes"].as_array().unwrap().is_empty(),
            "{result}"
        );
        let creates = result["creates"].as_array().unwrap();
        assert_eq!(creates.len(), 1, "{result}");
        assert_eq!(creates[0]["path"], "src/com/acme/OrderPricing.java");
        assert!(
            creates[0]["content"]
                .as_str()
                .unwrap()
                .contains("class OrderPricing"),
            "{result}"
        );
        // taxRate is captured (read by price) → a finding, classified.
        let findings = result["findings"].as_array().unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f["finding"] == "captured_variable" && f["name"] == "taxRate"),
            "{findings:?}"
        );
        // Spans are workspace-relative and hash-anchored.
        let span = &result["changes"][0]["span"];
        assert_eq!(span["file"], "src/com/acme/OrderService.java");
        assert_eq!(
            span["content_sha256"],
            bbox_refactor::sha256_hex(FIXTURE.as_bytes()),
            "{span}"
        );
    }

    // A Guice-managed source (uses `@Inject`) auto-defaults to
    // external_injection so the extracted delegate is itself container-
    // constructed and therefore AOP-interceptable. The moved injected dep
    // becomes the target's @Inject ctor param; the source receives the delegate
    // by injection (no `new`).
    const DI_FIXTURE: &str = "package com.acme;\n\
         import com.google.inject.Inject;\n\
         class OrderService {\n\
        \x20   private final Repo repo;\n\
        \x20   @Inject\n\
        \x20   OrderService(Repo repo) { this.repo = repo; }\n\
        \x20   void save() { repo.write(); }\n\
         }\n";

    #[tokio::test]
    async fn di_source_defaults_to_external_injection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(root.join("src/com/acme/OrderService.java"), DI_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/com/acme/OrderService.java",
                        "target": "src/com/acme/OrderWriter.java",
                        "delegateField": "writer",
                        "methods": ["save"],
                        "moveFields": ["repo"],
                    }),
                    &cx,
                )
                .await,
        );
        // Target is a container-constructed @Inject bean taking the moved dep.
        let target = result["creates"][0]["content"].as_str().unwrap();
        assert!(
            target.contains("import com.google.inject.Inject;"),
            "target imports the source's Inject flavor: {target}"
        );
        assert!(
            target.contains("@Inject") && target.contains("OrderWriter(Repo repo)"),
            "target ctor must be @Inject and take the moved dep: {target}"
        );
        // Source receives the delegate by injection (no `new`).
        let source_new_text: String = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect();
        assert!(
            source_new_text.contains("@Inject") && source_new_text.contains("writer"),
            "source delegate field must be @Inject-injected: {source_new_text}"
        );
        assert!(
            !source_new_text.contains("new OrderWriter"),
            "DI source must NOT new up the delegate (defeats Guice AOP): {source_new_text}"
        );
    }

    #[tokio::test]
    async fn explicit_own_construction_overrides_di_default() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(root.join("src/com/acme/OrderService.java"), DI_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/com/acme/OrderService.java",
                        "target": "src/com/acme/OrderWriter.java",
                        "delegateField": "writer",
                        "methods": ["save"],
                        "moveFields": ["repo"],
                        "wiring": "own_construction",
                    }),
                    &cx,
                )
                .await,
        );
        let source_new_text: String = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect();
        // Explicit override wins: the source news up the delegate, threading the dep.
        assert!(
            source_new_text.contains("new OrderWriter(repo)"),
            "explicit own_construction must new up the delegate: {source_new_text}"
        );
    }

    #[tokio::test]
    async fn wrappers_keep_delegating_stubs_on_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/OrderService.java"), FIXTURE).unwrap();
        let cx = cx_in(&root);

        let r = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/OrderService.java",
                        "target": "src/OrderPricing.java",
                        "delegateField": "pricing",
                        "methods": ["price", "discount"],
                        "wrappers": true,
                    }),
                    &cx,
                )
                .await,
        );
        // With wrappers, the source-side changes REPLACE method bodies with
        // delegating stubs rather than deleting the methods: the public API
        // survives for external callers (probe-pg-1's discovered need).
        let source_changes: String = r["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["span"]["file"] == "src/OrderService.java")
            .map(|c| c["new_text"].as_str().unwrap_or_default())
            .collect();
        assert!(
            source_changes.contains("pricing.price(") || source_changes.contains("return pricing."),
            "expected delegating wrapper bodies in source changes: {source_changes}"
        );
    }

    #[tokio::test]
    async fn mutable_capture_with_write_is_an_actionable_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/OrderService.java"), FIXTURE).unwrap();
        let cx = cx_in(&root);

        // `track` writes the mutable field `counter` without moving it.
        let result = JavaExtractClass
            .call(
                json!({
                    "file": "src/OrderService.java",
                    "target": "src/Tracking.java",
                    "delegateField": "tracking",
                    "methods": ["track", "counted"],
                }),
                &cx,
            )
            .await;
        match result {
            ToolResult::Error(e) => {
                assert!(e.contains("mutable_capture_with_write"), "{e}");
                assert!(e.contains("counter"), "{e}");
                assert!(e.contains("move_fields") || e.contains("moveFields"), "{e}");
            }
            other => panic!("expected refusal error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn full_recipe_applies_through_the_choke_point() {
        use super::super::edit_algebra::{
            EditStore, EditsApply, EditsBegin, EditsCreateFile, EditsMerge,
        };
        use super::super::ledger::ProvenanceLedger;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(root.join("src/com/acme/OrderService.java"), FIXTURE).unwrap();
        let cx = cx_in(&root);

        let r = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/com/acme/OrderService.java",
                        "target": "src/com/acme/OrderPricing.java",
                        "delegateField": "pricing",
                        "methods": ["price", "discount"],
                    }),
                    &cx,
                )
                .await,
        );

        let store = Arc::new(EditStore::default());
        let ledger = Arc::new(ProvenanceLedger::default());
        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        for create in r["creates"].as_array().unwrap() {
            json_of(
                EditsCreateFile(store.clone())
                    .call(
                        json!({ "es": es, "path": create["path"], "content": create["content"] }),
                        &cx,
                    )
                    .await,
            );
        }
        json_of(
            EditsMerge(store.clone(), ledger)
                .call(json!({ "es": es, "changes": r["changes"] }), &cx)
                .await,
        );
        let applied = json_of(EditsApply(store).call(json!({ "es": es }), &cx).await);
        assert_eq!(applied["applied"], true, "{applied}");
        // Both files written, tree-sitter validated, delegate wired.
        let source = std::fs::read_to_string(root.join("src/com/acme/OrderService.java")).unwrap();
        assert!(source.contains("private final OrderPricing pricing;"), "{source}");
        assert!(source.contains("this.pricing = new OrderPricing(taxRate);"), "{source}");
        assert!(!source.contains("public double price"), "{source}");
        let target = std::fs::read_to_string(root.join("src/com/acme/OrderPricing.java")).unwrap();
        assert!(target.contains("class OrderPricing"), "{target}");
        assert!(target.contains("public double price"), "{target}");
    }

    // probe-pg-2 reported wrapper insertion landing after the class closing
    // brace on a class whose tail is a nested record (a real-world shape).
    // Reproduction fixture: ctor + moved methods + trailing method +
    // nested record at the end of the class body.
    const NESTED_TAIL_FIXTURE: &str = r#"package com.acme;

import java.util.List;

public class AggregationAdmin {
    private final double rate;

    public AggregationAdmin(double rate) {
        super();
        this.rate = rate;
    }

    public void saveThings(final long id, final List<String> things) {
        System.out.println("save " + id + things.size() * rate);
    }

    public void removeThings(final long id) {
        System.out.println("remove " + id);
    }

    public List<String> fetchOther(final long id) {
        return List.of(Long.toString(id));
    }

    public record TagData(
            long id,
            String name) {
    }
}
"#;

    #[tokio::test]
    async fn wrappers_stay_inside_class_with_nested_record_tail() {
        use super::super::edit_algebra::{
            EditStore, EditsApply, EditsBegin, EditsCreateFile, EditsMerge,
        };
        use super::super::ledger::ProvenanceLedger;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/AggregationAdmin.java"), NESTED_TAIL_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let r = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/AggregationAdmin.java",
                        "target": "src/AggregationWriter.java",
                        "delegateField": "writer",
                        "methods": ["saveThings", "removeThings"],
                        "wrappers": true,
                    }),
                    &cx,
                )
                .await,
        );

        let store = Arc::new(EditStore::default());
        let ledger = Arc::new(ProvenanceLedger::default());
        let es = json_of(EditsBegin(store.clone()).call(json!({}), &cx).await)
            .as_str()
            .unwrap()
            .to_string();
        for create in r["creates"].as_array().unwrap() {
            json_of(
                EditsCreateFile(store.clone())
                    .call(
                        json!({ "es": es, "path": create["path"], "content": create["content"] }),
                        &cx,
                    )
                    .await,
            );
        }
        json_of(
            EditsMerge(store.clone(), ledger)
                .call(json!({ "es": es, "changes": r["changes"] }), &cx)
                .await,
        );
        let applied = json_of(EditsApply(store).call(json!({ "es": es }), &cx).await);
        assert_eq!(applied["applied"], true, "{applied}");

        let source = std::fs::read_to_string(root.join("src/AggregationAdmin.java")).unwrap();
        // Wrappers delegate on the source...
        assert!(source.contains("writer.saveThings("), "{source}");
        // ...and live INSIDE the class body: nothing but whitespace may follow
        // the final closing brace.
        let last_brace = source.rfind('}').unwrap();
        assert!(
            source[last_brace + 1..].trim().is_empty(),
            "content after final brace: {:?}",
            &source[last_brace + 1..]
        );
        // The class still parses with the record intact and no stray braces:
        // brace balance must be zero.
        let balance: i64 = source
            .chars()
            .map(|c| match c {
                '{' => 1,
                '}' => -1,
                _ => 0,
            })
            .sum();
        assert_eq!(balance, 0, "unbalanced braces:\n{source}");
    }

    #[tokio::test]
    async fn describe_returns_contract_and_rejects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_in(dir.path());
        let result = json_of(
            JavaDescribe
                .call(json!({ "transform": "extractClass" }), &cx)
                .await,
        );
        assert!(
            result["contract"].as_str().unwrap().contains("moveFields"),
            "{result}"
        );
        let unknown = JavaDescribe
            .call(json!({ "transform": "lombokify" }), &cx)
            .await;
        assert!(
            matches!(unknown, ToolResult::Error(ref e) if e.contains("available: extractClass")),
            "{unknown:?}"
        );
    }

    #[tokio::test]
    async fn remove_unused_constructor_params_drops_dead_inject_param() {
        // Post-extract shape: `repo` is no longer used in the @Inject ctor body
        // (its field + assignment moved to a delegate); `log` is still used.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/S.java"),
            "package com.acme;\n\
             import com.google.inject.Inject;\n\
             class S {\n\
            \x20   private final Logger log;\n\
            \x20   @Inject\n\
            \x20   S(Repo repo, Logger log) { this.log = log; }\n\
            \x20   void use() { log.info(); }\n\
             }\n",
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaRemoveUnusedCtorParams
                .call(json!({ "file": "src/com/acme/S.java" }), &cx)
                .await,
        );
        assert_eq!(result["ctor_is_inject"], true, "{result}");
        assert_eq!(result["removed"], json!(["repo"]), "{result}");
        assert_eq!(result["kept"], json!(["log"]), "{result}");
        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1, "one param-list change: {result}");
        assert_eq!(changes[0]["new_text"], "(Logger log)", "{result}");
        // Hash-anchored to the analyzed source.
        assert!(
            changes[0]["span"]["content_sha256"].as_str().unwrap().len() == 64,
            "{result}"
        );
    }

    #[tokio::test]
    async fn remove_unused_constructor_params_refuses_non_inject() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/S.java"),
            "package com.acme;\nclass S {\n    S(Repo repo) { }\n}\n",
        )
        .unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            JavaRemoveUnusedCtorParams
                .call(json!({ "file": "src/com/acme/S.java" }), &cx)
                .await,
        );
        assert_eq!(result["ctor_is_inject"], false, "{result}");
        assert!(result["changes"].as_array().unwrap().is_empty(), "{result}");
        assert!(
            result["note"].as_str().unwrap().contains("no @Inject constructor"),
            "{result}"
        );
    }
}
