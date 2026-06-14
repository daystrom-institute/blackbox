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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
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

fn build_dependency_projection(
    plan: &bbox_refactor::RefactorPlan,
    move_fields: &[String],
    effective_wiring: &str,
    classification: Option<&bbox_refactor::facts::FileJavaFieldClassificationFacts>,
) -> (Value, Vec<Value>) {
    let moved: BTreeSet<&str> = move_fields.iter().map(String::as_str).collect();
    let fields_by_name: BTreeMap<&str, &bbox_refactor::facts::JavaFieldClassificationFact> =
        classification
            .map(|facts| {
                facts
                    .fields
                    .iter()
                    .map(|field| (field.name.as_str(), field))
                    .collect()
            })
            .unwrap_or_default();

    let external_injection = effective_wiring == "external_injection";
    let mut capture_projections = Vec::new();
    let mut constructor_params = Vec::new();
    let mut non_injectable_params = Vec::new();
    let mut moved_field_names = Vec::new();
    let mut static_final_constants = Vec::new();

    for capture in &plan.captured_variables {
        let field = fields_by_name.get(capture.name.as_str()).copied();
        let field_is_moved = moved.contains(capture.name.as_str());
        let moved_constructor_param = field_is_moved
            && field
                .and_then(|f| f.injection_style.as_deref())
                .is_some_and(|style| style == "constructor_param");
        let route = if capture.source_static_final && !field_is_moved {
            "moved_static_final_constant"
        } else if field_is_moved && moved_constructor_param {
            "moved_field_constructor_param"
        } else if field_is_moved {
            "moved_field"
        } else {
            "captured_constructor_param"
        };
        let target_constructor_param = matches!(
            route,
            "captured_constructor_param" | "moved_field_constructor_param"
        );
        let is_injected = field.map(|f| f.is_injected).unwrap_or(false);
        let is_provider = field.map(|f| f.is_provider).unwrap_or(false);
        let injection_style = field.and_then(|f| f.injection_style.clone());
        let wireability = if !target_constructor_param {
            "not_constructor_param"
        } else {
            match effective_wiring {
                "external_injection" if is_injected => "likely_injectable",
                "external_injection" if is_provider => "provider_binding_review",
                "external_injection" => "non_injectable_capture",
                "none" => "manual_wiring_required",
                _ => "source_argument",
            }
        };
        let risk = if external_injection && target_constructor_param && !is_injected && !is_provider
        {
            Some("non_injectable_capture")
        } else if effective_wiring == "own_construction"
            && target_constructor_param
            && capture.source_mutable
        {
            Some("snapshot_mutable_capture")
        } else {
            None
        };
        let recommendation = match risk {
            Some("non_injectable_capture") => Some(
                "target @Inject constructor will ask the DI container for this captured source field; move the field only if it belongs on the delegate, supply an injectable binding/provider, or choose a seam with injected captures",
            ),
            Some("snapshot_mutable_capture") => Some(
                "own_construction passes the source field value once; move the mutable field or choose a seam that does not snapshot changing source state",
            ),
            _ => None,
        };

        let mut projection = json!({
            "finding": "captured_dependency",
            "name": capture.name,
            "type": capture.source_type,
            "route": route,
            "target_constructor_param": target_constructor_param,
            "wiring": effective_wiring,
            "wireability": wireability,
            "source_mutable": capture.source_mutable,
            "source_static_final": capture.source_static_final,
            "field_is_moved": field_is_moved,
            "is_injected": is_injected,
            "is_provider": is_provider,
        });
        if let Some(style) = injection_style {
            projection["injection_style"] = json!(style);
        }
        if let Some(risk) = risk {
            projection["risk"] = json!(risk);
        }
        if let Some(recommendation) = recommendation {
            projection["recommendation"] = json!(recommendation);
        }
        if let Some(field) = field {
            projection["reads"] = json!(field.reads);
            projection["writes"] = json!(field.writes);
            projection["read_by"] = json!(field.read_by);
            projection["written_by"] = json!(field.written_by);
        }

        if target_constructor_param {
            constructor_params.push(projection.clone());
        }
        if projection.get("risk").and_then(Value::as_str) == Some("non_injectable_capture") {
            non_injectable_params.push(capture.name.clone());
        }
        if field_is_moved {
            moved_field_names.push(capture.name.clone());
        }
        if route == "moved_static_final_constant" {
            static_final_constants.push(capture.name.clone());
        }
        capture_projections.push(projection);
    }

    let projection = json!({
        "wiring": effective_wiring,
        "constructor_param_count": constructor_params.len(),
        "constructor_params": constructor_params,
        "non_injectable_params": non_injectable_params,
        "moved_captured_fields": moved_field_names,
        "static_final_constants": static_final_constants,
        "summary": match effective_wiring {
            "external_injection" => "external_injection means target constructor params must be DI-resolvable; review non_injectable_params before apply",
            "none" => "wiring none leaves target constructor params for the operator to wire manually",
            _ => "own_construction passes constructor params from the source instance; review snapshot_mutable_capture risks before apply",
        },
    });
    (projection, capture_projections)
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

fn resolve_workspace_file(root: &Path, file: &str, tool: &str) -> Result<PathBuf, String> {
    let rel = Path::new(file);
    if rel.is_absolute()
        || rel
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "{tool}: file must be a workspace-relative path without `..`: {file}"
        ));
    }
    Ok(root.join(rel))
}

fn file_edits_to_changes(
    root: &Path,
    tool: &str,
    file_edits: &[bbox_refactor::FileEdit],
) -> Result<(Vec<Value>, Vec<Value>), String> {
    let mut changes = Vec::new();
    let mut changed_files = Vec::new();
    for file_edit in file_edits {
        let rel = relativize(root, &file_edit.path)?;
        if !file_edit.edits.is_empty() {
            let replacement_bytes: usize = file_edit
                .edits
                .iter()
                .map(|edit| edit.replacement.len())
                .sum();
            changed_files.push(json!({
                "path": rel,
                "edit_count": file_edit.edits.len(),
                "replacement_bytes": replacement_bytes,
            }));
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
    if changes.is_empty() {
        tracing::debug!(tool, "java hygiene binding returned no changes");
    }
    Ok((changes, changed_files))
}

#[derive(Deserialize)]
struct JavaFilesParams {
    files: Vec<String>,
}

#[derive(Deserialize)]
struct JavaHygieneParams {
    files: Vec<String>,
    #[serde(default)]
    imports: Option<bool>,
    #[serde(default)]
    whitespace: Option<bool>,
}

#[derive(Deserialize)]
struct JavaExtractMethodCodeBlockParams {
    file: String,
    #[serde(rename = "oldText", alias = "old_text")]
    old_text: String,
    #[serde(
        rename = "methodName",
        alias = "method_name",
        alias = "helperName",
        alias = "helper_name"
    )]
    method_name: String,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default, rename = "newText", alias = "new_text")]
    new_text: Option<String>,
    #[serde(default)]
    parameters: Option<Vec<JavaExtractMethodParam>>,
    #[serde(default)]
    arguments: Option<Vec<String>>,
    #[serde(default, rename = "returnType", alias = "return_type")]
    return_type: Option<String>,
    #[serde(default, rename = "returnVar", alias = "return_var")]
    return_var: Option<String>,
    #[serde(default, rename = "resultRecord", alias = "result_record")]
    result_record: Option<bool>,
    #[serde(default, rename = "resultRecordName", alias = "result_record_name")]
    result_record_name: Option<String>,
    #[serde(default, rename = "resultRecordVar", alias = "result_record_var")]
    result_record_var: Option<String>,
    #[serde(
        default,
        rename = "previewOnly",
        alias = "preview_only",
        alias = "preview"
    )]
    preview_only: Option<bool>,
}

#[derive(Deserialize)]
struct JavaExtractMethodParam {
    #[serde(rename = "type", alias = "typeName", alias = "type_name")]
    type_name: String,
    name: String,
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
    /// Run full analysis/synthesis but omit the heavy edit/create payloads.
    /// Agents use this to inspect findings on risky seams before carrying the
    /// full target text through the isolate.
    #[serde(
        default,
        rename = "previewOnly",
        alias = "preview_only",
        alias = "preview"
    )]
    preview_only: Option<bool>,
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
                "file": { "type": "string", "description": "Path to a Java source file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is." },
                "target": { "type": "string", "description": "Path for the NEW class file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is. Must not exist (bounces at apply)." },
                "delegateField": { "type": "string", "description": "Field name for the delegate instance on the source class." },
                "methods": { "type": "array", "items": { "type": "string" }, "description": "Method names to move to the new class." },
                "moveFields": { "type": "array", "items": { "type": "string" }, "description": "Field names to move with the methods (mutable fields written by extracted code MUST be listed here)." },
                "className": { "type": "string", "description": "Name for the new class (default: derived from target filename)." },
                "wiring": { "type": "string", "enum": ["own_construction", "external_injection", "none"], "description": "How the source obtains the delegate. AUTO-SELECTED from the source — leave unset: a Guice/DI source (@Inject) gets external_injection (delegate stays container-managed + AOP-interceptable); a non-DI source gets own_construction. Set only to force a choice." },
                "wrappers": { "type": "boolean", "description": "Keep thin delegating wrappers for the moved methods on the source class, preserving its public API. Pass true whenever callers OUTSIDE this file use the moved methods." },
                "previewOnly": { "type": "boolean", "description": "Run the full planner but omit heavy change/create payloads. Use this for risky seams to inspect findings and dependency_projection before re-calling without previewOnly to apply." }
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
                    "java.extractClass: bad input — expected {{ file, target, delegateField, methods: string[], moveFields?, className?, wiring?, previewOnly? }}; {e}"
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
            let captured_names: Vec<String> = plan
                .captured_variables
                .iter()
                .map(|capture| capture.name.clone())
                .collect();
            let field_classification = if captured_names.is_empty() {
                None
            } else {
                bbox_refactor::facts::java_field_classification(
                    &root.join(&params.file),
                    Some(&captured_names),
                    None,
                )
                .ok()
            };
            let move_fields = params.move_fields.clone().unwrap_or_default();
            let (dependency_projection, dependency_findings) = build_dependency_projection(
                &plan,
                &move_fields,
                &effective_wiring,
                field_classification.as_ref(),
            );

            // FileEdits → hash-anchored span changes (the edits.merge shape).
            // The v1 planner emits NEW files as whole-content inserts against
            // the empty-file hash (its apply created missing files); the
            // algebra's stale_span check would bounce those, so they convert
            // to creates — the shape edits.createFile consumes.
            let empty_sha = bbox_refactor::sha256_hex(&[]);
            let mut changes: Vec<Value> = Vec::new();
            let mut creates: Vec<Value> = Vec::new();
            let mut would_change_files: Vec<Value> = Vec::new();
            let mut would_create_files: Vec<Value> = Vec::new();
            let preview_only = params.preview_only.unwrap_or(false);
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
                    would_create_files.push(json!({
                        "path": rel,
                        "bytes": content.len(),
                    }));
                    if !preview_only {
                        creates.push(json!({ "path": rel, "content": content }));
                    }
                    continue;
                }
                if !file_edit.edits.is_empty() {
                    let replacement_bytes: usize =
                        file_edit.edits.iter().map(|e| e.replacement.len()).sum();
                    would_change_files.push(json!({
                        "path": rel,
                        "edit_count": file_edit.edits.len(),
                        "replacement_bytes": replacement_bytes,
                    }));
                }
                for edit in &file_edit.edits {
                    if !preview_only {
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
            }
            for create in &plan.file_creates {
                let rel = match relativize(&root, &create.path) {
                    Ok(r) => r,
                    Err(e) => return err(format!("java.extractClass: {e}")),
                };
                would_create_files.push(json!({
                    "path": rel,
                    "bytes": create.content.len(),
                }));
                if !preview_only {
                    creates.push(json!({ "path": rel, "content": create.content }));
                }
            }

            // The v1 analysis structs ARE the findings vocabulary
            // (pressure-test §4) — re-keyed under one array, fields verbatim.
            let mut findings: Vec<Value> = Vec::new();
            for c in &plan.captured_variables {
                let mut f = serde_json::to_value(c).unwrap_or_default();
                f["finding"] = json!("captured_variable");
                findings.push(f);
            }
            findings.extend(dependency_findings);
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
                "dependency_projection": dependency_projection,
                "preview_only": preview_only,
                "would_change_files": would_change_files,
                "would_create_files": would_create_files,
                "fixme_count": fixme_count,
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

/// `java.extractMethodCodeBlock` — extract one contiguous Java statement
/// range into a private helper method.
pub struct JavaExtractMethodCodeBlock;

#[async_trait]
impl Tool for JavaExtractMethodCodeBlock {
    fn name(&self) -> &str {
        "java.extractMethodCodeBlock"
    }
    fn description(&self) -> &str {
        "Extract one exact contiguous Java code block from a method body into a helper method. Thin code-mode binding over extract_java_code_block_to_method: infers captures, arguments, and zero/one return value; refuses mutated captures, unsafe multiple live-outs, and non-local control flow. Returns hash-anchored {changes} for edits.merge — never writes. Run analysis.methodRegions first for contiguity/live-out gates."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative Java source file." },
                "oldText": { "type": "string", "description": "Exact contiguous source text to extract. Must match exactly once." },
                "methodName": { "type": "string", "description": "Name of the new helper method." },
                "className": { "type": "string", "description": "Optional enclosing class name when the file has multiple classes." },
                "visibility": { "type": "string", "enum": ["private", "package-private", "protected", "public"], "description": "Helper visibility. Default private." },
                "newText": { "type": "string", "description": "Optional explicit call-site replacement. Usually omit and let the planner synthesize it." },
                "parameters": { "type": "array", "items": { "type": "object", "properties": { "type": { "type": "string" }, "name": { "type": "string" } }, "required": ["type", "name"] }, "description": "Optional operator override for helper parameters. Omit to infer captures." },
                "arguments": { "type": "array", "items": { "type": "string" }, "description": "Optional argument override aligned with parameters." },
                "returnType": { "type": "string", "description": "Optional return type override. Omit to infer void or one live-out variable." },
                "returnVar": { "type": "string", "description": "Optional return variable name override." },
                "resultRecord": { "type": "boolean", "description": "Opt in to generated nested-record result bundle when the selected block has multiple live-out locals. Default false: multi-live-out blocks still refuse." },
                "resultRecordName": { "type": "string", "description": "Optional generated record type name when resultRecord is true. Default <MethodName>Result." },
                "resultRecordVar": { "type": "string", "description": "Optional call-site local name for the helper result record. Default <methodName>Result." },
                "previewOnly": { "type": "boolean", "description": "Run the planner but omit edit payloads; returns would_change_files and findings." }
            },
            "required": ["file", "oldText", "methodName"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "extractMethodCodeBlock".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: JavaExtractMethodCodeBlockParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.extractMethodCodeBlock: bad input — expected {{ file, oldText, methodName, className?, visibility?, previewOnly? }}; {e}"
                ));
            }
        };
        if params.old_text.trim().is_empty() {
            return err("java.extractMethodCodeBlock: `oldText` must be non-empty");
        }
        if params.method_name.trim().is_empty() {
            return err("java.extractMethodCodeBlock: `methodName` must be non-empty");
        }
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let mut plan_input = json!({
                "kind": "extract_java_code_block_to_method",
                "source": params.file,
                "project_dir": root.to_string_lossy(),
                "old_text": params.old_text,
                "module_name": params.method_name,
            });
            if let Some(class_name) = params.class_name {
                plan_input["impl_name"] = json!(class_name);
            }
            if let Some(visibility) = params.visibility {
                plan_input["visibility"] = json!(visibility);
            }
            if let Some(new_text) = params.new_text {
                plan_input["new_text"] = json!(new_text);
            }
            if let Some(parameters) = params.parameters {
                plan_input["parameters"] = json!(
                    parameters
                        .into_iter()
                        .map(|param| json!({ "type": param.type_name, "name": param.name }))
                        .collect::<Vec<_>>()
                );
            }
            let mut toml_entries = serde_json::Map::new();
            if let Some(arguments) = params.arguments {
                toml_entries.insert("arguments".to_string(), json!(arguments));
            }
            if let Some(return_type) = params.return_type {
                toml_entries.insert("return_type".to_string(), json!(return_type));
            }
            if let Some(return_var) = params.return_var {
                toml_entries.insert("return_var".to_string(), json!(return_var));
            }
            if let Some(result_record) = params.result_record {
                toml_entries.insert("result_record".to_string(), json!(result_record));
            }
            if let Some(result_record_name) = params.result_record_name {
                toml_entries.insert("result_record_name".to_string(), json!(result_record_name));
            }
            if let Some(result_record_var) = params.result_record_var {
                toml_entries.insert("result_record_var".to_string(), json!(result_record_var));
            }
            if !toml_entries.is_empty() {
                plan_input["toml_entries"] = Value::Object(toml_entries);
            }

            let plan_params: bbox_refactor::RefactorPlanParams =
                match serde_json::from_value(plan_input) {
                    Ok(p) => p,
                    Err(e) => {
                        return err(format!(
                            "java.extractMethodCodeBlock: internal param shape: {e}"
                        ));
                    }
                };
            let plan_json = match bbox_refactor::plan(&plan_params) {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("{e:#}");
                    let hint = if msg.contains("multi_return_needs_record") {
                        " — run analysis.methodRegions on this candidate range to inspect live_outs; if each live-out is a real top-level output with an explicit type, re-call with resultRecord: true, otherwise pick a smaller block"
                    } else if msg.contains("result_record_live_out_not_visible") {
                        " — resultRecord can only return locals visible at the helper return site; widen the range to include the declaring scope or pick top-level statements"
                    } else if msg.contains("inferred_capture_parameter_type") {
                        " — Java helper parameters cannot use `var`/inferred types; resolve the captured declaration type and re-call with explicit parameters/arguments"
                    } else if msg.contains("result_record_inferred_type") {
                        " — resultRecord requires explicit Java component types; avoid `var` live-outs or provide a typed boundary manually"
                    } else if msg.contains("mutated_capture") {
                        " — run analysis.methodRegions to see mutated captures before choosing a smaller range"
                    } else if msg.contains("non_local_control_flow") {
                        " — run analysis.methodRegions to locate the return/break/continue gate before mutating"
                    } else {
                        ""
                    };
                    return err(format!("java.extractMethodCodeBlock: {msg}{hint}"));
                }
            };
            let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
                Ok(p) => p,
                Err(e) => return err(format!("java.extractMethodCodeBlock: plan decode: {e}")),
            };
            if plan.plan_status != bbox_refactor::PlanStatus::Planned {
                return err(format!(
                    "java.extractMethodCodeBlock: planner returned {:?} — {}",
                    plan.plan_status,
                    plan.leftovers.join("; ")
                ));
            }
            let (mut changes, changed_files) =
                match file_edits_to_changes(&root, "java.extractMethodCodeBlock", &plan.edits) {
                    Ok(converted) => converted,
                    Err(e) => return err(format!("java.extractMethodCodeBlock: {e}")),
                };
            let preview_only = params.preview_only.unwrap_or(false);
            if preview_only {
                changes.clear();
            }
            let findings = plan
                .leftovers
                .iter()
                .map(|note| json!({ "finding": "note", "detail": note }))
                .collect::<Vec<_>>();
            ToolResult::Json(json!({
                "title": plan.title,
                "changes": changes,
                "findings": findings,
                "preview_only": preview_only,
                "would_change_files": changed_files,
                "fixme_count": plan.fixme_count.as_ref().map(|f| f.plan_only + f.warning).unwrap_or(0),
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
  file: string            Path to a .java file. Relative paths resolve against
                          the session worktree root; absolute paths are
                          accepted as-is. The FIRST class declaration is the
                          source class. (Refactor plan OUTPUTS still must
                          land inside the worktree — see transform integrity
                          checks.)
  target: string          Path for the new class file. Relative paths
                          resolve against the session worktree root;
                          absolute paths are accepted as-is. Bounces at
                          apply if it exists. Refactor plan integrity still
                          requires the target to land inside the worktree.
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
  previewOnly?: boolean   run the same planner but return [] for changes/creates and use
                          would_change_files/would_create_files summaries instead. Use this on
                          risky seams to inspect findings + dependency_projection before carrying
                          full edit payloads through the isolate. Re-call without previewOnly to apply.

RETURNS { title, changes, creates, findings, dependency_projection, preview_only,
          would_change_files, would_create_files, fixme_count, provenance }
  changes:  hash-anchored {span, new_text}[] → edits.merge
  creates:  {path, content}[]               → edits.createFile (one call each)
  dependency_projection: compact pre-apply summary of which captured fields will become target
                          constructor params under the selected wiring. For external_injection,
                          non_injectable_params names captures that the DI container is unlikely
                          to resolve without moving the field, supplying a binding/provider, or
                          choosing a cleaner seam.
  findings: analysis facts, each tagged with `finding`:
    captured_variable     source fields the moved code reads — non-moved ones become constructor params;
                          source_mutable/source_static_final classify the promotion
    captured_dependency   per-capture projection: route, target_constructor_param, wireability,
                          injection_style, risk, and recommendation
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

const EXTRACT_METHOD_CODE_BLOCK_CONTRACT: &str = r#"java.extractMethodCodeBlock — extract one contiguous Java code block into a helper method.

WHAT IT DOES
  Thin code-mode binding over the existing extract_java_code_block_to_method
  planner. It extracts one exact contiguous statement range from inside a Java
  method/constructor body, infers captured locals/params as helper parameters,
  and infers void vs one returned live-out variable. Generated helper insertion
  preserves call-site indentation, method spacing, and moved-body relative
  indentation; still run java.hygiene after apply for imports and file-level
  whitespace. If analysis.methodRegions reports `type:"var"` plus
  `resolved_type`, the planner can use that syntax-only type projection. If an
  inferred captured parameter or live-out type remains `var` / unresolved, the
  planner refuses before edits; pass explicit parameters/arguments or
  returnType/returnVar after resolving the declaration type.

PARAMS
  file: string          workspace-relative .java file
  oldText: string       exact contiguous source text to extract; must match once
  methodName: string    new helper method name
  className?: string    optional enclosing class when the file has multiple classes
  visibility?: "private" | "package-private" | "protected" | "public"
                        default private
  newText?: string      explicit call-site replacement; usually omit
  parameters?: Array<{ type: string, name: string }>
                        override inferred helper params; usually omit
  arguments?: string[]  override call-site args aligned with parameters
  returnType?: string   override inferred return type; use when a live-out has
                        type:"var" and no resolved_type
  returnVar?: string    override inferred return variable name
  resultRecord?: boolean
                        opt in to a generated nested record result bundle when
                        the selected block has multiple live-out locals. Default
                        false: multi-live-out blocks still refuse. Requires each
                        live-out to be visible at the helper return site and to
                        have an explicit type (not `var` / inferred).
  resultRecordName?: string
                        generated record type name. Default <MethodName>Result.
  resultRecordVar?: string
                        call-site local name for the helper result record.
                        Default <methodName>Result.
  previewOnly?: boolean run the planner but return [] changes and only summaries/findings

RETURNS { title, changes, findings, preview_only, would_change_files, fixme_count, provenance }
  changes: hash-anchored {span,new_text}[] for edits.merge
  findings: planner notes such as enclosing method/static helper facts
  would_change_files: edit summaries; present in both preview and normal mode

REFUSALS
  error.mutated_capture(name)       selected range mutates a captured local/param
  error.multi_return_needs_record   more than one local declared inside is read after;
                                    re-call with resultRecord:true only when the
                                    live-outs are real top-level outputs
  error.result_record_live_out_not_visible
                                    a requested record component is not visible
                                    at the generated helper return site
  error.result_record_inferred_type requested record component has `var`/inferred type
  error.inferred_capture_parameter_type
                                    inferred helper parameter has `var`/inferred type;
                                    pass explicit parameters/arguments
  error.non_local_control_flow      return/break/continue would cross the extraction boundary
  oldText match failures            selected text must match exactly once

RECIPE
  // First run the gate. Do not skip this on long/monolithic methods.
  const gate = await analysis.methodRegions({
    file, method: "buildView", className: "View",
    ranges: [{ label: "candidate", startLine: 120, endLine: 155 }]
  });
  const r = gate.requested_ranges[0];
  if (!gate.requested_contiguous || !r.extractability.can_extract_with_current_tool) {
    text({ stop_reasons: r.extractability.stop_reasons, live_outs: r.live_outs });
    exit();
  }
  // Then read the exact accepted range and pass it as oldText. If the gate
  // stopped only because of multiple real, explicitly typed top-level live-outs,
  // set resultRecord:true; otherwise omit the resultRecord fields.
  const result = await java.extractMethodCodeBlock({
    file, oldText, methodName: "buildControls", className: "View",
    resultRecord: true, resultRecordName: "BuildControlsResult"
  });
  const es = await edits.begin();
  await edits.merge({ es, changes: result.changes });
  await edits.apply({ es });
  // compile, java.hygiene({ files: [file] }), compile again if hygiene changed
"#;

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
            "extractMethodCodeBlock" => {
                ToolResult::Json(json!({ "contract": EXTRACT_METHOD_CODE_BLOCK_CONTRACT }))
            }
            "removeUnusedConstructorParams" => {
                ToolResult::Json(json!({ "contract": REMOVE_UNUSED_PARAMS_CONTRACT }))
            }
            "organizeImports" => ToolResult::Json(json!({ "contract": ORGANIZE_IMPORTS_CONTRACT })),
            "normalizeWhitespace" => {
                ToolResult::Json(json!({ "contract": NORMALIZE_WHITESPACE_CONTRACT }))
            }
            "hygiene" => ToolResult::Json(json!({ "contract": HYGIENE_CONTRACT })),
            other => err(format!(
                "java.describe: unknown transform `{other}` (available: extractClass, extractMethodCodeBlock, removeUnusedConstructorParams, organizeImports, normalizeWhitespace, hygiene)"
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

PARAMS  { file: string }   Path to a .java file. Relative paths resolve
                              against the session worktree root; absolute
                              paths are accepted as-is.
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

const ORGANIZE_IMPORTS_CONTRACT: &str = r#"java.organizeImports — prune/sort Java imports for touched files.

WHAT IT DOES
  Runs the syntax-only Java import hygiene planner on each workspace-relative
  file. It keeps imports whose simple names are referenced by the AST, prunes
  unused single-type and single-member static imports, preserves wildcard imports,
  and adds uniquely-resolvable project-local type imports for simple names.

WHY SYNTAX-ONLY
  The code-mode binding does not currently carry a JDTLS session handle. This is
  the same conservative heuristic used by extract-class target generation; it
  returns hash-anchored edits for `edits.merge` and never writes.

PARAMS  { files: string[] }   touched/created workspace-relative .java files
RETURNS { changes, changed_files, findings, provenance }
  changes: hash-anchored span changes for edits.merge; [] means no import edits.
  findings: per-file no-change or changed summaries.

RECIPE
  const r = await java.organizeImports({ files: touchedFiles });
  if (r.changes.length) { const es = await edits.begin(); await edits.merge({ es, changes: r.changes }); await edits.apply({ es }); }"#;

const NORMALIZE_WHITESPACE_CONTRACT: &str = r#"java.normalizeWhitespace — conservative Java whitespace hygiene for touched files.

WHAT IT DOES
  Normalizes the small formatting residues common after generated Java
  refactors: package/import/type spacing, excessive blank-line runs, trailing
  whitespace, and one-space indentation drift on common statement/declaration
  lines. It is intentionally not a full Java formatter.

PARAMS  { files: string[] }   touched/created workspace-relative .java files
RETURNS { changes, changed_files, findings, provenance }
  changes: one whole-file hash-anchored change per changed file; [] means clean.

RECIPE
  Run after the semantic transform compiles. If changes are returned, apply them
  and compile again."#;

const HYGIENE_CONTRACT: &str = r#"java.hygiene — post-apply Java hygiene bundle for touched files.

WHAT IT DOES
  Runs import hygiene and whitespace hygiene in-memory per file, then returns at
  most one whole-file change per changed file. This is the routine recipes should
  call after the semantic transform applies and compiles.

PARAMS
  files: string[]       touched/created workspace-relative .java files
  imports?: boolean     default true
  whitespace?: boolean  default true

RETURNS { changes, changed_files, findings, provenance }
  findings includes per-file routines_applied; [] changes means no hygiene edits.

RECIPE
  const h = await java.hygiene({ files: touchedFiles });
  if (h.changes.length) {
    const es = await edits.begin();
    await edits.merge({ es, changes: h.changes });
    await edits.apply({ es });
    // compile again
  }"#;

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
                "file": { "type": "string", "description": "Path to a Java file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is." }
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

pub struct JavaOrganizeImports;

#[async_trait]
impl Tool for JavaOrganizeImports {
    fn name(&self) -> &str {
        "java.organizeImports"
    }
    fn description(&self) -> &str {
        "Post-apply Java import hygiene for touched files. Syntax-only: prunes/sorts imports and adds uniquely-resolvable project-local imports. Returns {changes} for edits.merge; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Touched/created workspace-relative Java files."
                }
            },
            "required": ["files"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "organizeImports".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: JavaFilesParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.organizeImports: bad input — expected {{ files: string[] }}; {e}"
                ));
            }
        };
        if params.files.is_empty() {
            return err("java.organizeImports: `files` must not be empty");
        }
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let mut file_edits = Vec::new();
            let mut findings = Vec::new();
            for file in params.files {
                let abs = match resolve_workspace_file(&root, &file, "java.organizeImports") {
                    Ok(path) => path,
                    Err(e) => return err(e),
                };
                match bbox_refactor::organize_java_imports(&root, &abs) {
                    Ok(mut edits) if !edits.is_empty() => {
                        findings.push(json!({
                            "finding": "imports_changed",
                            "file": file,
                            "edit_count": edits.iter().map(|e| e.edits.len()).sum::<usize>(),
                        }));
                        file_edits.append(&mut edits);
                    }
                    Ok(_) => findings.push(json!({
                        "finding": "no_import_changes",
                        "file": file,
                    })),
                    Err(e) => return err(format!("java.organizeImports: {file}: {e:#}")),
                }
            }
            let (changes, changed_files) =
                match file_edits_to_changes(&root, "java.organizeImports", &file_edits) {
                    Ok(converted) => converted,
                    Err(e) => return err(format!("java.organizeImports: {e}")),
                };
            ToolResult::Json(json!({
                "changes": changes,
                "changed_files": changed_files,
                "findings": findings,
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

pub struct JavaNormalizeWhitespace;

#[async_trait]
impl Tool for JavaNormalizeWhitespace {
    fn name(&self) -> &str {
        "java.normalizeWhitespace"
    }
    fn description(&self) -> &str {
        "Conservative post-apply Java whitespace hygiene for touched files: package/import/type spacing, excessive blank lines, trailing whitespace, and one-space indentation drift. Returns {changes} for edits.merge; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Touched/created workspace-relative Java files."
                }
            },
            "required": ["files"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "normalizeWhitespace".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: JavaFilesParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.normalizeWhitespace: bad input — expected {{ files: string[] }}; {e}"
                ));
            }
        };
        if params.files.is_empty() {
            return err("java.normalizeWhitespace: `files` must not be empty");
        }
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let mut file_edits = Vec::new();
            let mut findings = Vec::new();
            for file in params.files {
                let abs = match resolve_workspace_file(&root, &file, "java.normalizeWhitespace") {
                    Ok(path) => path,
                    Err(e) => return err(e),
                };
                match bbox_refactor::normalize_java_whitespace_file(&abs) {
                    Ok(mut edits) if !edits.is_empty() => {
                        findings.push(json!({
                            "finding": "whitespace_changed",
                            "file": file,
                            "edit_count": edits.iter().map(|e| e.edits.len()).sum::<usize>(),
                        }));
                        file_edits.append(&mut edits);
                    }
                    Ok(_) => findings.push(json!({
                        "finding": "no_whitespace_changes",
                        "file": file,
                    })),
                    Err(e) => return err(format!("java.normalizeWhitespace: {file}: {e:#}")),
                }
            }
            let (changes, changed_files) =
                match file_edits_to_changes(&root, "java.normalizeWhitespace", &file_edits) {
                    Ok(converted) => converted,
                    Err(e) => return err(format!("java.normalizeWhitespace: {e}")),
                };
            ToolResult::Json(json!({
                "changes": changes,
                "changed_files": changed_files,
                "findings": findings,
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

pub struct JavaHygiene;

#[async_trait]
impl Tool for JavaHygiene {
    fn name(&self) -> &str {
        "java.hygiene"
    }
    fn description(&self) -> &str {
        "Routine post-apply Java hygiene bundle for touched files. Runs import hygiene and conservative whitespace hygiene in-memory, returns at most one whole-file {changes} entry per changed file for edits.merge; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Touched/created workspace-relative Java files."
                },
                "imports": {
                    "type": "boolean",
                    "description": "Run import hygiene. Default true."
                },
                "whitespace": {
                    "type": "boolean",
                    "description": "Run whitespace hygiene. Default true."
                }
            },
            "required": ["files"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("java".to_string(), "hygiene".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: JavaHygieneParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "java.hygiene: bad input — expected {{ files: string[], imports?: boolean, whitespace?: boolean }}; {e}"
                ));
            }
        };
        if params.files.is_empty() {
            return err("java.hygiene: `files` must not be empty");
        }
        let imports = params.imports.unwrap_or(true);
        let whitespace = params.whitespace.unwrap_or(true);
        if !imports && !whitespace {
            return err("java.hygiene: at least one of `imports` or `whitespace` must be true");
        }
        let mut routines_checked = Vec::new();
        if imports {
            routines_checked.push("organize_imports");
        }
        if whitespace {
            routines_checked.push("normalize_whitespace");
        }
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let mut file_edits = Vec::new();
            let mut findings = Vec::new();
            for file in params.files {
                let abs = match resolve_workspace_file(&root, &file, "java.hygiene") {
                    Ok(path) => path,
                    Err(e) => return err(e),
                };
                match bbox_refactor::java_hygiene_file(&root, &abs, imports, whitespace) {
                    Ok((mut edits, routines_applied)) if !edits.is_empty() => {
                        findings.push(json!({
                            "finding": "hygiene_changed",
                            "file": file,
                            "routines_applied": routines_applied,
                            "edit_count": edits.iter().map(|e| e.edits.len()).sum::<usize>(),
                        }));
                        file_edits.append(&mut edits);
                    }
                    Ok((_edits, routines_applied)) => findings.push(json!({
                        "finding": "no_hygiene_changes",
                        "file": file,
                        "routines_checked": routines_checked.clone(),
                        "routines_applied": routines_applied,
                    })),
                    Err(e) => return err(format!("java.hygiene: {file}: {e:#}")),
                }
            }
            let (changes, changed_files) =
                match file_edits_to_changes(&root, "java.hygiene", &file_edits) {
                    Ok(converted) => converted,
                    Err(e) => return err(format!("java.hygiene: {e}")),
                };
            ToolResult::Json(json!({
                "changes": changes,
                "changed_files": changed_files,
                "findings": findings,
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
        Arc::new(JavaExtractMethodCodeBlock) as Arc<dyn Tool>,
        Arc::new(JavaRemoveUnusedCtorParams) as Arc<dyn Tool>,
        Arc::new(JavaOrganizeImports) as Arc<dyn Tool>,
        Arc::new(JavaNormalizeWhitespace) as Arc<dyn Tool>,
        Arc::new(JavaHygiene) as Arc<dyn Tool>,
        Arc::new(JavaDescribe) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-dsl §5.2).
/// Deliberately a compact INDEX (§6.5 surface economics): one line per
/// transform; depth on demand via java.describe.
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "java".to_string(),
        description: "Java transform authorities (tree-sitter-backed; provenance syntax_only). Each transform runs real capture/wiring/hygiene analysis host-side and returns {changes, creates, findings} for the edits algebra — never writes. Call java.describe({transform}) for the full contract before first use. Transforms: extractClass — move methods/fields from a class into a new delegate class with source-side wiring (DI sources auto-wire external_injection so the delegate stays AOP-interceptable); extractMethodCodeBlock — extract one contiguous code block into a helper method after analysis.methodRegions gates; removeUnusedConstructorParams — drop dead @Inject ctor params after an extract (move the injection point); organizeImports / normalizeWhitespace / hygiene — routine post-apply cleanup for touched Java files."
            .to_string(),
        declarations: r#"type JavaDependencyProjection = { wiring: "own_construction" | "external_injection" | "none"; constructor_param_count: number; constructor_params: ({ finding: "captured_dependency"; name: string; type: string; route: string; target_constructor_param: boolean; wireability: string; risk?: string; recommendation?: string } & Record<string, unknown>)[]; non_injectable_params: string[]; moved_captured_fields: string[]; static_final_constants: string[]; summary: string };
type JavaTransformResult = { title: string; changes: SpanChange[]; creates: { path: string; content: string }[]; findings: ({ finding: string } & Record<string, unknown>)[]; dependency_projection: JavaDependencyProjection; preview_only: boolean; would_change_files: { path: string; edit_count: number; replacement_bytes: number }[]; would_create_files: { path: string; bytes: number }[]; fixme_count: number; provenance: "syntax_only" };
type JavaExtractMethodResult = { title: string; changes: SpanChange[]; findings: ({ finding: string } & Record<string, unknown>)[]; preview_only: boolean; would_change_files: { path: string; edit_count: number; replacement_bytes: number }[]; fixme_count: number; provenance: "syntax_only" };
type JavaHygieneResult = { changes: SpanChange[]; changed_files: { path: string; edit_count: number; replacement_bytes: number }[]; findings: ({ finding: string; file: string } & Record<string, unknown>)[]; provenance: "syntax_only" };
declare const java: {
  /** Full contract (params, findings vocabulary, recipe) for one transform. Call before first use. */
  describe(args: { transform: string }): Promise<{ contract: string }>;
  /** Extract methods/fields into a new delegate class. changes → edits.merge, creates → edits.createFile, then edits.apply. Pass wrappers: true to keep delegating stubs on the source (REQUIRED when callers outside the file use the moved methods — survey first). `wiring` auto-selects (Guice/DI source → external_injection, AOP-interceptable) — leave unset. Refusals are errors naming the exact fix. */
  extractClass(args: { file: string; target: string; delegateField: string; methods: string[]; moveFields?: string[]; className?: string; wiring?: "own_construction" | "external_injection" | "none"; wrappers?: boolean; previewOnly?: boolean }): Promise<JavaTransformResult>;
  /** Extract one exact contiguous code block into a helper method. Run analysis.methodRegions first for contiguity/live-out gates. changes → edits.merge. Refuses mutated captures and non-local control flow. Multiple live-outs refuse by default; pass resultRecord:true only when they are real top-level outputs with explicit types. */
  extractMethodCodeBlock(args: { file: string; oldText: string; methodName: string; className?: string; visibility?: "private" | "package-private" | "protected" | "public"; newText?: string; parameters?: Array<{ type: string; name: string }>; arguments?: string[]; returnType?: string; returnVar?: string; resultRecord?: boolean; resultRecordName?: string; resultRecordVar?: string; previewOnly?: boolean }): Promise<JavaExtractMethodResult>;
  /** Drop dead @Inject ctor params left by an extract (move the injection point). Returns {changes} → edits.merge. Run AFTER applying the extract. @Inject ctors only; refuses others with a note. */
  removeUnusedConstructorParams(args: { file: string }): Promise<{ changes: SpanChange[]; ctor_is_inject: boolean; removed: string[]; kept: string[]; findings: ({ finding: string } & Record<string, unknown>)[]; note: string | null; provenance: "syntax_only" }>;
  /** Prune/sort Java imports for touched files. Returns {changes} → edits.merge; [] means no import edits. */
  organizeImports(args: { files: string[] }): Promise<JavaHygieneResult>;
  /** Conservative whitespace hygiene for touched files. Returns {changes} → edits.merge; [] means no whitespace edits. */
  normalizeWhitespace(args: { files: string[] }): Promise<JavaHygieneResult>;
  /** Routine post-apply hygiene bundle: imports + whitespace by default. Returns {changes} → edits.merge; compile again if applied. */
  hygiene(args: { files: string[]; imports?: boolean; whitespace?: boolean }): Promise<JavaHygieneResult>;
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

    fn first_replacement(result: &Value) -> &str {
        result["changes"][0]["new_text"].as_str().unwrap()
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
        // Spans are relative to the worktree root and hash-anchored.
        let span = &result["changes"][0]["span"];
        assert_eq!(span["file"], "src/com/acme/OrderService.java");
        assert_eq!(
            span["content_sha256"],
            bbox_refactor::sha256_hex(FIXTURE.as_bytes()),
            "{span}"
        );
    }

    #[tokio::test]
    async fn extract_method_code_block_returns_merge_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/Auto.java"),
            "package com.acme;\n\
             class Auto {\n\
            \x20   int compute(int seed) {\n\
            \x20       int doubled = seed * 2;\n\
            \x20       return doubled;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractMethodCodeBlock
                .call(
                    json!({
                        "file": "src/com/acme/Auto.java",
                        "oldText": "int doubled = seed * 2;",
                        "methodName": "doubleIt",
                        "className": "Auto"
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(result["provenance"], "syntax_only", "{result}");
        assert_eq!(result["preview_only"], false, "{result}");
        let changes = result["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 2, "{result}");
        let replacements = changes
            .iter()
            .map(|change| change["new_text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            replacements.contains("int doubled = doubleIt(seed);"),
            "{replacements}"
        );
        assert!(
            replacements.contains("private int doubleIt(int seed)"),
            "{replacements}"
        );
        assert!(
            result["would_change_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["path"] == "src/com/acme/Auto.java"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn extract_method_code_block_multi_live_out_error_points_to_region_analysis() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/Multi.java"),
            "class Multi {\n\
            \x20   int run() {\n\
            \x20       int a = 1;\n\
            \x20       int b = 2;\n\
            \x20       return a + b;\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = JavaExtractMethodCodeBlock
            .call(
                json!({
                    "file": "src/Multi.java",
                    "oldText": "int a = 1;\n        int b = 2;",
                    "methodName": "prep"
                }),
                &cx,
            )
            .await;

        match result {
            ToolResult::Error(e) => {
                assert!(e.contains("multi_return_needs_record"), "{e}");
                assert!(e.contains("analysis.methodRegions"), "{e}");
            }
            other => panic!("expected multi-live-out refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_method_code_block_can_generate_result_record_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/Multi.java"),
            "class Multi {\n\
            \x20   int run() {\n\
            \x20       int a = 1;\n\
            \x20       String b = \"two\";\n\
            \x20       return a + b.length();\n\
            \x20   }\n\
             }\n",
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractMethodCodeBlock
                .call(
                    json!({
                        "file": "src/Multi.java",
                        "oldText": "int a = 1;\n        String b = \"two\";",
                        "methodName": "prep",
                        "resultRecord": true,
                        "resultRecordName": "PrepResult"
                    }),
                    &cx,
                )
                .await,
        );

        let replacements = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| change["new_text"].as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            replacements.contains("PrepResult prepResult = prep();"),
            "{replacements}"
        );
        assert!(
            replacements.contains("private record PrepResult(int a, String b) {}"),
            "{replacements}"
        );
        assert!(
            replacements.contains("return new PrepResult(a, b);"),
            "{replacements}"
        );
        assert_eq!(result["fixme_count"], 0, "{result}");
    }

    #[tokio::test]
    async fn organize_imports_returns_import_hygiene_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/Thing.java"),
            r#"package com.acme;

import java.util.Set;
import java.util.List;
public class Thing {
    public List<String> list() {
        return List.of();
    }
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaOrganizeImports
                .call(json!({ "files": ["src/com/acme/Thing.java"] }), &cx)
                .await,
        );
        assert_eq!(result["provenance"], "syntax_only", "{result}");
        assert_eq!(result["changes"].as_array().unwrap().len(), 1, "{result}");
        assert!(first_replacement(&result).contains("import java.util.List;"));
        assert!(!first_replacement(&result).contains("Set"), "{result}");
    }

    #[tokio::test]
    async fn normalize_whitespace_returns_spacing_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/Thing.java"),
            r#"package com.acme;
import java.util.List;
public class Thing {
    public Thing() {
    }


    public List<String> list() {
         return List.of();
    }
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaNormalizeWhitespace
                .call(json!({ "files": ["src/com/acme/Thing.java"] }), &cx)
                .await,
        );
        let rewritten = first_replacement(&result);
        assert!(
            rewritten.contains("package com.acme;\n\nimport java.util.List;\n\npublic class Thing"),
            "{rewritten}"
        );
        assert!(!rewritten.contains("\n\n\n"), "{rewritten}");
        assert!(
            rewritten.contains("\n        return List.of();"),
            "{rewritten}"
        );
    }

    #[tokio::test]
    async fn hygiene_combines_import_and_whitespace_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/Thing.java"),
            r#"package com.acme;
import java.util.Set;
import java.util.List;
public class Thing {
    public Thing() {
    }


    public List<String> list() {
         return List.of();
    }
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaHygiene
                .call(json!({ "files": ["src/com/acme/Thing.java"] }), &cx)
                .await,
        );
        assert_eq!(result["changes"].as_array().unwrap().len(), 1, "{result}");
        assert_eq!(
            result["findings"][0]["routines_applied"],
            json!(["organize_imports", "normalize_whitespace"]),
            "{result}"
        );
        let rewritten = first_replacement(&result);
        assert!(!rewritten.contains("Set"), "{rewritten}");
        assert!(
            rewritten.contains("package com.acme;\n\nimport java.util.List;\n\npublic class Thing"),
            "{rewritten}"
        );
        assert!(!rewritten.contains("\n\n\n"), "{rewritten}");
        assert!(
            rewritten.contains("\n        return List.of();"),
            "{rewritten}"
        );
    }

    #[tokio::test]
    async fn hygiene_noop_reports_checked_routines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/Thing.java"),
            r#"package com.acme;

import java.util.List;

public class Thing {
    public List<String> list() {
        return List.of();
    }
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaHygiene
                .call(json!({ "files": ["src/com/acme/Thing.java"] }), &cx)
                .await,
        );
        assert_eq!(result["changes"].as_array().unwrap().len(), 0, "{result}");
        assert_eq!(
            result["findings"][0]["routines_checked"],
            json!(["organize_imports", "normalize_whitespace"]),
            "{result}"
        );
        assert_eq!(
            result["findings"][0]["routines_applied"],
            json!([]),
            "{result}"
        );
    }

    #[tokio::test]
    async fn preview_only_omits_edit_payloads_but_keeps_findings() {
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
                        "previewOnly": true,
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(result["preview_only"], true, "{result}");
        assert!(
            result["changes"].as_array().unwrap().is_empty(),
            "preview should not ship edit payloads: {result}"
        );
        assert!(
            result["creates"].as_array().unwrap().is_empty(),
            "preview should not ship create payloads: {result}"
        );
        assert!(
            result["would_change_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["path"] == "src/com/acme/OrderService.java"),
            "preview should summarize source edits: {result}"
        );
        assert!(
            result["would_create_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["path"] == "src/com/acme/OrderPricing.java"),
            "preview should summarize target creation: {result}"
        );
        assert!(
            result["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| { f["finding"] == "captured_dependency" && f["name"] == "taxRate" }),
            "preview should still include dependency findings: {result}"
        );
        assert_eq!(
            result["dependency_projection"]["constructor_param_count"], 1,
            "{result}"
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

    const DI_MIXED_CAPTURE_FIXTURE: &str = r#"package com.acme;

import com.google.inject.Inject;

class OrderService {
    private final Repo repo;
    private final UiState state;

    @Inject
    OrderService(Repo repo) {
        this.repo = repo;
        this.state = new UiState();
    }

    void render() {
        repo.write(state.label());
    }
}
"#;

    #[tokio::test]
    async fn dependency_projection_flags_non_injectable_external_capture() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/com/acme")).unwrap();
        std::fs::write(
            root.join("src/com/acme/OrderService.java"),
            DI_MIXED_CAPTURE_FIXTURE,
        )
        .unwrap();
        let cx = cx_in(&root);

        let result = json_of(
            JavaExtractClass
                .call(
                    json!({
                        "file": "src/com/acme/OrderService.java",
                        "target": "src/com/acme/OrderRenderer.java",
                        "delegateField": "renderer",
                        "methods": ["render"],
                    }),
                    &cx,
                )
                .await,
        );

        let projection = &result["dependency_projection"];
        assert_eq!(projection["wiring"], "external_injection", "{result}");
        let non_injectable = projection["non_injectable_params"]
            .as_array()
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(
            non_injectable.contains(&Some("state")),
            "plain source state should be flagged for DI review: {projection}"
        );

        let params = projection["constructor_params"].as_array().unwrap();
        let repo = params
            .iter()
            .find(|param| param["name"] == "repo")
            .expect("repo projection");
        assert_eq!(repo["wireability"], "likely_injectable", "{repo}");
        assert_eq!(repo["injection_style"], "constructor_param", "{repo}");

        let state = params
            .iter()
            .find(|param| param["name"] == "state")
            .expect("state projection");
        assert_eq!(state["wireability"], "non_injectable_capture", "{state}");
        assert_eq!(state["risk"], "non_injectable_capture", "{state}");
        assert!(
            result["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| {
                    finding["finding"] == "captured_dependency"
                        && finding["name"] == "state"
                        && finding["risk"] == "non_injectable_capture"
                }),
            "captured_dependency finding should mirror projection: {result}"
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
        assert!(
            source.contains("private final OrderPricing pricing;"),
            "{source}"
        );
        assert!(
            source.contains("this.pricing = new OrderPricing(taxRate);"),
            "{source}"
        );
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
            result["note"]
                .as_str()
                .unwrap()
                .contains("no @Inject constructor"),
            "{result}"
        );
    }
}
