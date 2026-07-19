//! `analysis.*` — the reduce-Rust-side tier (cell-DSL §5; pressure-test §5).
//!
//! The complement to `code.*` facts. Where a fact binding returns
//! hash-anchored Spans for *surgical editing* (and is aggregate-capped so a
//! cell never sweeps a repo for raw spans), an *analysis* binding answers a
//! whole-file/whole-corpus QUESTION by running the reduction Rust-side and
//! returning a small structured result. The raw intermediate data — every
//! field touch, every call edge — never crosses into the isolate; the
//! reduced answer (a cluster graph, a dependency summary) is the product.
//!
//! Why a separate tier (probe-dash-1/2): a god-class decomposition needs the
//! cohesion structure. Driven through `code.*` facts it fails two ways —
//! sweep the repo with one broad query and OOM the isolate (dash-1), or
//! avoid the sweep and grind ~50 cells reconstructing the cluster graph in
//! JS (dash-2). Both are the same missing capability: a Rust-side reduction.
//! `analysis.cohesionClusters(file)` collapses that into one call.
//!
//! Still values-not-refs (cell-DSL §2): the returned value is the reduced
//! answer, computed where the data lives. Provenance tier is `syntax_only`
//! (tree-sitter analysis); these bindings never write.

use std::sync::Arc;

use async_trait::async_trait;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

fn err(msg: impl std::fmt::Display) -> ToolResult {
    ToolResult::Error(msg.to_string())
}

/// `analysis.cohesionClusters` — partition a class's methods into cohesive
/// suggested clusters (the seams of a god-class decomposition), returning the
/// reduced cluster graph rather than the raw field-touch/call edges.
pub struct AnalysisCohesionClusters;

#[derive(Deserialize)]
struct CohesionParams {
    file: String,
}

#[derive(Deserialize)]
struct ReferencesParams {
    symbols: Vec<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
    #[serde(default, rename = "declaringClass", alias = "declaring_class")]
    declaring_class: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Deserialize)]
struct FieldClassificationParams {
    file: String,
    #[serde(default)]
    fields: Option<Vec<String>>,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
}

#[derive(Deserialize)]
struct MethodRegionsParams {
    file: String,
    method: String,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
    #[serde(default)]
    ranges: Option<Vec<MethodRegionRangeParam>>,
    #[serde(
        default,
        rename = "includeStatementRegions",
        alias = "include_statement_regions"
    )]
    include_statement_regions: Option<bool>,
    #[serde(
        default,
        rename = "includeNestedStatementRegions",
        alias = "include_nested_statement_regions"
    )]
    include_nested_statement_regions: bool,
    #[serde(default, rename = "statementLimit", alias = "statement_limit")]
    statement_limit: Option<usize>,
    #[serde(default, rename = "statementStartLine", alias = "statement_start_line")]
    statement_start_line: Option<usize>,
    #[serde(default, rename = "statementEndLine", alias = "statement_end_line")]
    statement_end_line: Option<usize>,
    #[serde(default, rename = "statementContains", alias = "statement_contains")]
    statement_contains: Option<String>,
}

#[derive(Deserialize)]
struct MethodRegionRangeParam {
    #[serde(default)]
    label: Option<String>,
    #[serde(default, rename = "byteStart", alias = "byte_start")]
    byte_start: Option<usize>,
    #[serde(default, rename = "byteEnd", alias = "byte_end")]
    byte_end: Option<usize>,
    #[serde(default, rename = "startLine", alias = "start_line")]
    start_line: Option<usize>,
    #[serde(default, rename = "endLine", alias = "end_line")]
    end_line: Option<usize>,
}

impl From<MethodRegionRangeParam> for bbox_refactor::JavaMethodRegionRequest {
    fn from(value: MethodRegionRangeParam) -> Self {
        Self {
            label: value.label,
            byte_start: value.byte_start,
            byte_end: value.byte_end,
            start_line: value.start_line,
            end_line: value.end_line,
        }
    }
}

impl MethodRegionsParams {
    fn options(&self) -> bbox_refactor::JavaMethodRegionsOptions {
        bbox_refactor::JavaMethodRegionsOptions {
            include_statement_regions: self.include_statement_regions.unwrap_or(true),
            include_nested_statement_regions: self.include_nested_statement_regions,
            statement_limit: self.statement_limit,
            statement_start_line: self.statement_start_line,
            statement_end_line: self.statement_end_line,
            statement_contains: self.statement_contains.clone(),
        }
    }
}

#[async_trait]
impl Tool for AnalysisCohesionClusters {
    fn name(&self) -> &str {
        "analysis.cohesionClusters"
    }
    fn description(&self) -> &str {
        "Partition the first class in a Java file into cohesive method clusters — the candidate seams for splitting a god class. Runs the field-co-touch + call-graph analysis Rust-side and returns a small cluster graph: each cluster has {name_hint, item_names, move_fields, score, expected_wiring} and the cross-cluster calls between them. Pick a high-score cluster and feed item_names/move_fields/expected_wiring straight into java.extractClass. Pure; syntax_only; never writes. Use this instead of reconstructing cohesion from code.query captures."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Path to a .java file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is. The FIRST class declaration is analyzed." }
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
        Some(("analysis".to_string(), "cohesionClusters".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: CohesionParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.cohesionClusters: bad input — expected {{ file: string }}; {e}"
                ));
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let plan_input = json!({
                "kind": "extract_java_class_cohesive_clusters",
                "source": params.file,
                "project_dir": root.to_string_lossy(),
            });
            let plan_params: bbox_refactor::RefactorPlanParams =
                match serde_json::from_value(plan_input) {
                    Ok(p) => p,
                    Err(e) => {
                        return err(format!(
                            "analysis.cohesionClusters: internal param shape: {e}"
                        ));
                    }
                };
            let plan_json = match bbox_refactor::plan(&plan_params) {
                Ok(s) => s,
                Err(e) => return err(format!("analysis.cohesionClusters: {e:#}")),
            };
            let v: Value = match serde_json::from_str(&plan_json) {
                Ok(v) => v,
                Err(e) => return err(format!("analysis.cohesionClusters: decode: {e}")),
            };
            // Project the reduced answer: clusters + cross-cluster coupling +
            // class summary. Drop the analysis-only plan scaffolding
            // (file_moves/edits/validations are all empty here).
            let clusters = v.get("suggested_clusters").cloned().unwrap_or(json!([]));
            let cluster_count = clusters.as_array().map(|a| a.len()).unwrap_or(0);
            // gap-b241d431: the v1 class summary carries an absolute
            // source_path; relativize like every other binding payload.
            let mut class = v.get("class").cloned().unwrap_or(json!({}));
            if let Some(sp) = class.get("source_path").and_then(Value::as_str) {
                let rel = workspace_relative(&root, sp);
                class["source_path"] = json!(rel);
            }
            ToolResult::Json(json!({
                "file": params.file,
                "class": class,
                "cluster_count": cluster_count,
                "clusters": clusters,
                "cross_cluster_calls": v.get("cross_cluster_calls").cloned().unwrap_or(json!([])),
                "provenance": "syntax_only",
            }))
        })
        .await
    }
}

/// `analysis.references` — count Java symbol references across the workspace
/// without materializing every full capture into the isolate.
pub struct AnalysisReferences;

fn workspace_relative(root: &std::path::Path, path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        if let Ok(stripped) = p.strip_prefix(root) {
            return stripped.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

fn relativize_reference_paths(root: &std::path::Path, v: &mut Value) {
    if let Some(files_by_name) = v
        .get_mut("usage_files_by_name")
        .and_then(Value::as_object_mut)
    {
        for files in files_by_name.values_mut() {
            if let Some(files) = files.as_array_mut() {
                for file in files {
                    if let Some(path) = file.as_str() {
                        *file = json!(workspace_relative(root, path));
                    }
                }
            }
        }
    }
    if let Some(examples_by_name) = v
        .get_mut("usage_examples_by_name")
        .and_then(Value::as_object_mut)
    {
        for examples in examples_by_name.values_mut() {
            if let Some(examples) = examples.as_array_mut() {
                for example in examples {
                    if let Some(path) = example.get("path").and_then(Value::as_str) {
                        example["path"] = json!(workspace_relative(root, path));
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Tool for AnalysisReferences {
    fn name(&self) -> &str {
        "analysis.references"
    }
    fn description(&self) -> &str {
        "Count symbol references across the workspace without returning full capture payloads. Java (default) uses tree-sitter Java analysis with usage-kind filter; Rust mode uses syntactic Rust reference counting with rust usage kinds (call, type_ref, path_ref, macro_use). Returns per-symbol counts, files, and up to five example sites per symbol. Use this when deciding whether extracted methods need wrappers, whether a candidate field is only forwarded, or whether a seam has external callers. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbols": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Simple symbol names to count, e.g. method, field, or type names."
                },
                "language": {
                    "type": "string",
                    "enum": ["java", "rust"],
                    "description": "Language mode (default \"java\"). Rust mode counts syntactic references with usage kinds: call, type_ref, path_ref, macro_use."
                },
                "kinds": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["type_reference", "method_invocation", "field_access", "method_reference", "import"]
                    },
                    "description": "Optional Java usage-kind filter (ignored in Rust mode)."
                },
                "declaringClass": {
                    "type": "string",
                    "description": "Optional Java receiver filter for method invocations (ignored in Rust mode)."
                }
            },
            "required": ["symbols"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("analysis".to_string(), "references".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ReferencesParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.references: bad input — expected {{ symbols: string[] }}; {e}"
                ));
            }
        };
        if params.symbols.is_empty() {
            return err("analysis.references: `symbols` must be non-empty");
        }
        let is_rust = params
            .language
            .as_deref()
            .map(|l| l == "rust")
            .unwrap_or(false);
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let requested_symbols = params.symbols.clone();
            if is_rust {
                // Rust mode: direct syntactic reference counting.
                match bbox_refactor::rust_references::count_rust_references(
                    &root,
                    &requested_symbols,
                ) {
                    Ok(summary) => {
                        ToolResult::Json(json!({
                            "symbols": summary.symbols,
                            "total_usages": summary.total_usages,
                            "unique_files": summary.unique_files,
                            "production_sites": summary.production_sites,
                            "test_sites": summary.test_sites,
                            "counts_by_symbol": summary.counts_by_symbol,
                            "files_by_symbol": summary.files_by_symbol,
                            "examples_by_symbol": summary.examples_by_symbol,
                            "truncated": summary.truncated,
                            "provenance": "syntax_only",
                        }))
                    }
                    Err(e) => err(format!("analysis.references: {e:#}")),
                }
            } else {
                // Java mode: plan-based path (existing behavior).
                let plan_input = json!({
                    "kind": "find_java_usages",
                    "source": "",
                    "project_dir": root.to_string_lossy(),
                    "item_names": params.symbols,
                    "item_kinds": params.kinds,
                    "declaring_class": params.declaring_class,
                    "summary_only": true,
                });
                let plan_params: bbox_refactor::RefactorPlanParams =
                    match serde_json::from_value(plan_input) {
                        Ok(p) => p,
                        Err(e) => {
                            return err(format!("analysis.references: internal param shape: {e}"));
                        }
                    };
                let plan_json = match bbox_refactor::plan(&plan_params) {
                    Ok(s) => s,
                    Err(e) => return err(format!("analysis.references: {e:#}")),
                };
                let mut v: Value = match serde_json::from_str(&plan_json) {
                    Ok(v) => v,
                    Err(e) => return err(format!("analysis.references: decode: {e}")),
                };
                relativize_reference_paths(&root, &mut v);
                ToolResult::Json(json!({
                    "symbols": requested_symbols,
                    "total_usages": v.get("total_usages").cloned().unwrap_or(json!(0)),
                    "unique_files": v.get("unique_call_files").cloned().unwrap_or(json!(0)),
                    "production_sites": v.get("production_sites").cloned().unwrap_or(json!(0)),
                    "test_sites": v.get("test_sites").cloned().unwrap_or(json!(0)),
                    "counts_by_symbol": v.get("usage_summary_by_name").cloned().unwrap_or(json!({})),
                    "files_by_symbol": v.get("usage_files_by_name").cloned().unwrap_or(json!({})),
                    "examples_by_symbol": v.get("usage_examples_by_name").cloned().unwrap_or(json!({})),
                    "provenance": "syntax_only",
                }))
            }
        })
        .await
    }
}

/// `analysis.fieldClassification` — pre-extract field state/read-write facts
/// for Java classes.
pub struct AnalysisFieldClassification;

fn field_classification_payload(
    file: &str,
    found: &bbox_refactor::facts::FileJavaFieldClassificationFacts,
) -> Value {
    let fields: Vec<Value> = found
        .fields
        .iter()
        .map(|field| {
            let accesses: Vec<Value> = field
                .accesses
                .iter()
                .map(|access| {
                    json!({
                        "method": access.method,
                        "kind": access.kind,
                        "line": access.line,
                        "column": access.column,
                        "context": access.context,
                    })
                })
                .collect();
            json!({
                "name": field.name,
                "type": field.type_text,
                "owner_class": field.owner_class,
                "visibility": field.visibility,
                "modifiers": field.modifiers,
                "annotations": field.annotations,
                "is_static_final": field.is_static_final,
                "is_mutable_instance": field.is_mutable_instance,
                "is_injected": field.is_injected,
                "injection_style": field.injection_style,
                "is_provider": field.is_provider,
                "reads": field.reads,
                "writes": field.writes,
                "read_by": field.read_by,
                "written_by": field.written_by,
                "accesses": accesses,
            })
        })
        .collect();
    json!({
        "file": file,
        "language": found.language,
        "content_sha256": found.content_sha256,
        "source_len": found.source_len,
        "fields": fields,
        "provenance": "syntax_only",
    })
}

#[async_trait]
impl Tool for AnalysisFieldClassification {
    fn name(&self) -> &str {
        "analysis.fieldClassification"
    }
    fn description(&self) -> &str {
        "Classify Java fields before extraction: static/final vs mutable instance state, injection/provider hints, and read/write sites grouped by enclosing method. Use this before java.extractClass when deciding whether moved fields are constants, dependencies, or mutable view state. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative .java source file." },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Optional field names to classify. Omit to classify every field in the file/class." },
                "className": { "type": "string", "description": "Optional owner class name to restrict the field set." }
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
        Some(("analysis".to_string(), "fieldClassification".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: FieldClassificationParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.fieldClassification: bad input — expected {{ file, fields?: string[] }}; {e}"
                ));
            }
        };
        if params.fields.as_ref().is_some_and(Vec::is_empty) {
            return err("analysis.fieldClassification: `fields`, when passed, must be non-empty");
        }
        let path = match bro_tools::workspace::resolve_in_root(&cx.root, &params.file) {
            Ok(path) => path,
            Err(e) => {
                return err(format!(
                    "analysis.fieldClassification: {}: {e}",
                    params.file
                ));
            }
        };
        let file = params.file;
        let fields = params.fields;
        let class_name = params.class_name;
        bro_tools::tool::call_blocking(
            move || match bbox_refactor::facts::java_field_classification(
                &path,
                fields.as_deref(),
                class_name.as_deref(),
            ) {
                Ok(found) => ToolResult::Json(field_classification_payload(&file, &found)),
                Err(e) => err(format!("analysis.fieldClassification: {e:#}")),
            },
        )
        .await
    }
}

/// `analysis.methodRegions` — method-body region/dataflow facts for
/// monolithic-method extract planning.
pub struct AnalysisMethodRegions;

#[async_trait]
impl Tool for AnalysisMethodRegions {
    fn name(&self) -> &str {
        "analysis.methodRegions"
    }
    fn description(&self) -> &str {
        "Analyze one Java method body before extract-method work. Returns top-level statement regions by default, optional nested statement regions, and optional requested ranges with captures, live-outs, field touches, lambda/listener counts, non-local control flow, and extractability stop reasons. Pure; syntax_only; never writes. Use this for the contiguity/live-out gates before java.extractMethodCodeBlock."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative .java source file." },
                "method": { "type": "string", "description": "Method or constructor name to analyze." },
                "className": { "type": "string", "description": "Optional owner class name when the file has multiple classes." },
                "includeStatementRegions": { "type": "boolean", "description": "Default true. Set false when only requested_ranges and statement_region_summary are needed." },
                "includeNestedStatementRegions": { "type": "boolean", "description": "Default false. When true, statement_regions inventories nested Java statement nodes instead of only method-body top-level statements; useful when a giant loop/block encloses the target marker." },
                "statementLimit": { "type": "integer", "description": "Optional cap on returned statement_regions after filtering." },
                "statementStartLine": { "type": "integer", "description": "Optional line filter: include statement regions whose line range overlaps this start line or later." },
                "statementEndLine": { "type": "integer", "description": "Optional line filter: include statement regions whose line range overlaps this end line or earlier." },
                "statementContains": { "type": "string", "description": "Optional substring filter over the statement source text, useful as a marker-first compact query." },
                "ranges": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string" },
                            "startLine": { "type": "integer" },
                            "endLine": { "type": "integer" },
                            "byteStart": { "type": "integer" },
                            "byteEnd": { "type": "integer" }
                        }
                    },
                    "description": "Optional candidate ranges to analyze. Each range is either {startLine,endLine} or {byteStart,byteEnd}."
                }
            },
            "required": ["file", "method"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("analysis".to_string(), "methodRegions".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: MethodRegionsParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.methodRegions: bad input — expected {{ file, method, className?, ranges? }}; {e}"
                ));
            }
        };
        if params.method.trim().is_empty() {
            return err("analysis.methodRegions: `method` must be non-empty");
        }
        let path = match bro_tools::workspace::resolve_in_root(&cx.root, &params.file) {
            Ok(path) => path,
            Err(e) => return err(format!("analysis.methodRegions: {}: {e}", params.file)),
        };
        let options = params.options();
        let file = params.file;
        let method = params.method;
        let class_name = params.class_name;
        let ranges = params
            .ranges
            .unwrap_or_default()
            .into_iter()
            .map(bbox_refactor::JavaMethodRegionRequest::from)
            .collect::<Vec<_>>();
        bro_tools::tool::call_blocking(move || {
            let range_slice = if ranges.is_empty() {
                None
            } else {
                Some(ranges.as_slice())
            };
            match bbox_refactor::analyze_java_method_regions_with_options(
                &path,
                &method,
                class_name.as_deref(),
                range_slice,
                Some(&options),
            ) {
                Ok(found) => {
                    let mut value = serde_json::to_value(found).unwrap_or_else(|_| json!({}));
                    value["file"] = json!(file);
                    ToolResult::Json(value)
                }
                Err(e) => err(format!("analysis.methodRegions: {e:#}")),
            }
        })
        .await
    }
}

/// `analysis.fieldInitializerClosure` — transitive field/constant initializer
/// dependency closure for moved fields.
pub struct AnalysisFieldInitializerClosure;

#[derive(Deserialize)]
struct FieldInitClosureParams {
    file: String,
    fields: Vec<String>,
    #[serde(default, rename = "className", alias = "class_name")]
    class_name: Option<String>,
}

#[async_trait]
impl Tool for AnalysisFieldInitializerClosure {
    fn name(&self) -> &str {
        "analysis.fieldInitializerClosure"
    }
    fn description(&self) -> &str {
        "Compute transitive field/constant dependency closure for moved fields. For each static final field in 'fields', finds constants referenced in its initializer and follows the chain transitively. Use before java.extractClass to prevent compile failures from moved constants whose initializers reference source constants left behind. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Workspace-relative .java source file." },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Field names to compute initializer closure for (typically seam.move_fields)." },
                "className": { "type": "string", "description": "Optional owner class name when the file has multiple classes." }
            },
            "required": ["file", "fields"]
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
            "analysis".to_string(),
            "fieldInitializerClosure".to_string(),
        ))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: FieldInitClosureParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.fieldInitializerClosure: bad input — expected {{ file, fields }}; {e}"
                ));
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            match bbox_refactor::facts::java_field_initializer_closure(
                &root.join(&params.file),
                &params.fields,
                params.class_name.as_deref(),
            ) {
                Ok(closure) => ToolResult::Json(json!({
                    "file": params.file,
                    "fields": params.fields,
                    "closure": closure,
                    "provenance": "syntax_only",
                })),
                Err(e) => err(format!("analysis.fieldInitializerClosure: {e:#}")),
            }
        })
        .await
    }
}

/// `analysis.implPartition` (rust) — impl-method call/state graph of one
/// impl block for split planning.
pub struct AnalysisImplPartition;

#[derive(Deserialize)]
struct ImplPartitionParams {
    file: String,
    #[serde(default, rename = "implName", alias = "impl_name")]
    impl_name: Option<String>,
    #[serde(default, rename = "moduleName", alias = "module_name")]
    module_name: Option<String>,
}

#[async_trait]
impl Tool for AnalysisImplPartition {
    fn name(&self) -> &str {
        "analysis.implPartition"
    }
    fn description(&self) -> &str {
        "Analyze one or more Rust impl blocks in a file, returning a partition graph with per-method field reads/writes/calls, shared fields, and inferred edges. `impl_name` accepts either the simple type name (e.g. \"BlackboxServer\") or the impl header form (e.g. \"impl BlackboxServer\"). All impl blocks for that type are merged; methods are ordered by source byte position. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Path to a .rs file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is." },
                "implName": { "type": "string", "description": "The impl type name, e.g. \"BlackboxServer\" or \"impl BlackboxServer\"." },
                "moduleName": { "type": "string", "description": "Alias for implName." }
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
        Some(("analysis".to_string(), "implPartition".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: ImplPartitionParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.implPartition: bad input — expected {{ file, implName? }}; {e}"
                ));
            }
        };
        let impl_name = match (params.impl_name, params.module_name) {
            (Some(name), _) if !name.trim().is_empty() => name,
            (_, Some(name)) if !name.trim().is_empty() => name,
            _ => {
                return err("analysis.implPartition: `implName` (or `moduleName`) is required");
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let source_path = match bro_tools::workspace::resolve_in_root(&root, &params.file) {
                Ok(path) => path,
                Err(e) => {
                    return err(format!("analysis.implPartition: {}: {e}", params.file));
                }
            };
            match bbox_refactor::rust_partition::analyze_impl(&source_path, &impl_name) {
                Ok(graph) => ToolResult::Json(json!({
                    "file": params.file,
                    // relativize impl_name back to user's input form
                    "impl_name": impl_name,
                    "methods": graph.methods,
                    "fields": graph.fields,
                    "edges": graph.edges,
                    "provenance": "syntax_only",
                })),
                Err(e) => err(format!("analysis.implPartition: {e:#}")),
            }
        })
        .await
    }
}

/// `analysis.topLevelDeps` (rust) — top-level item dependency graph with
/// external reference hints and suggested clusters.
pub struct AnalysisTopLevelDeps;

#[derive(Deserialize)]
struct TopLevelDepsParams {
    file: String,
    #[serde(default, rename = "projectDir", alias = "project_dir")]
    project_dir: Option<String>,
    #[serde(default, rename = "itemNames", alias = "item_names")]
    item_names: Option<Vec<String>>,
    #[serde(default, rename = "itemKinds", alias = "item_kinds")]
    item_kinds: Option<Vec<String>>,
}

#[async_trait]
impl Tool for AnalysisTopLevelDeps {
    fn name(&self) -> &str {
        "analysis.topLevelDeps"
    }
    fn description(&self) -> &str {
        "Analyze top-level Rust items in a file: dependency graph (calls, type refs, module refs, globals), external reference hints scanned across the project, and suggested clusters for extraction planning. Optionally narrow to specific item names/kinds. Use as the pre-extract survey before rust.extractItems. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Path to a .rs file. Relative paths resolve against the session worktree root; absolute paths are accepted as-is." },
                "projectDir": { "type": "string", "description": "Optional project directory for external reference scanning. Defaults to the file's parent directory." },
                "itemNames": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional specific item names to analyze; omit for all top-level named items."
                },
                "itemKinds": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional filter: only items of these tree-sitter kinds (e.g. \"function_item\", \"struct_item\")."
                }
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
        Some(("analysis".to_string(), "topLevelDeps".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let params: TopLevelDepsParams = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return err(format!(
                    "analysis.topLevelDeps: bad input — expected {{ file, itemNames?, itemKinds? }}; {e}"
                ));
            }
        };
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let source_path = match bro_tools::workspace::resolve_in_root(&root, &params.file) {
                Ok(path) => path,
                Err(e) => {
                    return err(format!("analysis.topLevelDeps: {}: {e}", params.file));
                }
            };
            let project_dir = params.project_dir.as_deref();
            let item_names = params.item_names.as_deref();
            let item_kinds = params.item_kinds.as_deref();
            match bbox_refactor::rust_top_level_deps::analyze_top_level(
                &source_path,
                project_dir,
                item_names,
                item_kinds,
            ) {
                Ok(graph) => {
                    // relativize paths in external_references
                    let ext_refs: Vec<Value> = graph
                        .external_references
                        .iter()
                        .map(|r| {
                            json!({
                                "item": r.item,
                                "path": workspace_relative(&root, &r.path),
                                "line": r.line,
                                "context": r.context,
                            })
                        })
                        .collect();
                    ToolResult::Json(json!({
                        "file": params.file,
                        "items": graph.items,
                        "edges": graph.edges,
                        "external_references": ext_refs,
                        "suggested_clusters": graph.suggested_clusters,
                        "warnings": graph.warnings,
                        "provenance": "syntax_only",
                    }))
                }
                Err(e) => err(format!("analysis.topLevelDeps: {e:#}")),
            }
        })
        .await
    }
}

/// `analysis.describe` — depth-on-demand contract for one analysis (matches
/// the java.describe pattern; the namespace index stays a compact one-liner).
pub struct AnalysisDescribe;

const COHESION_CONTRACT: &str = r#"analysis.cohesionClusters — find the cohesive method clusters (decomposition seams) of a Java class.

WHAT IT DOES
  Builds the method↔field co-touch graph and the method↔method call graph for the
  FIRST class in the file, partitions methods by modularity community detection
  over an inverse-field-frequency-weighted graph, and returns the reduced graph.
  Connector-aware: a high-fan-out field touched by many methods (a shared UI
  container, a refresh dispatcher) is down-weighted so it cannot fuse distinct
  concerns into one megacluster — concern-private fields dominate the partition.
  This is the Rust-side answer to "what are the real seams" — do NOT reconstruct it
  from code.query captures (that path OOMs on a sweep or burns ~50 cells by hand).

PARAMS
  file: string   Path to a .java file. Relative paths resolve against the
                     session worktree root; absolute paths are accepted
                     as-is. (Refactor plan OUTPUTS still must land inside
                     the worktree — see java.* transform integrity checks.)

RETURNS { file, class, cluster_count, clusters, cross_cluster_calls, provenance }
  clusters[]: each is a ready-to-extract seam —
    id              cluster id
    name_hint       suggested class name (e.g. "OrderPricing")
    item_names      methods in the cluster  → java.extractClass `methods`
    move_fields     fields touched ONLY by this cluster → java.extractClass `moveFields`
    score           cohesion 0..1 (internal touches / (internal + cross-cluster coupling));
                    higher = cleaner to extract. Singletons / low scores = weak seams.
    expected_wiring "delegate" | "callback" | "source_instance" — the cluster's COUPLING
                    shape, a seam-QUALITY signal (NOT java.extractClass's `wiring` param,
                    which is the DI strategy — leave that unset; see below).
                    delegate: clean one-way split — source holds it and calls in. Extract directly.
                    callback: cluster calls back into source — those surface as external_call
                              findings to resolve (or pick a cleaner seam).
                    source_instance: bidirectional coupling — not a clean seam, prefer another cluster.
    internal_field_touches / internal_calls / inbound_calls / outbound_calls — the raw counts
  cross_cluster_calls[]: {from_cluster, to_cluster, from_method, to_method} — coupling you
                    keep before deciding to split. Beware false seams: fields that are merely
                    injected-and-forwarded show up as singleton/low-score clusters, not real ones.

RECIPE (god-class decomposition)
  const a = await analysis.cohesionClusters({ file });
  const seam = a.clusters.filter(c => c.score >= 0.7 && c.item_names.length > 1)
                         .sort((x,y) => y.score - x.score)[0];   // pick the cleanest real seam
  if (!seam) { text("no clean seam — class may be genuinely cohesive or need finer analysis"); exit(); }
  // Prefer expected_wiring === "delegate" seams (clean one-way splits).
  const r = await java.extractClass({
    file, target: `.../${seam.name_hint}.java`, delegateField: lc(seam.name_hint),
    methods: seam.item_names, moveFields: seam.move_fields,
    className: seam.name_hint, wrappers: true,
    // NOTE: do NOT pass `wiring` here. extractClass auto-selects it from the
    // source: a Guice/DI-managed class (uses @Inject) gets external_injection
    // so the delegate stays container-managed and AOP-interceptable. Only set
    // wiring explicitly to force own_construction (a plain `new`-ed delegate).
  });
  // edits.createFile/merge/apply, then compile-gate, as java.describe shows.
  // FINALLY, when the source is DI-managed (external_injection was used), the
  // moved deps are left as dead @Inject params on the source ctor — compose
  // java.removeUnusedConstructorParams({ file }) → edits.merge/apply to drop
  // them and fully move the injection point (run it AFTER the extract apply)."#;

const REFERENCES_CONTRACT: &str = r#"analysis.references — count Java symbol references across the workspace without returning full capture payloads.

WHAT IT DOES
  Walks .java files under the session root and counts AST-grounded references to
  simple symbol names. It returns per-symbol counts, every file containing each
  symbol, and up to five example sites per symbol. This is the bounded answer to
  "where is this referenced?" when you need counts for planning, not editable
  spans. Do NOT fan out code.query across the repo just to count.

PARAMS
  symbols: string[]   simple names to count (method, field, or type names)
  kinds?: string[]    optional filter: type_reference | method_invocation |
                      field_access | method_reference | import
  declaringClass?: string
                      optional method-call receiver filter. When set, includes
                      method invocations whose receiver plausibly resolves to a
                      field of this class in the enclosing class.

RETURNS { symbols, total_usages, unique_files, production_sites, test_sites,
          counts_by_symbol, files_by_symbol, examples_by_symbol, provenance }
  counts_by_symbol: { [symbol]: count }
  files_by_symbol:  { [symbol]: relative-to-worktree-root files[] }
  examples_by_symbol: top 5 examples per symbol with path, line, column, context,
                      usage_kind, is_test_site. Examples are for orientation only;
                      use code.query/code.items if you need hash-anchored spans.

USES
  - Before java.extractClass, count moved public methods with
    kinds:["method_invocation"] to decide whether wrappers:true is needed.
  - During seam selection, count candidate fields or delegate names to distinguish
    real concern-private state from forwarded-field false seams.
  - Before deleting or moving a type, get cheap production/test usage counts.

RECIPE
  const refs = await analysis.references({
    symbols: seam.item_names,
    kinds: ["method_invocation"],
  });
  const externalFiles = Object.values(refs.files_by_symbol).flat()
    .filter(f => f !== file);
  const hasExternalCallers = externalFiles.length > 0;
  const r = await java.extractClass({ ..., wrappers: hasExternalCallers });
"#;

const FIELD_CLASSIFICATION_CONTRACT: &str = r#"analysis.fieldClassification — classify Java field declarations before extraction.

WHAT IT DOES
  Reads one Java file and returns a reduced per-field answer:
  declaration kind (static/final/mutable), injection/provider hints, and read/write
  sites grouped by enclosing method. Injection hints cover both @Inject field
  annotations and @Inject constructors that assign this.field = param. This is
  the pre-extract answer to "are these move_fields constants, dependencies, or
  mutable source state?" Use it instead of raw field_declaration modifier queries.

PARAMS
  file: string        workspace-relative .java file
  fields?: string[]   optional field names to classify; omit for all fields
  className?: string  optional owner class restriction

RETURNS { file, language, content_sha256, source_len, fields, provenance }
  fields[]:
    name, type, owner_class, visibility, modifiers, annotations
    is_static_final       true for constants that can usually move as constants
    is_mutable_instance   true for non-static, non-final instance state
    is_injected           declaration has @Inject, or @Inject ctor assigns this.field = param
    injection_style       "field_annotation" | "constructor_param" | null
    is_provider           declared type is a Provider-like type
    reads / writes        access counts in the current source file
    read_by / written_by  enclosing method names
    accesses[]            small diagnostic sites: method, kind, line, column, context

RECIPE
  const cls = await analysis.fieldClassification({
    file,
    fields: seam.move_fields,
    className: "SourceView"
  });
  const mutable = cls.fields.filter(f => f.is_mutable_instance);
  const constants = cls.fields.filter(f => f.is_static_final);
  const deps = cls.fields.filter(f => f.is_injected || f.is_provider);
  // Keep the classification in store(); echo only the small derived counts/table.
"#;

const METHOD_REGIONS_CONTRACT: &str = r#"analysis.methodRegions — analyze Java method-body regions before extract-method work.

WHAT IT DOES
  Reads one Java method/constructor body and returns a bounded region report.
  The default `statement_regions` are the method body's top-level statements.
  Optional `ranges` are exact candidate regions you want to gate before
  java.extractMethodCodeBlock. Each region is analyzed with the same lexical
  scope/dataflow engine as the mutating extractor.

PARAMS
  file: string        workspace-relative .java file
  method: string      method or constructor name
  className?: string  optional owner class restriction
  includeStatementRegions?: boolean  default true; set false for gate-only calls
  includeNestedStatementRegions?: boolean default false; when true,
                            statement_regions inventories nested statements,
                            not only method-body top-level statements
  statementLimit?: number            cap returned statement_regions after filtering
  statementStartLine?: number        include statements overlapping this line or later
  statementEndLine?: number          include statements overlapping this line or earlier
  statementContains?: string         substring marker filter for statement source text
  ranges?: Array<{
    label?: string,
    startLine?: number, endLine?: number,     // 1-based inclusive lines
    byteStart?: number, byteEnd?: number      // exact byte span alternative
  }>

RETURNS
  { file, class_name, method_name, method_line_range, body_line_range,
    parameters, statement_region_summary, statement_regions, requested_ranges,
    requested_contiguous, provenance }

  statement_region_summary:
    total_count      total top-level statement count in the method body
    matched_count    count matching statementStartLine/statementEndLine/statementContains
    returned_count   count materialized in statement_regions
    omitted_count    total statements omitted by filtering, limit, or include=false
    included         false when includeStatementRegions=false

  region fields:
    id, label, kind, byte_start, byte_end, line_range, statement_count, preview
    captures[]            locals/params declared before the region and read inside
                           (mutated=true means the current extractor must stop)
                           If a source declaration uses Java `var`, `type` stays
                           "var"; `resolved_type` is present only when syntax-only
                           inference found a concrete helper-safe type.
    live_outs[]           locals declared inside the region and read after it
                           (>1 means current extractor needs a record/result bundle)
                           Each live-out may include after_use_kinds[] such as
                           return_value, method_argument, method_receiver,
                           component_tree_argument, component_tree_receiver,
                           and component_tree_consumptions[] for in-region
                           UI/component-tree wiring like layout.add(child).
                           `resolved_type` follows the same Java `var` convention
                           as captures.
    field_touches[]       class fields read/written in the region
    lambda_count / listener_call_count
    non_local_control_flow[]  return/break/continue that would change semantics;
                              returns inside a fully selected lambda are local
                              to that lambda and do not count here
    extractability        { can_extract_with_current_tool, stop_reasons,
                            live_out_count, mutated_capture_count,
                            non_local_control_flow_count }

RECIPE (monolithic-method stage extraction)
  // For large methods, search/filter first instead of materializing every statement.
  const a = await analysis.methodRegions({
    file, method: "buildView", className: "View",
    statementContains: "flowSplit", statementLimit: 20
  });
  // If the marker sits inside one giant top-level loop/block, opt into nested
  // statement inventory and use the returned line ranges to gate exact ranges.
  const nested = await analysis.methodRegions({
    file, method: "buildView", className: "View",
    includeNestedStatementRegions: true,
    statementContains: "flowSplit", statementLimit: 20
  });
  // Identify a candidate from the compact statement_regions result, then analyze
  // the exact contiguous range before mutating. For gate-only calls, set
  // includeStatementRegions:false and pass ranges.
  const gate = await analysis.methodRegions({
    file, method: "buildView", className: "View",
    ranges: [{ label: "controls", startLine: 120, endLine: 155 }]
  });
  const r = gate.requested_ranges[0];
  if (!gate.requested_contiguous || !r.extractability.can_extract_with_current_tool) {
    text({ stop_reasons: r.extractability.stop_reasons, live_outs: r.live_outs });
    exit(); // choose a smaller contiguous block or wait for a stronger primitive
  }
  // Then call java.extractMethodCodeBlock with the exact text from the accepted range.
"#;

const FIELD_INIT_CLOSURE_CONTRACT: &str = r#"analysis.fieldInitializerClosure — transitive field/constant initializer dependency closure.

WHAT IT DOES
  For each static final field in `fields`, walks its initializer and finds
  references to OTHER static final fields/constants in the same file. Then
  follows those references transitively to produce the full closure — the set
  of all source constants that must move with each requested field to preserve
  compile-time validity. Only fields with non-empty closures appear in the
  result; a field with no constant dependencies returns nothing.

PARAMS
  file: string        workspace-relative .java file
  fields: string[]    field names to check (typically seam.move_fields)
  className?: string  optional owner class restriction

RETURNS { file, fields, closure, provenance }
  closure: { [field_name]: string[] }  only present for fields with deps;
          values are sorted alphabetically

RECIPE
  const fc = await analysis.fieldInitializerClosure({
    file, fields: seam.move_fields,
  });
  if (Object.keys(fc.closure).length) {
    // Add transitive deps to move_fields before calling java.extractClass
    for (const [field, deps] of Object.entries(fc.closure)) {
      for (const dep of deps) {
        if (!seam.move_fields.includes(dep)) seam.move_fields.push(dep);
      }
    }
  }
  // Now seam.move_fields includes the full transitive closure.
"#;

const IMPL_PARTITION_CONTRACT: &str = r#"analysis.implPartition (rust) — impl-method call/state graph for split planning.

WHAT IT DOES
  Parses the named Rust impl block(s) in a .rs file, builds a per-method
  field read/write/call graph plus shared-field tracking, and returns the
  reduced graph. Methods are ordered by source byte position; calls that
  don't resolve to a method in the same impl set are placed in
  unresolved_callbacks. `impl_name` accepts either the simple type name
  (e.g. "BlackboxServer") or the impl header form (e.g.
  "impl BlackboxServer"). All impl blocks for that type are merged.

PARAMS
  file: string       workspace-relative .rs file
  implName?: string  impl type name or "impl TypeName"; required
  moduleName?: string  alias for implName

RETURNS { file, impl_name, methods, fields, edges, provenance }
  methods[]: each has name, reads[], writes[], calls[],
             unresolved_callbacks[], attrs[], router
  fields[]:  { name, type, shared_by }
  edges[]:   { from: method_name, to: method_name, kind }

RECIPE (god-impl split survey)
  const g = await analysis.implPartition({
    file: "src/server.rs", implName: "BlackboxServer"
  });
  // Survey: which methods touch which fields? Which call each other?
  // Methods sharing fields suggest they belong together in a split.
  // unresolved_callbacks flag cross-impl / trait / external calls.
  // Feed findings into rust.extractImplMethods for the actual split.
"#;

const TOP_LEVEL_DEPS_CONTRACT: &str = r#"analysis.topLevelDeps (rust) — top-level item dependency graph + external reference hints.

WHAT IT DOES
  Analyzes top-level Rust items in one file: builds a dependency graph
  (calls, type_refs, module_refs, global_refs), scans the project for
  external references (up to 12 per item, 40 items max), and suggests
  connected-component clusters. This is the pre-extract survey: use it
  before rust.extractItems to choose items that move together. Macro-heavy
  items emit warnings about hidden dependency edges.

PARAMS
  file: string         workspace-relative .rs file
  projectDir?: string  project root for external reference scanning;
                       defaults to the file's parent directory
  itemNames?: string[] optional specific item names to analyze; omit
                       for all top-level named items
  itemKinds?: string[] optional filter by tree-sitter kind
                       (function_item, struct_item, mod_item, ...)

RETURNS { file, items, edges, external_references,
          suggested_clusters, warnings, provenance }
  items[]: { name, kind, line_start, line_end, calls, type_refs,
             module_refs, global_refs, attrs, warnings }
  edges[]: { from, to, kind: calls|type_ref|module_ref|global_ref }
  external_references[]: { item, path, line, context }
  suggested_clusters[]: { id, items, reason, internal_edges,
                          external_references }
  warnings[]: macro-heavy items flag

RECIPE (monster-file split)
  const g = await analysis.topLevelDeps({ file: "src/lib.rs" });
  // Survey: which items depend on each other?
  // suggested_clusters are connected components — items in the same
  // cluster reference each other and may move together.
  // Use rust.extractItems to extract a cluster into a submodule.
"#;

#[async_trait]
impl Tool for AnalysisDescribe {
    fn name(&self) -> &str {
        "analysis.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one analysis.* binding (params, result vocabulary, recipe). The namespace index lists analyses one line each; call this before first use. Pass language (\"java\" or \"rust\") to filter available analyses."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "analysis": { "type": "string", "description": "Analysis name, e.g. \"cohesionClusters\" or \"implPartition\"." },
                "language": { "type": "string", "enum": ["java", "rust"], "description": "Optional language filter: only analyses for this language are valid." }
            },
            "required": ["analysis"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("analysis".to_string(), "describe".to_string()))
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let analysis = input
            .get("analysis")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let language = input.get("language").and_then(Value::as_str);
        match analysis {
            "cohesionClusters" => {
                if language == Some("rust") {
                    return err("analysis.cohesionClusters is Java-only");
                }
                ToolResult::Json(json!({ "contract": COHESION_CONTRACT }))
            }
            "references" => ToolResult::Json(json!({ "contract": REFERENCES_CONTRACT })),
            "fieldClassification" => {
                if language == Some("rust") {
                    return err("analysis.fieldClassification is Java-only");
                }
                ToolResult::Json(json!({ "contract": FIELD_CLASSIFICATION_CONTRACT }))
            }
            "methodRegions" => {
                if language == Some("rust") {
                    return err("analysis.methodRegions is Java-only");
                }
                ToolResult::Json(json!({ "contract": METHOD_REGIONS_CONTRACT }))
            }
            "fieldInitializerClosure" => {
                if language == Some("rust") {
                    return err("analysis.fieldInitializerClosure is Java-only");
                }
                ToolResult::Json(json!({ "contract": FIELD_INIT_CLOSURE_CONTRACT }))
            }
            "implPartition" => {
                if language == Some("java") {
                    return err("analysis.implPartition is Rust-only");
                }
                ToolResult::Json(json!({ "contract": IMPL_PARTITION_CONTRACT }))
            }
            "topLevelDeps" => {
                if language == Some("java") {
                    return err("analysis.topLevelDeps is Rust-only");
                }
                ToolResult::Json(json!({ "contract": TOP_LEVEL_DEPS_CONTRACT }))
            }
            other => err(format!(
                "analysis.describe: unknown analysis `{other}` (available: cohesionClusters, references, fieldClassification, methodRegions, fieldInitializerClosure, implPartition, topLevelDeps)"
            )),
        }
    }
}

/// The `analysis.*` binding set.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AnalysisCohesionClusters) as Arc<dyn Tool>,
        Arc::new(AnalysisReferences) as Arc<dyn Tool>,
        Arc::new(AnalysisFieldClassification) as Arc<dyn Tool>,
        Arc::new(AnalysisMethodRegions) as Arc<dyn Tool>,
        Arc::new(AnalysisFieldInitializerClosure) as Arc<dyn Tool>,
        Arc::new(AnalysisImplPartition) as Arc<dyn Tool>,
        Arc::new(AnalysisTopLevelDeps) as Arc<dyn Tool>,
        Arc::new(AnalysisDescribe) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-DSL §5.2).
/// Compact index (§6.5 surface economics): one line per analysis; depth on
/// demand via analysis.describe.
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "analysis".to_string(),
        description: "Reduce-Rust-side analyses: ask a whole-class/corpus QUESTION and get back a small structured answer, instead of materializing raw facts into the cell. Each runs the reduction host-side; never writes; provenance syntax_only. Call analysis.describe({analysis}) for the full contract. Analyses: cohesionClusters (java) — partition a Java class's methods into cohesive decomposition seams (feeds java.extractClass); references — count symbol references across the workspace without full capture payloads (Java + Rust, pass language:\"rust\"); fieldClassification (java) — classify Java fields as constants/deps/mutable state with read/write sites; methodRegions (java) — analyze Java method-body regions before extract-method gates; fieldInitializerClosure (java) — transitive field/constant dependency closure for moved fields (prevents extract-time compile failures from missed initializer deps); implPartition (rust) — impl-method call/state graph for split planning (feeds rust.extractImplMethods); topLevelDeps (rust) — top-level item dependency graph + external reference hints + suggested clusters (feeds rust.extractItems). USE THESE rather than reconstructing reductions from code.query captures."
            .to_string(),
        declarations: r#"type CohesionCluster = { id: string; name_hint: string; item_names: string[]; move_fields: string[]; score: number; internal_field_touches: number; internal_calls: number; inbound_calls: number; outbound_calls: number; expected_wiring: "delegate" | "callback" | "source_instance" };
type CrossClusterCall = { from_cluster: string; to_cluster: string; from_method: string; to_method: string };
type ReferenceExample = { path: string; line: number; column: number; byte_start: number; byte_end: number; context: string; is_test_site: boolean; usage_kind: "type_reference" | "method_invocation" | "field_access" | "method_reference" | "import"; matched_name: string };
type FieldAccess = { method?: string; kind: "read" | "write"; line: number; column: number; context: string };
type FieldClassification = { name: string; type: string; owner_class?: string; visibility?: string; modifiers: string[]; annotations: string[]; is_static_final: boolean; is_mutable_instance: boolean; is_injected: boolean; injection_style?: "field_annotation" | "constructor_param"; is_provider: boolean; reads: number; writes: number; read_by: string[]; written_by: string[]; accesses: FieldAccess[] };
type ComponentTreeConsumption = { kind: "component_tree_argument" | "component_tree_receiver" | string; method: string; line: number; column: number };
type MethodRegionVar = { name: string; type: string; resolved_type?: string; mutated?: boolean; after_use_kinds?: string[]; component_tree_consumptions?: ComponentTreeConsumption[] };
type MethodRegionStatementSummary = { total_count: number; matched_count: number; returned_count: number; omitted_count: number; included: boolean; filter?: { include_nested_statement_regions?: boolean; start_line?: number; end_line?: number; contains?: string; limit?: number } };
type MethodRegion = { id: string; label?: string; kind: string; byte_start: number; byte_end: number; line_range: [number, number]; statement_count: number; preview: string; captures: MethodRegionVar[]; live_outs: MethodRegionVar[]; field_touches: { name: string; reads: number; writes: number }[]; enclosing_class_refs: string[]; this_super_refs: number; lambda_count: number; listener_call_count: number; non_local_control_flow: { kind: string; line: number; column: number }[]; extractability: { can_extract_with_current_tool: boolean; stop_reasons: string[]; live_out_count: number; mutated_capture_count: number; non_local_control_flow_count: number } };
type RustMethodNode = { name: string; reads: string[]; writes: string[]; calls: string[]; unresolved_callbacks: string[]; attrs: string[]; router?: string | null };
type RustFieldNode = { name: string; ty: string; shared_by: string[] };
type RustPartitionEdge = { from: string; to: string; kind: string };
type RustTopLevelItem = { name: string; kind: string; line_start: number; line_end: number; calls: string[]; type_refs: string[]; module_refs: string[]; global_refs: string[]; attrs: string[]; warnings: string[] };
type RustTopLevelEdge = { from: string; to: string; kind: string };
type RustTopLevelExternalRef = { item: string; path: string; line: number; context: string };
type RustTopLevelCluster = { id: string; items: string[]; reason: string; internal_edges: number; external_references: number };
declare const analysis: {
  /** Full contract (params, result vocabulary, recipe) for one analysis. Call before first use. Pass language ("java"|"rust") to filter. */
  describe(args: { analysis: string; language?: "java" | "rust" }): Promise<{ contract: string }>;
  /** Partition the first Java class's methods into cohesive clusters — the decomposition seams. Pick a high-score cluster and feed item_names/move_fields/expected_wiring into java.extractClass. The Rust-side answer to "what are the real seams"; do not rebuild it from code.query. */
  cohesionClusters(args: { file: string }): Promise<{ file: string; class: Record<string, unknown>; cluster_count: number; clusters: CohesionCluster[]; cross_cluster_calls: CrossClusterCall[]; provenance: "syntax_only" }>;
  /** Count symbol references across the workspace without returning full capture payloads. Default Java; Rust mode with language:"rust" (usage kinds: call, type_ref, path_ref, macro_use). Use before extraction to decide wrappers:true, distinguish forwarded fields, or estimate production/test blast radius. */
  references(args: { symbols: string[]; language?: "java" | "rust"; kinds?: Array<"type_reference" | "method_invocation" | "field_access" | "method_reference" | "import">; declaringClass?: string }): Promise<{ symbols: string[]; total_usages: number; unique_files: number; production_sites: number; test_sites: number; counts_by_symbol: Record<string, number>; files_by_symbol: Record<string, string[]>; examples_by_symbol: Record<string, ReferenceExample[]>; provenance: "syntax_only" }>;
  /** Classify Java fields before extraction: constants/dependencies/mutable state plus read/write sites by method. */
  fieldClassification(args: { file: string; fields?: string[]; className?: string }): Promise<{ file: string; language: "java"; content_sha256: string; source_len: number; fields: FieldClassification[]; provenance: "syntax_only" }>;
  /** Analyze one Java method's top-level statement regions and optional candidate ranges before extract-method. Use this for contiguity/live-out gates. */
  methodRegions(args: { file: string; method: string; className?: string; includeStatementRegions?: boolean; includeNestedStatementRegions?: boolean; statementLimit?: number; statementStartLine?: number; statementEndLine?: number; statementContains?: string; ranges?: Array<{ label?: string; startLine?: number; endLine?: number; byteStart?: number; byteEnd?: number }> }): Promise<{ file: string; language: "java"; content_sha256: string; source_len: number; class_name?: string; method_name: string; method_kind: string; method_line_range: [number, number]; body_line_range: [number, number]; parameters: MethodRegionVar[]; statement_region_summary: MethodRegionStatementSummary; statement_regions: MethodRegion[]; requested_ranges: MethodRegion[]; requested_contiguous: boolean; provenance: "syntax_only" }>;
  /** Compute transitive field/constant dependency closure for moved fields. For each static final field, finds constants referenced in its initializer and follows the chain transitively. Use before java.extractClass. */
  fieldInitializerClosure(args: { file: string; fields: string[]; className?: string }): Promise<{ file: string; fields: string[]; closure: Record<string, string[]>; provenance: "syntax_only" }>;
  /** (rust) Analyze one or more Rust impl blocks, returning a partition graph with per-method field reads/writes/calls, shared fields, and inferred edges. Feed into rust.extractImplMethods. */
  implPartition(args: { file: string; implName?: string; moduleName?: string }): Promise<{ file: string; impl_name: string; methods: RustMethodNode[]; fields: RustFieldNode[]; edges: RustPartitionEdge[]; provenance: "syntax_only" }>;
  /** (rust) Analyze top-level Rust items: dependency graph, external reference hints, and suggested clusters. The pre-extract survey before rust.extractItems. */
  topLevelDeps(args: { file: string; projectDir?: string; itemNames?: string[]; itemKinds?: string[] }): Promise<{ file: string; items: RustTopLevelItem[]; edges: RustTopLevelEdge[]; external_references: RustTopLevelExternalRef[]; suggested_clusters: RustTopLevelCluster[]; warnings: string[]; provenance: "syntax_only" }>;
};"#
            .to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as StdBTreeMap;
    use std::path::Path;
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

    fn json_of(result: ToolResult) -> Value {
        match result {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    // Two clean concerns sharing no fields → two clusters: a pricing concern
    // (price/discount over taxRate) and a tracking concern (track/counted
    // over counter).
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

    #[tokio::test]
    async fn cohesion_clusters_separates_two_concerns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/OrderService.java"), FIXTURE).unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisCohesionClusters
                .call(json!({ "file": "src/OrderService.java" }), &cx)
                .await,
        );
        assert_eq!(out["provenance"], "syntax_only", "{out}");
        let clusters = out["clusters"].as_array().unwrap();
        // taxRate-touching methods (price/discount) cluster apart from the
        // counter-touching methods (track/counted).
        assert!(clusters.len() >= 2, "expected >=2 clusters: {out}");
        let names_in = |c: &Value| -> Vec<String> {
            c["item_names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_str().unwrap().to_string())
                .collect()
        };
        let pricing = clusters
            .iter()
            .find(|c| names_in(c).contains(&"price".to_string()))
            .expect("a cluster owns price");
        assert!(
            names_in(pricing).contains(&"discount".to_string()),
            "{pricing}"
        );
        assert!(
            !names_in(pricing).contains(&"track".to_string()),
            "{pricing}"
        );
        assert!(
            pricing["move_fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "taxRate"),
            "pricing cluster should move taxRate: {pricing}"
        );
        // Each cluster carries the extract-ready vocabulary.
        assert!(pricing["score"].is_number());
        assert!(pricing["name_hint"].as_str().is_some());
        assert!(pricing["expected_wiring"].as_str().is_some());
    }

    #[tokio::test]
    async fn references_returns_counts_files_and_examples_without_full_usages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let main_pkg = root.join("src/main/java/com/acme");
        let test_pkg = root.join("src/test/java/com/acme");
        std::fs::create_dir_all(&main_pkg).unwrap();
        std::fs::create_dir_all(&test_pkg).unwrap();
        std::fs::write(
            main_pkg.join("Owner.java"),
            "package com.acme;\n\
             public class Owner {\n\
            \x20   public void targetMethod() {}\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            main_pkg.join("Caller.java"),
            "package com.acme;\n\
             class Caller {\n\
            \x20   void run(Owner owner) { owner.targetMethod(); }\n\
             }\n",
        )
        .unwrap();
        std::fs::write(
            test_pkg.join("OwnerTest.java"),
            "package com.acme;\n\
             class OwnerTest {\n\
            \x20   void run(Owner owner) { owner.targetMethod(); }\n\
             }\n",
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisReferences
                .call(
                    json!({
                        "symbols": ["targetMethod"],
                        "kinds": ["method_invocation"]
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(out["provenance"], "syntax_only", "{out}");
        assert!(
            out.get("usages").is_none(),
            "must not return full usages: {out}"
        );
        assert_eq!(out["symbols"], json!(["targetMethod"]));
        assert_eq!(out["counts_by_symbol"]["targetMethod"].as_u64().unwrap(), 2);
        assert_eq!(out["production_sites"].as_u64().unwrap(), 1);
        assert_eq!(out["test_sites"].as_u64().unwrap(), 1);
        let files = out["files_by_symbol"]["targetMethod"].as_array().unwrap();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(
            files
                .iter()
                .any(|f| f == "src/main/java/com/acme/Caller.java"),
            "{files:?}"
        );
        assert!(
            files
                .iter()
                .any(|f| f == "src/test/java/com/acme/OwnerTest.java"),
            "{files:?}"
        );
        assert!(
            out["examples_by_symbol"]["targetMethod"][0]["path"]
                .as_str()
                .unwrap()
                .starts_with("src/"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn field_classification_reports_state_and_read_write_sites() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Widget.java"),
            r#"package com.acme;

class Widget {
    @Inject
    private final Repo repo;
    private final Service service;
    private int count;
    private static final String KIND = "widget";

    @Inject
    Widget(Repo repo, Service service) {
        this.repo = repo;
        this.service = service;
    }

    void bump() {
        count = count + 1;
        count++;
        log(KIND);
    }

    void show() {
        log(count);
        repo.load();
        service.refresh();
    }

    void log(Object value) {}
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisFieldClassification
                .call(
                    json!({
                        "file": "Widget.java",
                        "fields": ["repo", "service", "count", "KIND"],
                        "className": "Widget"
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(out["provenance"], "syntax_only", "{out}");
        let fields = out["fields"].as_array().unwrap();
        let repo = fields.iter().find(|field| field["name"] == "repo").unwrap();
        assert_eq!(repo["is_injected"], true, "{repo}");
        assert_eq!(repo["injection_style"], "field_annotation", "{repo}");
        assert_eq!(repo["is_mutable_instance"], false, "{repo}");
        assert_eq!(repo["read_by"], json!(["show"]), "{repo}");

        let service = fields
            .iter()
            .find(|field| field["name"] == "service")
            .unwrap();
        assert_eq!(service["is_injected"], true, "{service}");
        assert_eq!(service["injection_style"], "constructor_param", "{service}");
        assert_eq!(service["read_by"], json!(["show"]), "{service}");

        let count = fields
            .iter()
            .find(|field| field["name"] == "count")
            .unwrap();
        assert_eq!(count["is_mutable_instance"], true, "{count}");
        assert!(
            count["reads"].as_u64().unwrap() >= 2,
            "expected reads: {count}"
        );
        assert!(
            count["writes"].as_u64().unwrap() >= 2,
            "expected writes: {count}"
        );
        assert!(
            count["written_by"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "bump"),
            "{count}"
        );

        let kind = fields.iter().find(|field| field["name"] == "KIND").unwrap();
        assert_eq!(kind["is_static_final"], true, "{kind}");
        assert_eq!(kind["is_mutable_instance"], false, "{kind}");
        assert_eq!(kind["read_by"], json!(["bump"]), "{kind}");
    }

    #[tokio::test]
    async fn method_regions_reports_requested_range_gates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Stage.java"),
            r#"package com.acme;

class Stage {
    private int count;

    void build(int seed) {
        int first = seed + 1;
        int second = first + count;
        int third = second + 1;
        log(first);
        log(second);
        log(third);
    }

    void log(Object value) {}
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisMethodRegions
                .call(
                    json!({
                        "file": "Stage.java",
                        "method": "build",
                        "className": "Stage",
                        "ranges": [{ "label": "candidate", "startLine": 7, "endLine": 8 }]
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(out["file"], "Stage.java", "{out}");
        assert_eq!(out["provenance"], "syntax_only", "{out}");
        assert_eq!(out["statement_region_summary"]["total_count"], 6, "{out}");
        assert_eq!(
            out["statement_region_summary"]["returned_count"], 6,
            "{out}"
        );
        assert!(
            out["statement_regions"].as_array().unwrap().len() >= 6,
            "{out}"
        );
        let range = &out["requested_ranges"][0];
        assert_eq!(
            range["extractability"]["can_extract_with_current_tool"], false,
            "{range}"
        );
        assert!(
            range["extractability"]["stop_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reason| reason == "multi_live_out_needs_record"),
            "{range}"
        );
        assert!(
            range["live_outs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|var| var["name"] == "first"),
            "{range}"
        );
        assert!(
            range["field_touches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field["name"] == "count" && field["reads"] == 1),
            "{range}"
        );
    }

    #[tokio::test]
    async fn method_regions_can_return_compact_filtered_statement_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Stage.java"),
            r#"package com.acme;

class Stage {
    void build() {
        int alpha = 1;
        int beta = 2;
        int gamma = alpha + beta;
        log(gamma);
    }

    void log(Object value) {}
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisMethodRegions
                .call(
                    json!({
                        "file": "Stage.java",
                        "method": "build",
                        "className": "Stage",
                        "includeStatementRegions": false,
                        "statementContains": "beta",
                        "statementLimit": 1
                    }),
                    &cx,
                )
                .await,
        );

        assert_eq!(out["statement_regions"], json!([]), "{out}");
        assert_eq!(out["statement_region_summary"]["total_count"], 4, "{out}");
        assert_eq!(out["statement_region_summary"]["matched_count"], 2, "{out}");
        assert_eq!(
            out["statement_region_summary"]["returned_count"], 0,
            "{out}"
        );
        assert_eq!(out["statement_region_summary"]["omitted_count"], 4, "{out}");
        assert_eq!(out["statement_region_summary"]["included"], false, "{out}");
        assert_eq!(
            out["statement_region_summary"]["filter"]["contains"], "beta",
            "{out}"
        );
    }

    #[tokio::test]
    async fn method_regions_can_return_nested_filtered_statement_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("NestedStage.java"),
            r#"package com.acme;

class NestedStage {
    void build(boolean keepGoing) {
        while (keepGoing) {
            int before = 1;
            int targetStageValue = before + 1;
            log(targetStageValue);
        }
    }

    void log(Object value) {}
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisMethodRegions
                .call(
                    json!({
                        "file": "NestedStage.java",
                        "method": "build",
                        "className": "NestedStage",
                        "includeNestedStatementRegions": true,
                        "statementContains": "targetStageValue",
                        "statementLimit": 10
                    }),
                    &cx,
                )
                .await,
        );

        let kinds = out["statement_regions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|region| region["kind"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"local_variable_declaration"), "{out}");
        assert!(kinds.contains(&"expression_statement"), "{out}");
        assert_eq!(
            out["statement_region_summary"]["filter"]["include_nested_statement_regions"], true,
            "{out}"
        );
    }

    const CLOSURE_FIXTURE: &str = r#"package com.acme;

public class ClosureTest {
    private static final String A = "a";
    private static final String B = A;
    private static final String C = B;
    private static final String D = C + "extra";
    private static final String E = "independent";
    private int counter;
}
"#;

    #[tokio::test]
    async fn field_initializer_closure_finds_transitive_deps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("ClosureTest.java"), CLOSURE_FIXTURE).unwrap();
        let cx = cx_in(root);

        // B → A, so B's closure should contain A
        let out = json_of(
            AnalysisFieldInitializerClosure
                .call(json!({ "file": "ClosureTest.java", "fields": ["B"] }), &cx)
                .await,
        );
        let closure = &out["closure"];
        assert!(closure["B"].is_array(), "{out}");
        let b_deps: Vec<&str> = closure["B"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(b_deps.contains(&"A"), "{out}");

        // C → B → A, so C's closure should contain [A, B]
        let out = json_of(
            AnalysisFieldInitializerClosure
                .call(json!({ "file": "ClosureTest.java", "fields": ["C"] }), &cx)
                .await,
        );
        let c_deps: Vec<&str> = out["closure"]["C"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(c_deps.contains(&"A"), "{out}");
        assert!(c_deps.contains(&"B"), "{out}");

        // D → C → B → A, full chain
        let out = json_of(
            AnalysisFieldInitializerClosure
                .call(json!({ "file": "ClosureTest.java", "fields": ["D"] }), &cx)
                .await,
        );
        let d_deps: Vec<&str> = out["closure"]["D"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(d_deps.contains(&"A"), "{out}");
        assert!(d_deps.contains(&"B"), "{out}");
        assert!(d_deps.contains(&"C"), "{out}");

        // E has no constant deps → should not appear in closure
        let out = json_of(
            AnalysisFieldInitializerClosure
                .call(json!({ "file": "ClosureTest.java", "fields": ["E"] }), &cx)
                .await,
        );
        assert!(
            !out["closure"].as_object().unwrap().contains_key("E"),
            "{out}"
        );

        // Non-static field → no deps
        let out = json_of(
            AnalysisFieldInitializerClosure
                .call(
                    json!({ "file": "ClosureTest.java", "fields": ["counter"] }),
                    &cx,
                )
                .await,
        );
        assert!(
            !out["closure"].as_object().unwrap().contains_key("counter"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn describe_returns_contract_and_rejects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let cx = cx_in(dir.path());
        let out = json_of(
            AnalysisDescribe
                .call(json!({ "analysis": "cohesionClusters" }), &cx)
                .await,
        );
        assert!(
            out["contract"]
                .as_str()
                .unwrap()
                .contains("expected_wiring"),
            "{out}"
        );
        let refs = json_of(
            AnalysisDescribe
                .call(json!({ "analysis": "references" }), &cx)
                .await,
        );
        assert!(
            refs["contract"]
                .as_str()
                .unwrap()
                .contains("counts_by_symbol"),
            "{refs}"
        );
        let unknown = AnalysisDescribe
            .call(json!({ "analysis": "bogus" }), &cx)
            .await;
        assert!(
            matches!(unknown, ToolResult::Error(ref e) if e.contains("available: cohesionClusters, references, fieldClassification")),
            "{unknown:?}"
        );
        // Language filter: Java-only analysis refuses rust filter.
        let jrust = AnalysisDescribe
            .call(
                json!({ "analysis": "cohesionClusters", "language": "rust" }),
                &cx,
            )
            .await;
        assert!(
            matches!(jrust, ToolResult::Error(ref e) if e.contains("Java-only")),
            "{jrust:?}"
        );
        // Rust-only: implPartition resolves, and rejects java filter.
        let ip = json_of(
            AnalysisDescribe
                .call(json!({ "analysis": "implPartition" }), &cx)
                .await,
        );
        assert!(
            ip["contract"]
                .as_str()
                .unwrap()
                .contains("impl-method call/state graph"),
            "{ip}"
        );
        let ipj = AnalysisDescribe
            .call(
                json!({ "analysis": "implPartition", "language": "java" }),
                &cx,
            )
            .await;
        assert!(
            matches!(ipj, ToolResult::Error(ref e) if e.contains("Rust-only")),
            "{ipj:?}"
        );
        // topLevelDeps resolves.
        let td = json_of(
            AnalysisDescribe
                .call(json!({ "analysis": "topLevelDeps" }), &cx)
                .await,
        );
        assert!(
            td["contract"]
                .as_str()
                .unwrap()
                .contains("pre-extract survey"),
            "{td}"
        );
    }

    // --- Rust analysis tests ---

    const RUST_IMPL_FIXTURE: &str = r#"pub struct Server {
    counter: u64,
    name: String,
}

impl Server {
    pub fn new(name: String) -> Server {
        Server { counter: 0, name }
    }

    pub fn increment(&mut self) {
        self.counter += 1;
    }

    pub fn count(&self) -> u64 {
        self.counter
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn reset(&mut self) {
        self.counter = 0;
        self.log_reset();
    }

    fn log_reset(&self) {
        println!("reset: {}", self.name());
    }
}
"#;

    #[tokio::test]
    async fn impl_partition_returns_methods_and_fields() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/server.rs"), RUST_IMPL_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisImplPartition
                .call(
                    json!({ "file": "src/server.rs", "implName": "Server" }),
                    &cx,
                )
                .await,
        );
        assert_eq!(out["provenance"], "syntax_only", "{out}");
        assert_eq!(out["impl_name"], "Server", "{out}");

        let methods = out["methods"].as_array().unwrap();
        assert!(methods.len() >= 5, "expected >=5 methods: {out}");
        // "increment" method should write counter
        let inc = methods
            .iter()
            .find(|m| m["name"].as_str() == Some("increment"))
            .expect("increment method exists");
        assert!(
            inc["writes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w == "counter"),
            "increment should write counter: {inc}"
        );

        let fields = out["fields"].as_array().unwrap();
        assert!(fields.len() >= 2, "expected >=2 fields: {out}");

        // edges should reflect call and field-touch relationships
        // (uses "method:<name>" / "field:<name>" prefix convention)
        let edges = out["edges"].as_array().unwrap();
        // reset writes counter and calls log_reset
        let reset_edges: Vec<_> = edges
            .iter()
            .filter(|e| e["from"].as_str() == Some("method:reset"))
            .collect();
        assert!(
            reset_edges.iter().any(|e| {
                e["to"].as_str() == Some("field:counter") && e["kind"].as_str() == Some("writes")
            }),
            "reset should write counter: {reset_edges:?}"
        );

        // "impl Server" form also works
        let out2 = json_of(
            AnalysisImplPartition
                .call(
                    json!({ "file": "src/server.rs", "implName": "impl Server" }),
                    &cx,
                )
                .await,
        );
        assert_eq!(out2["impl_name"], "impl Server", "{out2}");
        assert_eq!(out2["methods"].as_array().unwrap().len(), methods.len());
    }

    #[tokio::test]
    async fn impl_partition_requires_impl_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/server.rs"), RUST_IMPL_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let result = AnalysisImplPartition
            .call(json!({ "file": "src/server.rs" }), &cx)
            .await;
        assert!(
            matches!(result, ToolResult::Error(ref e) if e.contains("implName")),
            "{result:?}"
        );
    }

    const RUST_TOP_LEVEL_FIXTURE: &str = r#"pub mod sub;

pub fn helper() -> u32 { 42 }

pub struct Config {
    pub debug: bool,
}

pub fn init() -> Config {
    let _ = helper();
    Config { debug: true }
}

pub async fn run(config: &Config) {
    if config.debug {
        println!("running");
    }
}
"#;

    #[tokio::test]
    async fn top_level_deps_reports_dependency_graph() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), RUST_TOP_LEVEL_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisTopLevelDeps
                .call(json!({ "file": "src/lib.rs" }), &cx)
                .await,
        );
        assert_eq!(out["provenance"], "syntax_only", "{out}");

        let items = out["items"].as_array().unwrap();
        // Should have at least the function items and struct item; mod is also top-level
        assert!(items.len() >= 4, "expected >=4 items: {out}");

        // "init" calls "helper" -> there should be a calls edge
        let edges = out["edges"].as_array().unwrap();
        let init_edges: Vec<_> = edges
            .iter()
            .filter(|e| e["from"].as_str() == Some("init"))
            .collect();
        assert!(
            init_edges
                .iter()
                .any(|e| e["to"].as_str() == Some("helper") && e["kind"].as_str() == Some("calls")),
            "init should call helper: {init_edges:?}"
        );

        // suggested_clusters should exist
        let clusters = out["suggested_clusters"].as_array().unwrap();
        assert!(!clusters.is_empty(), "expected clusters: {out}");
    }

    #[tokio::test]
    async fn top_level_deps_with_item_name_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), RUST_TOP_LEVEL_FIXTURE).unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisTopLevelDeps
                .call(
                    json!({
                        "file": "src/lib.rs",
                        "itemNames": ["init", "helper"]
                    }),
                    &cx,
                )
                .await,
        );
        let items = out["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "should only have init and helper: {out}");
        let names: Vec<_> = items
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"init".to_string()));
        assert!(names.contains(&"helper".to_string()));
    }

    #[tokio::test]
    async fn references_rust_mode_counts_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            r#"pub fn hello() -> String { "hello".into() }
pub fn greet() { let _ = hello(); }
pub struct Foo { x: i32 }
impl Foo { pub fn new() -> Foo { Foo { x: 0 } } }
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("tests/test_lib.rs"),
            r#"use crate::hello;
#[test]
fn test_hello() { let s = hello(); }
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisReferences
                .call(
                    json!({
                        "symbols": ["hello", "Foo"],
                        "language": "rust"
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(out["provenance"], "syntax_only", "{out}");
        // "hello" should have at least 3 references
        let hello_count = out["counts_by_symbol"]["hello"].as_u64().unwrap_or(0);
        assert!(hello_count >= 3, "hello should have >=3 refs: {out}");
        // "Foo" appears as type_ref several times
        let foo_count = out["counts_by_symbol"]["Foo"].as_u64().unwrap_or(0);
        assert!(foo_count >= 2, "Foo should have >=2 refs: {out}");
        assert!(out["production_sites"].as_u64().unwrap_or(0) > 0);
        assert!(out["test_sites"].as_u64().unwrap_or(0) > 0);
        // examples should carry rust usage kinds
        let hello_examples = out["examples_by_symbol"]["hello"].as_array().unwrap();
        assert!(
            hello_examples
                .iter()
                .any(|e| e["usage_kind"].as_str() == Some("call")),
            "{hello_examples:?}"
        );
    }

    #[tokio::test]
    async fn references_java_mode_still_works() {
        // regression: adding language param doesn't break existing Java path
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/Test.java"),
            r#"public class Test {
    public void hello() {}
    public void world() { hello(); }
}
"#,
        )
        .unwrap();
        let cx = cx_in(&root);

        let out = json_of(
            AnalysisReferences
                .call(
                    json!({
                        "symbols": ["hello"],
                        "language": "java"
                    }),
                    &cx,
                )
                .await,
        );
        // Should find at least the call in world()
        let count = out["counts_by_symbol"]["hello"].as_u64().unwrap_or(0);
        assert!(count >= 1, "hello should have >=1 ref in Java mode: {out}");
        assert_eq!(out["provenance"], "syntax_only", "{out}");
    }
}
