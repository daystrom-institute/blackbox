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
                "file": { "type": "string", "description": "Workspace-relative .java file; the FIRST class declaration is analyzed." }
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
            ToolResult::Json(json!({
                "file": params.file,
                "class": v.get("class").cloned().unwrap_or(json!({})),
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
        "Count Java references to one or more simple symbols across the workspace without returning full capture payloads. Returns per-symbol counts, files, and up to five example sites per symbol. Use this when deciding whether extracted methods need wrappers, whether a candidate field is only forwarded, or whether a seam has external callers. Pure; syntax_only; never writes."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbols": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Simple Java symbols to count, e.g. method, field, or type names."
                },
                "kinds": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["type_reference", "method_invocation", "field_access", "method_reference", "import"]
                    },
                    "description": "Optional usage-kind filter."
                },
                "declaringClass": {
                    "type": "string",
                    "description": "Optional receiver filter for method invocations: include calls whose receiver plausibly resolves to a field of this class."
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
        let root = cx.root.clone();
        bro_tools::tool::call_blocking(move || {
            let requested_symbols = params.symbols.clone();
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
  file: string   workspace-relative .java file

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
  files_by_symbol:  { [symbol]: workspace-relative files[] }
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

#[async_trait]
impl Tool for AnalysisDescribe {
    fn name(&self) -> &str {
        "analysis.describe"
    }
    fn description(&self) -> &str {
        "Full contract for one analysis.* binding (params, result vocabulary, recipe). The namespace index lists analyses one line each; call this before first use."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "analysis": { "type": "string", "description": "Analysis name, e.g. \"cohesionClusters\"." }
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
        match analysis {
            "cohesionClusters" => ToolResult::Json(json!({ "contract": COHESION_CONTRACT })),
            "references" => ToolResult::Json(json!({ "contract": REFERENCES_CONTRACT })),
            other => err(format!(
                "analysis.describe: unknown analysis `{other}` (available: cohesionClusters, references)"
            )),
        }
    }
}

/// The `analysis.*` binding set.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(AnalysisCohesionClusters) as Arc<dyn Tool>,
        Arc::new(AnalysisReferences) as Arc<dyn Tool>,
        Arc::new(AnalysisDescribe) as Arc<dyn Tool>,
    ]
}

/// Hand-authored namespace documentation + TS declarations (cell-DSL §5.2).
/// Compact index (§6.5 surface economics): one line per analysis; depth on
/// demand via analysis.describe.
pub fn namespace_description() -> bro_code_mode::ToolNamespaceDescription {
    bro_code_mode::ToolNamespaceDescription {
        name: "analysis".to_string(),
        description: "Reduce-Rust-side analyses: ask a whole-class/corpus QUESTION and get back a small structured answer, instead of materializing raw facts into the cell. Each runs the reduction host-side; never writes; provenance syntax_only. Call analysis.describe({analysis}) for the full contract. Analyses: cohesionClusters — partition a Java class's methods into cohesive decomposition seams (feeds java.extractClass); references — count Java symbol references across the workspace without full capture payloads. USE THESE rather than reconstructing reductions from code.query captures."
            .to_string(),
        declarations: r#"type CohesionCluster = { id: string; name_hint: string; item_names: string[]; move_fields: string[]; score: number; internal_field_touches: number; internal_calls: number; inbound_calls: number; outbound_calls: number; expected_wiring: "delegate" | "callback" | "source_instance" };
type CrossClusterCall = { from_cluster: string; to_cluster: string; from_method: string; to_method: string };
type ReferenceExample = { path: string; line: number; column: number; byte_start: number; byte_end: number; context: string; is_test_site: boolean; usage_kind: "type_reference" | "method_invocation" | "field_access" | "method_reference" | "import"; matched_name: string };
declare const analysis: {
  /** Full contract (params, result vocabulary, recipe) for one analysis. Call before first use. */
  describe(args: { analysis: string }): Promise<{ contract: string }>;
  /** Partition the first Java class's methods into cohesive clusters — the decomposition seams. Pick a high-score cluster and feed item_names/move_fields/expected_wiring into java.extractClass. The Rust-side answer to "what are the real seams"; do not rebuild it from code.query. */
  cohesionClusters(args: { file: string }): Promise<{ file: string; class: Record<string, unknown>; cluster_count: number; clusters: CohesionCluster[]; cross_cluster_calls: CrossClusterCall[]; provenance: "syntax_only" }>;
  /** Count Java references to simple symbols across the workspace without returning full capture payloads. Use before extraction to decide wrappers:true, distinguish forwarded fields, or estimate production/test blast radius. */
  references(args: { symbols: string[]; kinds?: Array<"type_reference" | "method_invocation" | "field_access" | "method_reference" | "import">; declaringClass?: string }): Promise<{ symbols: string[]; total_usages: number; unique_files: number; production_sites: number; test_sites: number; counts_by_symbol: Record<string, number>; files_by_symbol: Record<string, string[]>; examples_by_symbol: Record<string, ReferenceExample[]>; provenance: "syntax_only" }>;
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
            matches!(unknown, ToolResult::Error(ref e) if e.contains("available: cohesionClusters, references")),
            "{unknown:?}"
        );
    }
}
