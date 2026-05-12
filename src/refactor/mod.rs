use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Tree};

mod rust;
use rust::*;
mod java;
use java::*;
pub(crate) mod plan_slot;
pub(crate) mod rust_partition;
pub(crate) mod rust_public_api;
pub(crate) mod rust_deep;
pub(crate) mod rust_extract_trait;
pub(crate) mod rust_migrate_types;
pub(crate) mod rust_lift_free;
pub(crate) mod rust_match_strategy;
pub(crate) mod rust_error_migrate;
pub(crate) mod rust_move_fields;
pub(crate) mod rust_delegate_field;
pub(crate) mod rust_update_callers;
pub(crate) mod rust_ra_move_item;
pub(crate) mod rust_ra_classify_callbacks;
pub(crate) mod rust_compile_fix;
pub(crate) mod rust_warning_markers;
pub(crate) mod rust_inline_mod;
pub(crate) mod rust_extract_to_submodule;
pub(crate) mod rust_move_with_callers;

#[cfg(test)]
mod tests;

use crate::chunker;
use crate::chunker::code::{language_for_path, parser_for_language};
use crate::entity_ref;
use crate::lsp::LspSessionManager;
use crate::projects::ProjectRecord;

/// Cross-cutting context threaded through `plan` so language-scoped
/// plan kinds can reach long-lived services (currently the LSP
/// session pool). Optional fields keep the entry points usable from
/// tests and one-off CLIs that don't bring up the daemon.
#[derive(Default, Clone)]
pub struct PlanContext {
    pub lsp: Option<LspSessionManager>,
}

const BLACKBOX_SERVICE_ENV_VARS: &[&str] = &[
    "BBOX_PORT",
    "BLACKBOX_MCP_NAME",
    "BLACKBOX_MCP_URL",
    "BLACKBOX_STATE_DIR",
    "BLACKBOX_KNOWLEDGE_PATH",
    "BLACKBOX_THREADS_PATH",
    "BLACKBOX_NOTES_PATH",
    "BLACKBOX_GLOBAL_CLAUDE_MD",
    "BLACKBOX_GLOBAL_CODEX_MD",
    "BLACKBOX_GLOBAL_GEMINI_MD",
    "BLACKBOX_BACKUP_DIR",
    "BRO_HOME",
    "TRANSCRIPT_SEARCH_ROOTS",
    "TRANSCRIPT_SEARCH_CODEX_ROOT",
    "TRANSCRIPT_SEARCH_INDEX_PATH",
];

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorStatusParams {
    /// Source file to inspect. Relative paths resolve against project_dir or cwd.
    pub file: String,
    /// Optional project root used to resolve relative paths.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Optional exact item names to return.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional item kinds to return, e.g. function_item, struct_item, impl_method.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Maximum matching items to return. Defaults to 200 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include syntax attributes in returned items. Defaults true.
    #[serde(default)]
    pub include_attributes: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorProjectRefsParams {
    /// Source file to chunk. Relative paths resolve against project_dir or cwd.
    pub file: String,
    /// Optional project root used to resolve relative paths and compute the project_file project id.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Optional substring filter applied to chunk content, symbol, chunk kind, and entity_ref.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum matching chunks to return. Defaults to 100 and is capped at 1000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include chunk content excerpts. Defaults true.
    #[serde(default)]
    pub include_excerpt: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct JavaFieldSpec {
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(rename = "type")]
    pub type_name: String,
    pub name: String,
    #[serde(default, rename = "final")]
    pub final_field: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct JavaParameterSpec {
    #[serde(rename = "type")]
    pub type_name: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct CapturedVariable {
    pub name: String,
    pub kind: String,
    pub source_type: String,
    pub source_visibility: String,
    /// Captured field is declared with `final`. When false, promoting it to a
    /// constructor parameter snapshots the value at construction time and the
    /// target sees stale data after subsequent source-side mutations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_mutable: bool,
    /// Captured field is declared `static final`. The composite plan should
    /// move it as a constant (initializer-preserving) rather than promote it
    /// to an instance field on the target.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_static_final: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FieldAccessSite {
    pub line: usize,
    pub column: usize,
    pub kind: String,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RemainingFieldAccessor {
    pub field: String,
    pub accesses: Vec<FieldAccessSite>,
}

/// Single call site for an external or inherited method dependency
/// reported by `extract_java_methods` analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ExtractedCallSite {
    pub line: usize,
    pub column: usize,
    pub in_method: String,
    /// "direct" or "lambda" — `lambda` means the call is inside a
    /// `lambda_expression` ancestor, before the enclosing method declaration.
    pub context: String,
}

/// A method call inside the extracted set that resolves to a method on the
/// source class but is NOT part of the extracted set. These calls become
/// unresolved references on the target class after extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ExternalCall {
    pub method: String,
    pub signature: String,
    /// True when the signature was reconstructed best-effort because the
    /// declaring method node could not be fully recovered.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signature_partial: bool,
    /// G8: source method's declared visibility. One of "public",
    /// "protected", "package", "private", or absent when modifiers
    /// could not be recovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_visibility: Option<String>,
    /// G8: source method is declared `static`. Public-static calls
    /// are auto-qualified to `<SourceClass>.<method>(...)` at G19;
    /// non-static externals need other resolution (callback, delegate,
    /// inject source instance, drop).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_is_static: bool,
    /// G8: recommended resolution for the operator. One of:
    /// `cross_class_static_call` (auto-resolved at apply by G19),
    /// `add_to_item_names` (move the method with the cluster),
    /// `add_to_callback_externals` (thread as functional-interface),
    /// `inject_source_instance` (operator wires source as a dep on target),
    /// `drop_the_call` (void return, side effect doesn't apply).
    /// Absent when the planner cannot recommend one resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_resolution: Option<String>,
    pub call_sites: Vec<ExtractedCallSite>,
}

/// A method call inside the extracted set that resolves to a method declared
/// on a superclass / implemented interface in the project type index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct InheritedDependency {
    pub method: String,
    pub source: String,
    /// "class" or "interface".
    pub source_kind: String,
    pub call_sites: Vec<ExtractedCallSite>,
}

/// Rust deep-analysis output for `extract_rust_impl_methods` with `deep_analysis: true`.
/// Populated when `semantic_status` is `indexed_hints`; all arrays are empty for
/// `syntax_only` plans.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct DeepAnalysis {
    /// Struct fields accessed through `self` in the extracted methods, with
    /// borrow-context classification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured_self_fields: Vec<CapturedSelfField>,
    /// Calls to `self.m()` or `Self::m()` that are NOT in the extracted set
    /// and therefore remain as unresolved dependencies after extraction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_callbacks: Vec<UnresolvedCallback>,
    /// Names of impl-level generic type or const parameters that appear in the
    /// extracted method signatures or bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_generics: Vec<String>,
    /// Bounds for the `inherited_generics` params (from the impl type_parameters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_bounds: Vec<String>,
    /// Impl-level lifetime parameters referenced in the extracted methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured_lifetimes: Vec<String>,
}

/// A struct field accessed through `self` in an extracted impl method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct CapturedSelfField {
    pub field_name: String,
    pub field_type: String,
    /// Borrow-context classification: `"copy"`, `"unique_ref"`, `"move"`,
    /// `"unknown_copy"`, or `"interior_mutation_call"`.
    pub borrow_context: String,
}

/// A call to a same-impl method that is NOT in the extracted set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct UnresolvedCallback {
    /// Callee expression, e.g. `"self.helper"` or `"Self::build"`.
    pub callee: String,
    /// Number of call sites in the extracted methods.
    pub count: usize,
}

#[derive(Debug, Default, Clone, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanParams {
    /// Supported generic or language-scoped plan kind. Pull sm-refactor first.
    pub kind: String,
    /// Source file. Relative paths resolve against project_dir or cwd.
    pub source: String,
    /// Optional target file. Required by plan kinds that write or copy elsewhere.
    #[serde(default)]
    pub target: Option<String>,
    /// Optional exact item names. Meaning is plan-specific.
    #[serde(default)]
    pub item_names: Option<Vec<String>>,
    /// Optional syntax item kinds. Values are language-specific.
    #[serde(default)]
    pub item_kinds: Option<Vec<String>>,
    /// Optional plan-specific implementation/scope filter.
    #[serde(default)]
    pub impl_name: Option<String>,
    /// Optional plan-specific module/declaration name.
    #[serde(default)]
    pub module_name: Option<String>,
    /// Optional plan-specific visibility value.
    #[serde(default)]
    pub visibility: Option<String>,
    /// Optional plan-specific import/use path.
    #[serde(default)]
    pub use_path: Option<String>,
    /// Optional plan-specific router name.
    #[serde(default)]
    pub router_name: Option<String>,
    /// Optional plan-specific router call expression.
    #[serde(default)]
    pub router_call: Option<String>,
    /// Optional plan-specific router export helper name.
    #[serde(default)]
    pub router_export_name: Option<String>,
    /// Optional plan-specific text inserted before generated target content.
    #[serde(default)]
    pub target_prelude: Option<String>,
    /// Optional exact text to replace. Used by generic text replacement plans.
    #[serde(default)]
    pub old_text: Option<String>,
    /// Optional replacement text or complete file content, depending on plan kind.
    #[serde(default)]
    pub new_text: Option<String>,
    /// Optional toggle for replacing every exact match instead of requiring one.
    #[serde(default)]
    pub replace_all: Option<bool>,
    /// Optional TOML table name for structured TOML edit plans.
    #[serde(default)]
    pub toml_table: Option<String>,
    /// Optional TOML key/value entries for structured TOML edit plans.
    #[serde(default)]
    pub toml_entries: Option<BTreeMap<String, serde_json::Value>>,
    /// Java field declarations for add_java_fields.
    #[serde(default)]
    pub fields: Option<Vec<JavaFieldSpec>>,
    /// Java constructor parameters for add_java_constructor.
    #[serde(default)]
    pub parameters: Option<Vec<JavaParameterSpec>>,
    /// Java constructor helper: assign this.<param> = <param>.
    #[serde(default)]
    pub assign_to_fields: Option<bool>,
    /// Java field names to move with extract_java_class.
    #[serde(default)]
    pub move_fields: Option<Vec<String>>,
    /// Java delegate field name for caller rewrites or source-side delegate wiring.
    #[serde(default)]
    pub delegate_field: Option<String>,
    /// Java delegate type for source-side delegate wiring.
    #[serde(default)]
    pub delegate_type: Option<String>,
    /// Whether move_java_constant should leave a copy of the constant in the
    /// source file. Default false.
    #[serde(default)]
    pub keep_copy: Option<bool>,
    /// Pessimistic-analysis umbrella for Java extraction/move plans. When
    /// true, populates `remaining_source_accessors` (move_java_field),
    /// `external_calls` and `inherited_dependencies` (extract_java_methods,
    /// extract_java_class). Default false keeps the response lean for simple
    /// extractions where the operator already knows the cluster is
    /// self-contained. Set true for complex clusters that touch shared
    /// fields, source-class methods, or inherited members — the reports
    /// surface silent-miscompile risks (wrong-instance calls, undeclared
    /// field references, missing inherited-method hookup) before apply.
    #[serde(default)]
    pub deep_analysis: Option<bool>,
    /// Gap 18: when `extract_java_class` runs with `move_fields` non-empty
    /// AND `deep_analysis: true`, also rewrite each remaining source-side
    /// read/write of a moved field through the delegate's getter/setter,
    /// and emit matching getter/setter declarations on the target. Defaults
    /// to true when `deep_analysis` is true; defaults to false (no rewrite)
    /// when `deep_analysis` is false. Set explicitly to false to keep the
    /// `remaining_source_accessors` report but skip the rewrites — useful
    /// when the operator wants to fix the call sites by hand or revisit the
    /// move strategy.
    #[serde(default)]
    pub rewrite_remaining_accessors: Option<bool>,
    /// Optional project root used to resolve relative paths.
    #[serde(default)]
    pub project_dir: Option<String>,
    /// Optional filename for writing the full RefactorPlan JSON to disk.
    /// When set, the planner writes the plan and returns a compact
    /// `RefactorPlanSummary` instead of the full plan body. Use this for
    /// large refactors whose plan JSON exceeds the MCP transport limit.
    /// Must be a relative filename (no path separators that escape the
    /// slot); it resolves under `$BLACKBOX_STATE_DIR/refactor/plans/`.
    /// Absolute paths and slot-escaping paths are rejected. Apply the
    /// saved plan via `bbox_refactor_apply(plan_path=<same value>)`.
    #[serde(default)]
    pub output_path: Option<String>,
    /// `extract_java_class`: source-class method names that the extracted
    /// methods call but the operator wants threaded back to source as
    /// functional-interface callbacks rather than appearing as
    /// `// FIXME: external call` markers. For each name listed, the planner
    /// picks a functional interface from the method's signature
    /// (no-arg void → `Runnable`, single-arg void → `Consumer<T>`,
    /// no-arg non-void → `Supplier<R>`, single-arg non-void →
    /// `Function<T,R>`), adds the matching `private final` field +
    /// constructor parameter to the target, rewrites each call site in the
    /// extracted bodies to the interface's invocation method
    /// (`.run()` / `.accept(arg)` / `.get()` / `.apply(arg)`), and appends
    /// a `this::<method>` argument to the source-side `new <Target>(...)`
    /// wiring expression. Two-plus-arg methods are not supported yet; the
    /// planner refuses with `error.bad_input(code=callback_arity_unsupported)`.
    #[serde(default)]
    pub callback_externals: Option<Vec<String>>,
    /// `lombokify_java_class`: how to handle a primitive `boolean` field
    /// whose hand-rolled getter uses `get-` prefix (e.g., `getShowFlag()`
    /// for `boolean showFlag`). Lombok's @Getter generates `isShowFlag()`,
    /// so dropping the original would break every caller of the get-prefix
    /// form. Values: `skip` (default — leave the field alone, fall back to
    /// per-field placement on others), `bridge` (drop original, emit
    /// `public boolean getXxx() { return isXxx(); }` after @Getter so
    /// callers continue to compile), `rename` (drop original and accept
    /// Lombok's name change — only safe when callers don't exist or are
    /// being rewritten in the same pass). The same logic applies in
    /// reverse for boxed `Boolean` fields whose hand-rolled getter uses
    /// `is-` prefix (Lombok generates `getXxx()` for boxed types).
    #[serde(default)]
    pub boolean_getter_strategy: Option<String>,
    /// `find_java_usages`: optional declaring class simple name. When set,
    /// filter results to call sites whose receiver expression plausibly
    /// resolves to a field of that type within the enclosing class (G9).
    #[serde(default)]
    pub declaring_class: Option<String>,
    /// `find_java_usages`: when true, return per-name counts + top-5
    /// example call sites instead of the full usage enumeration (G10).
    #[serde(default)]
    pub summary_only: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorApplyParams {
    /// RefactorPlan JSON returned by `bbox_refactor_plan`. Optional when
    /// `plan_path` is set; mutually exclusive with `plan_path`.
    #[serde(default)]
    pub plan: serde_json::Value,
    /// Optional filesystem path to a RefactorPlan JSON file (the same
    /// shape `bbox_refactor_plan` writes when `output_path` is set).
    /// Use this for large plans that exceed the MCP transport's inline
    /// parameter size limit. Mutually exclusive with `plan`.
    #[serde(default)]
    pub plan_path: Option<String>,
    /// Must be true. Prevents accidental writes from copied dry-run output.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Default false. When false, refuses to modify files that are dirty in git.
    #[serde(default)]
    pub allow_dirty_worktree: Option<bool>,
    /// Default false. When false, refuses paths outside registered projects.
    /// Use true only for disposable practice worktrees or isolated smoke tests.
    #[serde(default)]
    pub allow_unregistered_paths: Option<bool>,
    /// Caller's working directory. When set, apply checks that the plan
    /// was built against the same git worktree as `cwd`. Mismatch refuses
    /// with `cross_worktree_apply` unless `force_path=true`. Used by
    /// background-job and worktree-isolation flows where the operator
    /// switched worktrees between plan and apply. (G15)
    #[serde(default)]
    pub cwd: Option<String>,
    /// Default false. When true, bypasses the `cross_worktree_apply`
    /// refusal and writes to the plan's recorded paths verbatim. Use
    /// only when the operator explicitly wants the original (main
    /// checkout) paths despite the cwd mismatch. (G15)
    #[serde(default)]
    pub force_path: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanSummary {
    pub status: String,
    pub plan_path: String,
    pub title: String,
    pub kind: String,
    pub semantic_status: SemanticStatus,
    pub file_count: usize,
    pub total_edits: usize,
    pub file_moves: usize,
    pub files: Vec<RefactorPlanSummaryFile>,
    pub leftovers_count: usize,
    pub leftovers: Vec<String>,
    /// Propagated from the plan when `deep_analysis: true` was requested (RX-A1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_analysis: Option<DeepAnalysis>,
    /// Plan lifecycle state propagated from the plan (RX-A2).
    #[serde(default)]
    pub plan_status: PlanStatus,
    /// FIXME marker counts propagated from the plan (RX-A2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixme_count: Option<FixmeCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorPlanSummaryFile {
    pub path: String,
    pub edit_count: usize,
    pub original_sha256: String,
}

fn build_plan_summary(plan_json: &str, plan_path: &Path) -> Result<RefactorPlanSummary> {
    let plan: RefactorPlan = serde_json::from_str(plan_json)
        .context("plan JSON written to output_path could not be parsed")?;
    let total_edits: usize = plan.edits.iter().map(|f| f.edits.len()).sum();
    let files = plan
        .edits
        .iter()
        .map(|f| RefactorPlanSummaryFile {
            path: f.path.clone(),
            edit_count: f.edits.len(),
            original_sha256: f.original_sha256.clone(),
        })
        .collect::<Vec<_>>();
    let leftovers_count = plan.leftovers.len();
    Ok(RefactorPlanSummary {
        status: "ok".to_string(),
        plan_path: path_string(plan_path),
        title: plan.title,
        kind: plan.kind,
        semantic_status: plan.semantic_status,
        file_count: plan.edits.len(),
        total_edits,
        file_moves: plan.file_moves.len(),
        files,
        leftovers_count,
        leftovers: plan.leftovers,
        deep_analysis: plan.deep_analysis,
        plan_status: plan.plan_status,
        fixme_count: plan.fixme_count,
    })
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefactorRunParams {
    /// Human-readable compound run title.
    pub title: String,
    /// Project root used for relative paths.
    pub project_dir: String,
    /// Ordered primitive-plan steps.
    pub steps: Vec<RefactorRunStep>,
    /// Must be true to write files. Otherwise returns a plan-only report.
    #[serde(default)]
    pub confirm: Option<bool>,
    /// Default false. When false, refuses initially dirty files touched by primitive plans.
    #[serde(default)]
    pub allow_dirty_worktree: Option<bool>,
    /// Default false. When false, refuses paths outside registered projects.
    #[serde(default)]
    pub allow_unregistered_paths: Option<bool>,
}

/// Which stdout format to parse for a Command step (RX-F2a).
/// Additional variants (clippy JSON, miri) may be added in later phases.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSpec {
    /// Parse stdout as `cargo --message-format=json` output and extract
    /// `compiler-message` entries.  Malformed lines are silently dropped.
    RustcJson,
}

/// Failure-handling mode for a Command step (RX-F2b).
/// Supersedes the legacy `required: bool` field when set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    /// Exit-code != 0 rolls back all prior writes and terminates the run. Default.
    Required,
    /// Exit-code != 0 is logged as `failed_optional` and the run continues.
    Optional,
    /// Exit-code != 0 opens a repair obligation and continues. A later repair
    /// step must mark the obligation Consumed or LeftOver for terminal success.
    /// Any obligation that remains Open at run end triggers rollback from the
    /// first soft-fail cursor (RX-F2b).
    ContinueForRepair,
}

/// Summary of captured compiler diagnostics for a command step.
/// Full diagnostic bodies are NOT included in the MCP response (size budget);
/// only aggregate counts are surfaced here (RX-F2a).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CapturedDiagnosticsSummary {
    pub count: usize,
    /// Keyed by severity level, e.g. `{"error": 2, "warning": 3}`.
    pub severity_counts: HashMap<String, usize>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RefactorRunStep {
    Plan {
        #[serde(flatten)]
        params: RefactorPlanParams,
        /// When true, plan-time failure (e.g. "no boilerplate") is logged
        /// as a `skipped` step in the run report rather than aborting the
        /// batch and rolling back prior writes. Default false preserves
        /// strict batch semantics. Use this when batching lombokify or
        /// similar plans across many files where a subset will return
        /// "no boilerplate" — those files become per-step skips, not
        /// batch-failures (Gap 2 from JAVA_TOOL_GAPS).
        #[serde(default)]
        optional: bool,
    },
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// Optional extra files the command may mutate. Relative paths resolve against project_dir.
        #[serde(default)]
        touches: Vec<String>,
        /// Defaults true. Required command failure rolls back prior plan writes.
        #[serde(default)]
        required: Option<bool>,
        /// When set, the command's stdout is parsed in the specified format and
        /// stashed in the run-context capture store under ref `"last"` (RX-F2a).
        /// This field is independent of `required` / failure handling.
        #[serde(default)]
        capture: Option<CaptureSpec>,
        /// How to handle a non-zero exit code (RX-F2b). When set, supersedes
        /// the legacy `required` field. When unset, `required: false` → Optional,
        /// `required: true` (default) → Required.
        #[serde(default)]
        on_failure: Option<OnFailure>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct SyntaxItem {
    pub plan_local_id: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub leading_trivia_start: usize,
    pub trailing_trivia_end: usize,
    pub line_start: usize,
    pub line_end: usize,
    #[serde(default)]
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct TextEdit {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FileEdit {
    pub path: String,
    pub original_sha256: String,
    pub edits: Vec<TextEdit>,
    /// Pre-computed would-be file content (RX-A2). Populated for target files in
    /// `Blocked` plans so callers can grep for `FIXME(refactor-plan-only):` markers
    /// without re-applying the edits. Never written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct FileMove {
    pub source_path: String,
    pub target_path: String,
    pub original_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SemanticStatus {
    SyntaxOnly,
    #[serde(alias = "unverified")]
    IndexedHints,
    LspVerified,
}

/// Lifecycle state of a `RefactorPlan` (RX-A2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Plan is ready to apply (default).
    #[default]
    Planned,
    /// Plan has already been applied.
    Applied,
    /// Plan has deep-analysis findings that must be resolved before applying.
    /// Apply is refused with `plan_blocked` when `plan_status` is this variant.
    Blocked,
    /// Plan encountered an error during generation.
    Errored,
}

/// Count of FIXME markers emitted in the plan's target text (RX-A2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, schemars::JsonSchema)]
pub struct FixmeCount {
    /// `FIXME(refactor-plan-only):` markers — exist only in the saved plan JSON,
    /// never on disk. Apply is refused when this is non-zero.
    pub plan_only: usize,
    /// `FIXME(refactor-warning):` markers (RX-W1, not yet emitted in A2).
    pub warning: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ValidationStep {
    TreeSitterNoErrors {
        path: String,
        #[serde(default)]
        byte_range: Option<(usize, usize)>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct RefactorPlan {
    pub title: String,
    pub kind: String,
    pub semantic_status: SemanticStatus,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_moves: Vec<FileMove>,
    pub edits: Vec<FileEdit>,
    pub validations: Vec<ValidationStep>,
    pub items: Vec<SyntaxItem>,
    #[serde(default)]
    pub leftovers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captured_variables: Vec<CapturedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining_source_accessors: Vec<RemainingFieldAccessor>,
    /// Surviving source-side references to static-final constants moved by
    /// `extract_java_class` (under `deep_analysis: true`). Populated alongside
    /// the qualifier rewrites the planner emits — operator-facing preview of
    /// each constant's leftover ref sites. `kind` is always `"read"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining_source_constant_refs: Vec<RemainingFieldAccessor>,
    /// External method calls inside the extracted set that resolve to methods
    /// on the source class but are not in the extracted set. Reported by
    /// `extract_java_methods` and `extract_java_class`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_calls: Vec<ExternalCall>,
    /// Methods called inside the extracted set that are inherited from a
    /// superclass or implemented interface in the project type index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inherited_dependencies: Vec<InheritedDependency>,
    /// Populated when `deep_analysis: true` is passed to
    /// `extract_rust_impl_methods` (RX-A1). Empty / absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_analysis: Option<DeepAnalysis>,
    /// Plan lifecycle state (RX-A2). `Blocked` when deep-analysis found unresolved
    /// dependencies; apply is refused in that state.
    #[serde(default)]
    pub plan_status: PlanStatus,
    /// Counts of FIXME markers emitted in the target text (RX-A2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixme_count: Option<FixmeCount>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorStatus {
    pub status: String,
    pub path: String,
    pub language: String,
    pub sha256: String,
    pub parse: ParseReport,
    pub total_items: usize,
    pub matching_items: usize,
    pub returned_items: usize,
    pub truncated: bool,
    pub items: Vec<SyntaxItem>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorProjectRefs {
    pub status: String,
    pub path: String,
    pub project_dir: String,
    pub relative_path: String,
    pub project_id: String,
    pub rel_path_hash: String,
    pub total_chunks: usize,
    pub matching_chunks: usize,
    pub returned_chunks: usize,
    pub truncated: bool,
    pub chunks: Vec<ProjectFileChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectFileChunkRef {
    pub entity_ref: String,
    pub chunk_hash: String,
    pub occurrence_idx: u32,
    pub chunk_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Raw tree-sitter node kind for the chunk's symbol-producing
    /// node (CN-D1 / CN-D4). For Rust impl methods this is
    /// `"function_item"`; the synthetic refactor vocabulary
    /// (`"impl_method"`) is produced by `refactor::status` on a
    /// separate surface. `None` for non-code chunks and chunks
    /// without a tree-sitter kind. Field is optional in JSON for
    /// backward compatibility — callers parsing the pre-CN-D4 shape
    /// continue to work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    /// Kind of the nearest enclosing symbol-producing ancestor
    /// (CN-D1 / CN-D4). Useful for callers that want to disambiguate
    /// e.g. top-level functions from impl methods without consulting
    /// `bbox_refactor_status`. `None` at file top level or for non-code
    /// chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    /// 1-based start line in the source file (CN-D2 / CN-D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<u32>,
    /// 1-based end line in the source file (CN-D2 / CN-D4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<u32>,
    pub byte_start: u64,
    pub byte_end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ParseReport {
    pub has_error: bool,
    pub error_nodes: usize,
    pub missing_nodes: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorApplyResponse {
    pub status: String,
    pub files_written: Vec<String>,
    pub validations: Vec<ParseValidationResult>,
    pub rolled_back: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorRunResponse {
    pub status: String,
    pub title: String,
    pub dry_run: bool,
    pub steps: Vec<RefactorRunStepReport>,
    pub files_written: Vec<String>,
    pub rolled_back: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_errors: Vec<String>,
    /// Repair obligations opened during the run (RX-F2b). Non-empty when
    /// any `ContinueForRepair` command step fired.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<ObligationReport>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefactorRunStepReport {
    pub index: usize,
    pub op: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validations: Vec<ParseValidationResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set when the step was run with `capture=rustc_json` (RX-F2a).
    /// Full diagnostic bodies are omitted; only aggregate counts are returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_diagnostics_summary: Option<CapturedDiagnosticsSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParseValidationResult {
    pub path: String,
    pub has_error: bool,
    pub error_nodes: usize,
    pub missing_nodes: usize,
    /// First few error / missing node locations with a short surrounding
    /// excerpt, populated only when parsing produced errors. Capped to
    /// the first 5 nodes by tree-sitter walk order so the response stays
    /// debuggable without exploding when a rewrite ruptures the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_excerpts: Vec<ParseErrorExcerpt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ParseErrorExcerpt {
    /// "error" for a tree-sitter ERROR node, "missing" for a tree-sitter
    /// MISSING node.
    pub kind: String,
    /// 1-based line of the node's start byte in the rewritten source.
    pub line: usize,
    /// 1-based column of the node's start byte.
    pub column: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    /// ~3-line window of source around the start byte with leading line
    /// numbers, e.g. `"  41 | foo;\n  42 | bar baz\n  43 | }"`. Trailing
    /// whitespace is trimmed.
    pub snippet: String,
}

struct ParsedSource {
    path: PathBuf,
    language: &'static str,
    source: String,
    tree: Tree,
}

pub fn status(p: &RefactorStatusParams) -> Result<String> {
    let path = resolve_path(p.project_dir.as_deref(), &p.file)?;
    let parsed = parse_source_file(&path)?;
    let report = parse_report(parsed.tree.root_node());
    let mut items = match parsed.language {
        "rust" => rust_status_items(&parsed),
        "java" => java_status_items(&parsed),
        _ => generic_top_level_items(&parsed),
    };
    let total_items = items.len();
    if let Some(kinds) = p.item_kinds.as_deref().filter(|kinds| !kinds.is_empty()) {
        let kinds = kinds.iter().map(String::as_str).collect::<HashSet<_>>();
        items.retain(|item| kinds.contains(item.kind.as_str()));
    }
    if let Some(names) = p.item_names.as_deref().filter(|names| !names.is_empty()) {
        let names = names.iter().map(String::as_str).collect::<HashSet<_>>();
        items.retain(|item| {
            item.name
                .as_deref()
                .is_some_and(|name| names.contains(name))
        });
    }
    if p.include_attributes == Some(false) {
        for item in &mut items {
            item.attributes.clear();
        }
    }
    let matching_items = items.len();
    let limit = p.limit.unwrap_or(200).min(1000);
    let truncated = matching_items > limit;
    items.truncate(limit);
    let returned_items = items.len();
    let response = RefactorStatus {
        status: "ok".to_string(),
        path: path_string(&path),
        language: parsed.language.to_string(),
        sha256: sha256_hex(parsed.source.as_bytes()),
        parse: report,
        total_items,
        matching_items,
        returned_items,
        truncated,
        items,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

pub fn project_refs(p: &RefactorProjectRefsParams) -> Result<String> {
    let project_dir = resolve_project_dir(p.project_dir.as_deref())?;
    let project_dir_arg = project_dir.to_string_lossy().into_owned();
    let path = resolve_path(Some(&project_dir_arg), &p.file)?;
    let relative_path = path
        .strip_prefix(&project_dir)
        .with_context(|| {
            format!(
                "{} is not under project_dir {}",
                path.display(),
                project_dir.display()
            )
        })?
        .to_path_buf();
    let project_id = entity_ref::project_id_for_path(&project_dir)?;
    let rel_path_hash = short_hash(relative_path.to_string_lossy().as_bytes());
    let chunks = chunk_file_for_refs(&path, &relative_path, &project_id, &rel_path_hash)?;
    let total_chunks = chunks.len();
    let include_excerpt = p.include_excerpt.unwrap_or(true);
    let query = p.query.as_deref().map(str::to_lowercase);
    let mut refs = chunks
        .into_iter()
        .map(|chunk| {
            let entity_ref = format!(
                "project_file:{project_id}:{rel_path_hash}:{}:{}",
                chunk.chunk_hash, chunk.occurrence_idx
            );
            ProjectFileChunkRef {
                entity_ref,
                chunk_hash: chunk.chunk_hash,
                occurrence_idx: chunk.occurrence_idx,
                chunk_kind: chunk.chunk_kind,
                language: chunk.language,
                symbol: chunk.symbol,
                symbol_kind: chunk.symbol_kind,
                parent_kind: chunk.parent_kind,
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                byte_start: chunk.byte_start,
                byte_end: chunk.byte_end,
                excerpt: include_excerpt.then(|| excerpt(&chunk.content, 320)),
            }
        })
        .collect::<Vec<_>>();
    if let Some(query) = query.as_deref().filter(|query| !query.is_empty()) {
        refs.retain(|chunk| {
            chunk.entity_ref.to_lowercase().contains(query)
                || chunk.chunk_hash.to_lowercase().contains(query)
                || chunk.chunk_kind.to_lowercase().contains(query)
                || chunk
                    .symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.to_lowercase().contains(query))
                || chunk
                    .excerpt
                    .as_deref()
                    .is_some_and(|excerpt| excerpt.to_lowercase().contains(query))
        });
    }
    let matching_chunks = refs.len();
    let limit = p.limit.unwrap_or(100).min(1000);
    let truncated = matching_chunks > limit;
    refs.truncate(limit);
    let response = RefactorProjectRefs {
        status: "ok".to_string(),
        path: path_string(&path),
        project_dir: path_string(&project_dir),
        relative_path: relative_path.to_string_lossy().into_owned(),
        project_id,
        rel_path_hash,
        total_chunks,
        matching_chunks,
        returned_chunks: refs.len(),
        truncated,
        chunks: refs,
    };
    Ok(serde_json::to_string_pretty(&response)?)
}

#[allow(dead_code)] // test-only wrapper; production paths use plan_with_ctx
pub fn plan(p: &RefactorPlanParams) -> Result<String> {
    plan_with_ctx(p, &PlanContext::default())
}

pub fn plan_with_ctx(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    let plan_json = plan_dispatch(p, ctx)?;
    // `find_java_usages` owns custom output-path behavior (compact digest + full
    // report write). Let that plan manage persistence itself to avoid generic
    // post-processing overwriting the expected on-disk payload.
    if p.kind != "find_java_usages" && p.output_path.as_deref().is_some() {
        let output_path = p.output_path.as_deref().unwrap();
        let resolved = plan_slot::resolve_plan_write_path(output_path)?;
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent directory for plan output: {}", parent.display())
                })?;
            }
        }
        fs::write(&resolved, &plan_json)
            .with_context(|| format!("writing plan JSON to {}", resolved.display()))?;
        let summary = build_plan_summary(&plan_json, &resolved)?;
        return Ok(serde_json::to_string_pretty(&summary)?);
    }
    Ok(plan_json)
}

fn plan_dispatch(p: &RefactorPlanParams, ctx: &PlanContext) -> Result<String> {
    match p.kind.as_str() {
        "extract_rust_items" => plan_extract_rust_items(p),
        "extract_rust_impl_methods" => plan_extract_rust_impl_methods(p),
        "lift_rust_inherent_to_free" => rust_lift_free::plan_lift_to_free(p),
        "delete_rust_items" => plan_delete_rust_items(p),
        "add_rust_router_to_sum" => plan_add_rust_router_to_sum(p),
        "add_rust_mod_decl" => plan_add_rust_mod_decl(p),
        "add_rust_use_decl" => plan_add_rust_use_decl(p),
        "copy_rust_mod_decls" => plan_copy_rust_mod_decls(p),
        "rewrite_rust_mod_visibility" => plan_rewrite_rust_mod_visibility(p),
        "rewrite_rust_item_visibility" => plan_rewrite_rust_item_visibility(p),
        "rewrite_rust_field_visibility" => plan_rewrite_rust_field_visibility(p),
        "rust_lsp_rename" => plan_rust_lsp_rename(p, ctx),
        "rust_organize_imports" => plan_rust_organize_imports(p, ctx),
        "inline_mod_to_file_submodule" => {
            rust_inline_mod::plan_inline_mod_to_file_submodule(p)
        }
        "extract_rust_items_to_submodule" => {
            rust_extract_to_submodule::plan_extract_rust_items_to_submodule(p)
        }
        "move_rust_items_with_callers" => {
            rust_move_with_callers::plan_move_rust_items_with_callers(p)
        }
        "extract_java_methods" => plan_extract_java_methods(p),
        "extract_java_class" => plan_extract_java_class(p),
        "extract_java_nested_classes" => plan_extract_java_nested_classes(p),
        "promote_java_inner_class" => plan_promote_java_inner_class(p),
        "add_java_fields" => plan_add_java_fields(p),
        "add_java_constructor" => plan_add_java_constructor(p),
        "move_java_field" => plan_move_java_field(p),
        "move_java_constant" => plan_move_java_constant(p),
        "update_java_callers" => plan_update_java_callers(p),
        "add_java_delegate_field" => plan_add_java_delegate_field(p),
        "rewrite_java_visibility" => plan_rewrite_java_visibility(p),
        "java_lsp_organize_imports" => plan_java_lsp_organize_imports(p, ctx),
        "add_java_implements" => plan_add_java_implements(p),
        "extract_java_interface" => plan_extract_java_interface(p),
        "migrate_java_type_usages" => plan_migrate_java_type_usages(p),
        "find_java_usages" => plan_find_java_usages(p),
        "rename_java_symbol" => plan_rename_java_symbol(p),
        "java_class_dependency_analysis" => plan_java_class_dependency_analysis(p),
        "java_public_api_guard" => plan_java_public_api_guard(p),
        "lombokify_java_class" => plan_lombokify_java_class(p),
        "rewrite_rust_error_type" => rust_error_migrate::plan_rewrite_error_type(p),
        "migrate_rust_type_usages" => rust_migrate_types::plan_migrate_type_usages(p),
        "extract_rust_trait" => rust_extract_trait::plan_extract_trait(p),
        "rust_match_arm_to_strategy" => rust_match_strategy::plan_match_to_strategy(p),
        "move_rust_struct_fields" => rust_move_fields::plan_move_struct_fields(p),
        "add_rust_delegate_field" => rust_delegate_field::plan_add_delegate(p),
        "update_rust_callers" => rust_update_callers::plan_update_callers(p),
        "rust_ra_move_item_to_module" => rust_ra_move_item::plan_ra_move(p),
        "rust_ra_classify_callbacks" => rust_ra_classify_callbacks::plan_ra_classify(p, ctx),
        "rust_impl_partition_analysis" => plan_rust_impl_partition_analysis(p),
        "rust_public_api_guard" => plan_rust_public_api_guard(p),
        "move_file" => plan_move_file(p),
        "replace_text" => plan_replace_text(p),
        "write_file" => plan_write_file(p),
        "ensure_toml_table" => plan_ensure_toml_table(p),
        other => bail!(
            "unsupported refactor plan kind `{other}`; supported: extract_rust_items, extract_rust_impl_methods, lift_rust_inherent_to_free, delete_rust_items, add_rust_router_to_sum, add_rust_mod_decl, add_rust_use_decl, copy_rust_mod_decls, rewrite_rust_mod_visibility, rewrite_rust_item_visibility, rewrite_rust_field_visibility, rust_lsp_rename, rust_organize_imports, inline_mod_to_file_submodule, extract_rust_items_to_submodule, move_rust_items_with_callers, extract_java_methods, extract_java_class, extract_java_nested_classes, add_java_fields, add_java_constructor, move_java_field, move_java_constant, update_java_callers, add_java_delegate_field, rewrite_java_visibility, java_lsp_organize_imports, add_java_implements, extract_java_interface, migrate_java_type_usages, find_java_usages, rename_java_symbol, java_class_dependency_analysis, java_public_api_guard, lombokify_java_class, rewrite_rust_error_type, migrate_rust_type_usages, extract_rust_trait, rust_match_arm_to_strategy, move_rust_struct_fields, add_rust_delegate_field, update_rust_callers, rust_ra_move_item_to_module, rust_ra_classify_callbacks, rust_impl_partition_analysis, rust_public_api_guard, rust_compile_fix_round, move_file, replace_text, write_file, ensure_toml_table"
        ),
    }
}

/// RX-G1 wrapper: surface `rust_impl_partition_analysis` through the plan dispatcher.
///
/// Analysis-only — no FileEdits. The impl-graph struct is flattened into the
/// response under `partition_graph`.
fn plan_rust_impl_partition_analysis(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let impl_name = p
        .impl_name
        .as_deref()
        .or(p.module_name.as_deref())
        .ok_or_else(|| anyhow!("impl_name or module_name required for rust_impl_partition_analysis"))?;
    let graph = rust_partition::analyze_impl(&source_path, impl_name)?;
    let plan = RefactorPlan {
        title: format!("rust_impl_partition_analysis on `{impl_name}`"),
        kind: "rust_impl_partition_analysis".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        edits: Vec::new(),
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };
    let body = serde_json::json!({
        "title": plan.title,
        "kind": plan.kind,
        "semantic_status": plan.semantic_status,
        "dry_run": plan.dry_run,
        "file_moves": plan.file_moves,
        "edits": plan.edits,
        "validations": plan.validations,
        "items": plan.items,
        "leftovers": plan.leftovers,
        "plan_status": plan.plan_status,
        "partition_graph": graph,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

/// RX-G2 wrapper: surface `rust_public_api_guard` through the plan dispatcher.
///
/// Advisory only — no FileEdits. Severity + touched-item delta are flattened
/// into the response under `public_api_report`.
fn plan_rust_public_api_guard(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let proposed_changes: Vec<rust_public_api::ProposedChangeRef> = p
        .toml_entries
        .as_ref()
        .and_then(|e| e.get("proposed_changes"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let report = rust_public_api::analyze_public_api(&source_path, &proposed_changes)?;
    let plan = RefactorPlan {
        title: "rust_public_api_guard advisory".to_string(),
        kind: "rust_public_api_guard".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        edits: Vec::new(),
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };
    let body = serde_json::json!({
        "title": plan.title,
        "kind": plan.kind,
        "semantic_status": plan.semantic_status,
        "dry_run": plan.dry_run,
        "file_moves": plan.file_moves,
        "edits": plan.edits,
        "validations": plan.validations,
        "items": plan.items,
        "leftovers": plan.leftovers,
        "plan_status": plan.plan_status,
        "public_api_report": report,
    });
    Ok(serde_json::to_string_pretty(&body)?)
}

pub fn apply(p: &RefactorApplyParams, projects: &[ProjectRecord]) -> Result<String> {
    if p.confirm != Some(true) {
        bail!("confirm=true is required to apply a refactor plan");
    }
    let plan: RefactorPlan = match (&p.plan, p.plan_path.as_deref()) {
        (_, Some(path)) => {
            let resolved = plan_slot::resolve_plan_read_path(path)?;
            let body = fs::read_to_string(&resolved)
                .with_context(|| format!("reading plan_path {}", resolved.display()))?;
            serde_json::from_str(&body).with_context(|| {
                format!(
                    "plan_path {} must contain a RefactorPlan JSON object",
                    resolved.display()
                )
            })?
        }
        (value, None) if !value.is_null() => serde_json::from_value(value.clone())
            .context("plan must be a RefactorPlan JSON object returned by bbox_refactor_plan")?,
        (_, None) => {
            bail!("either `plan` or `plan_path` must be provided")
        }
    };
    validate_plan_shape(&plan)?;

    // G15: refuse to apply across git-worktree boundaries unless the
    // caller explicitly opted in. The plan stores absolute paths at
    // plan-generation time; if the caller switched worktrees after
    // planning, applying writes to the original (main-checkout) paths
    // and silently contaminates other sessions. When the caller passes
    // `cwd` we compare git toplevels and refuse mismatches.
    if p.force_path != Some(true) {
        if let Some(cwd_str) = p.cwd.as_deref() {
            let cwd_path = PathBuf::from(cwd_str);
            let plan_anchor: Option<PathBuf> = plan
                .edits
                .first()
                .map(|e| PathBuf::from(&e.path))
                .or_else(|| {
                    plan.file_moves
                        .first()
                        .map(|m| PathBuf::from(&m.source_path))
                });
            if let Some(plan_anchor) = plan_anchor {
                let cwd_top = crate::git::git_root_for_path(&cwd_path);
                let plan_top = crate::git::git_root_for_path(&plan_anchor);
                if let (Some(c), Some(pp)) = (cwd_top.as_ref(), plan_top.as_ref()) {
                    if c != pp {
                        bail!(
                            "cross_worktree_apply: plan was generated against `{}` but apply is running from `{}`. \
                             Re-plan from the current worktree, or pass `force_path=true` to write to the plan's recorded paths.",
                            pp.display(),
                            c.display()
                        );
                    }
                }
            }
        }
    }

    // RX-A2: refuse blocked plans before touching any files.
    if plan.plan_status == PlanStatus::Blocked {
        bail!(
            "plan_blocked: this plan has unresolved deep-analysis findings \
             (fixme_count.plan_only = {}). \
             Review the FIXME(refactor-plan-only) markers in the plan's target text, \
             resolve the listed dependencies, then re-plan without deep_analysis findings.",
            plan.fixme_count.as_ref().map_or(0, |c| c.plan_only)
        );
    }

    let mut originals: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut moved_files = Vec::new();
    for file_move in &plan.file_moves {
        let source_path = PathBuf::from(&file_move.source_path);
        let target_path = PathBuf::from(&file_move.target_path);
        if p.allow_unregistered_paths != Some(true) {
            ensure_path_in_registered_project(&source_path, projects)?;
            ensure_path_in_registered_project(&target_path, projects)?;
        }
        if p.allow_dirty_worktree != Some(true) {
            ensure_git_clean_for_path(&source_path)?;
            ensure_git_clean_for_path(&target_path)?;
        }
        if target_path.exists() {
            bail!(
                "refusing to move {} to {}: target already exists",
                source_path.display(),
                target_path.display()
            );
        }
        let original = read_original_for_edit(&source_path, &file_move.original_sha256)?;
        let original_hash = sha256_hex(&original);
        if original_hash != file_move.original_sha256 {
            bail!(
                "refusing to move {}: file hash changed (expected {}, got {})",
                source_path.display(),
                file_move.original_sha256,
                original_hash
            );
        }
        originals.push((source_path.clone(), Some(original.clone())));
        originals.push((target_path.clone(), None));
        moved_files.push((source_path, target_path, original));
    }

    let mut rewritten = Vec::new();
    for edit in &plan.edits {
        let path = PathBuf::from(&edit.path);
        if p.allow_unregistered_paths != Some(true) {
            ensure_path_in_registered_project(&path, projects)?;
        }
        if p.allow_dirty_worktree != Some(true) {
            ensure_git_clean_for_path(&path)?;
        }
        let original = read_original_for_edit(&path, &edit.original_sha256)?;
        let original_hash = sha256_hex(&original);
        if original_hash != edit.original_sha256 {
            bail!(
                "refusing to apply {}: file hash changed (expected {}, got {})",
                path.display(),
                edit.original_sha256,
                original_hash
            );
        }
        let original_text = String::from_utf8(original.clone())
            .with_context(|| format!("{} is not valid utf-8", path.display()))?;
        let next = apply_text_edits(&original_text, &edit.edits)
            .with_context(|| format!("failed to apply edits for {}", path.display()))?;
        originals.push((path.clone(), Some(original)));
        rewritten.push((path, next.into_bytes()));
    }

    let mut validation_inputs = rewritten.clone();
    validation_inputs.extend(
        moved_files
            .iter()
            .map(|(_, target_path, bytes)| (target_path.clone(), bytes.clone())),
    );
    let validations = validate_rewritten_files(&validation_inputs)?;
    if validations
        .iter()
        .any(|v| v.has_error || v.error_nodes > 0 || v.missing_nodes > 0)
    {
        return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
            status: "validation_failed".to_string(),
            files_written: Vec::new(),
            validations,
            rolled_back: false,
            error: None,
            rollback_errors: Vec::new(),
        })?);
    }

    let mut files_written = Vec::new();
    for (source_path, target_path, bytes) in &moved_files {
        if let Some(parent) = target_path.parent() {
            if let Err(err) = fs::create_dir_all(parent) {
                let rollback_errors = restore_snapshots(&originals);
                return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                    status: "write_failed".to_string(),
                    files_written,
                    validations,
                    rolled_back: rollback_errors.is_empty(),
                    error: Some(format!(
                        "failed to create parent directory {}: {err:#}",
                        parent.display()
                    )),
                    rollback_errors,
                })?);
            }
        }
        if let Err(err) = write_atomic(target_path, bytes) {
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "write_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!(
                    "failed to write {}: {err:#}",
                    target_path.display()
                )),
                rollback_errors,
            })?);
        }
        files_written.push(path_string(target_path));
        if let Err(err) = fs::remove_file(source_path) {
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "remove_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!(
                    "failed to remove {}: {err:#}",
                    source_path.display()
                )),
                rollback_errors,
            })?);
        }
    }

    for (path, bytes) in &rewritten {
        if p.allow_dirty_worktree != Some(true) {
            if let Err(err) = ensure_git_clean_for_path(path) {
                let rollback_errors = restore_snapshots(&originals);
                return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                    status: "dirty_worktree".to_string(),
                    files_written,
                    validations,
                    rolled_back: rollback_errors.is_empty(),
                    error: Some(err.to_string()),
                    rollback_errors,
                })?);
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Err(err) = write_atomic(path, bytes) {
            let rollback_errors = restore_snapshots(&originals);
            return Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
                status: "write_failed".to_string(),
                files_written,
                validations,
                rolled_back: rollback_errors.is_empty(),
                error: Some(format!("failed to write {}: {err:#}", path.display())),
                rollback_errors,
            })?);
        }
        files_written.push(path_string(path));
    }

    Ok(serde_json::to_string_pretty(&RefactorApplyResponse {
        status: "ok".to_string(),
        files_written,
        validations,
        rolled_back: false,
        error: None,
        rollback_errors: Vec::new(),
    })?)
}

#[allow(dead_code)] // test-only wrapper; production paths use run_with_ctx
pub fn run(p: &RefactorRunParams, projects: &[ProjectRecord]) -> Result<String> {
    run_with_ctx(p, projects, &PlanContext::default())
}

pub fn run_with_ctx(
    p: &RefactorRunParams,
    projects: &[ProjectRecord],
    ctx: &PlanContext,
) -> Result<String> {
    let project_dir = resolve_path(None, &p.project_dir)?;
    if !project_dir.is_dir() {
        bail!(
            "project_dir must be an existing directory: {}",
            project_dir.display()
        );
    }
    let confirmed = p.confirm == Some(true);
    let mut snapshots: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let mut snapshot_paths: HashSet<PathBuf> = HashSet::new();
    let mut reports = Vec::new();
    let mut files_written = Vec::new();
    let mut capture_ctx = RunCaptureContext::default();

    for (idx, step) in p.steps.iter().enumerate() {
        match step {
            RefactorRunStep::Plan { params, optional } => {
                let mut step_params = params.clone();
                if step_params.project_dir.is_none() {
                    step_params.project_dir = Some(path_string(&project_dir));
                }

                // RX-C1: rust_compile_fix_round — read diagnostics from capture context,
                // classify them, mark the obligation consumed/leftover.
                if step_params.kind == "rust_compile_fix_round" {
                    let ref_name = step_params
                        .toml_entries
                        .as_ref()
                        .and_then(|e| e.get("diagnostics_ref"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_else(|| {
                            if step_params.source.is_empty() {
                                "last"
                            } else {
                                step_params.source.as_str()
                            }
                        })
                        .to_string();
                    let diagnostics: Vec<RustcDiagnostic> = capture_ctx
                        .get(&ref_name)
                        .unwrap_or(&[])
                        .to_vec();
                    let plan_json = match rust_compile_fix::plan_compile_fix(&step_params, &diagnostics) {
                        Ok(j) => j,
                        Err(err) if *optional => {
                            reports.push(RefactorRunStepReport {
                                index: idx,
                                op: "plan".to_string(),
                                status: "skipped".to_string(),
                                kind: Some(step_params.kind.clone()),
                                title: None,
                                files: Vec::new(),
                                validations: Vec::new(),
                                error: Some(err.to_string()),
                                captured_diagnostics_summary: None,
                            });
                            continue;
                        }
                        Err(err) => {
                            let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                            let rollback_errors = restore_snapshots_from(&snapshots, cursor);
                            return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                status: "step_failed".to_string(),
                                title: p.title.clone(),
                                dry_run: !confirmed,
                                steps: append_report(
                                    reports,
                                    RefactorRunStepReport {
                                        index: idx,
                                        op: "plan".to_string(),
                                        status: "plan_failed".to_string(),
                                        kind: Some(step_params.kind.clone()),
                                        title: None,
                                        files: Vec::new(),
                                        validations: Vec::new(),
                                        error: Some(err.to_string()),
                                        captured_diagnostics_summary: None,
                                    },
                                ),
                                files_written,
                                rolled_back: rollback_errors.is_empty(),
                                error: Some(err.to_string()),
                                rollback_errors,
                                obligations: capture_ctx.obligation_reports(),
                            })?);
                        }
                    };
                    // Mark the obligation consumed or leftover based on plan's leftover count.
                    let leftover_count = serde_json::from_str::<RefactorPlan>(&plan_json)
                        .map(|plan| plan.leftovers.len())
                        .unwrap_or(0);
                    capture_ctx.mark_obligation_consumed(&ref_name, leftover_count);
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "plan".to_string(),
                        status: "ok".to_string(),
                        kind: Some(step_params.kind.clone()),
                        title: Some("rust_compile_fix_round: diagnostics classified".to_string()),
                        files: Vec::new(),
                        validations: Vec::new(),
                        error: None,
                        captured_diagnostics_summary: None,
                    });
                    continue;
                }

                // Test-only stub plan kinds for obligation consumption (RX-F2b).
                // These are handled before calling plan_with_ctx so they never
                // reach production plan dispatch. Compiled in only under #[cfg(test)].
                #[cfg(test)]
                {
                    let ref_name = if step_params.source.is_empty() {
                        "last".to_string()
                    } else {
                        step_params.source.clone()
                    };
                    if step_params.kind == "test_consume_obligation" {
                        capture_ctx.mark_obligation_consumed(&ref_name, 0);
                        reports.push(RefactorRunStepReport {
                            index: idx,
                            op: "plan".to_string(),
                            status: "ok".to_string(),
                            kind: Some(step_params.kind.clone()),
                            title: Some("stub: consume obligation".to_string()),
                            files: Vec::new(),
                            validations: Vec::new(),
                            error: None,
                            captured_diagnostics_summary: None,
                        });
                        continue;
                    }
                    if step_params.kind == "test_leftover_obligation" {
                        capture_ctx.mark_obligation_consumed(&ref_name, 1);
                        reports.push(RefactorRunStepReport {
                            index: idx,
                            op: "plan".to_string(),
                            status: "ok".to_string(),
                            kind: Some(step_params.kind.clone()),
                            title: Some("stub: leftover obligation".to_string()),
                            files: Vec::new(),
                            validations: Vec::new(),
                            error: None,
                            captured_diagnostics_summary: None,
                        });
                        continue;
                    }
                }

                let plan_text = match plan_with_ctx(&step_params, ctx) {
                    Ok(plan_text) => plan_text,
                    Err(err) if *optional => {
                        // Optional step — log as skipped, keep prior writes,
                        // continue to the next step. This is the bulk-batch
                        // path: many files where a subset has nothing to
                        // lombokify shouldn't kill the whole batch.
                        reports.push(RefactorRunStepReport {
                            index: idx,
                            op: "plan".to_string(),
                            status: "skipped".to_string(),
                            kind: Some(step_params.kind),
                            title: None,
                            files: Vec::new(),
                            validations: Vec::new(),
                            error: Some(err.to_string()),
                            captured_diagnostics_summary: None,
                        });
                        continue;
                    }
                    Err(err) => {
                        let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                        let rollback_errors = restore_snapshots_from(&snapshots, cursor);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: !confirmed,
                            steps: append_report(
                                reports,
                                RefactorRunStepReport {
                                    index: idx,
                                    op: "plan".to_string(),
                                    status: "plan_failed".to_string(),
                                    kind: Some(step_params.kind),
                                    title: None,
                                    files: Vec::new(),
                                    validations: Vec::new(),
                                    error: Some(err.to_string()),
                                    captured_diagnostics_summary: None,
                                },
                            ),
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: Some(err.to_string()),
                            rollback_errors,
                            obligations: capture_ctx.obligation_reports(),
                        })?);
                    }
                };
                let plan_value: serde_json::Value = serde_json::from_str(&plan_text)?;
                let refactor_plan: RefactorPlan = serde_json::from_value(plan_value.clone())?;
                validate_plan_shape(&refactor_plan)?;
                let mut step_files = refactor_plan
                    .file_moves
                    .iter()
                    .flat_map(|file_move| {
                        [file_move.source_path.clone(), file_move.target_path.clone()]
                    })
                    .collect::<Vec<_>>();
                step_files.extend(refactor_plan.edits.iter().map(|edit| edit.path.clone()));

                if confirmed {
                    for file in &step_files {
                        let path = PathBuf::from(file);
                        if p.allow_unregistered_paths != Some(true) {
                            if let Err(err) = ensure_path_in_registered_project(&path, projects) {
                                let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                                let rollback_errors =
                                    restore_snapshots_from(&snapshots, cursor);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(err.to_string()),
                                    rollback_errors,
                                    obligations: capture_ctx.obligation_reports(),
                                })?);
                            }
                        }
                        if !snapshot_paths.contains(&path) {
                            if p.allow_dirty_worktree != Some(true) {
                                if let Err(err) = ensure_git_clean_for_path(&path) {
                                    let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                                    let rollback_errors =
                                        restore_snapshots_from(&snapshots, cursor);
                                    return Ok(serde_json::to_string_pretty(
                                        &RefactorRunResponse {
                                            status: "step_failed".to_string(),
                                            title: p.title.clone(),
                                            dry_run: false,
                                            steps: reports,
                                            files_written,
                                            rolled_back: rollback_errors.is_empty(),
                                            error: Some(err.to_string()),
                                            rollback_errors,
                                            obligations: capture_ctx.obligation_reports(),
                                        },
                                    )?);
                                }
                            }
                            let original = match fs::read(&path) {
                                Ok(bytes) => Some(bytes),
                                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                                Err(err) => {
                                    let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                                    let rollback_errors =
                                        restore_snapshots_from(&snapshots, cursor);
                                    return Ok(serde_json::to_string_pretty(
                                        &RefactorRunResponse {
                                            status: "step_failed".to_string(),
                                            title: p.title.clone(),
                                            dry_run: false,
                                            steps: reports,
                                            files_written,
                                            rolled_back: rollback_errors.is_empty(),
                                            error: Some(format!(
                                                "failed to read {}: {err}",
                                                path.display()
                                            )),
                                            rollback_errors,
                                            obligations: capture_ctx.obligation_reports(),
                                        },
                                    )?);
                                }
                            };
                            snapshot_paths.insert(path.clone());
                            snapshots.push((path, original));
                        }
                    }

                    let apply_text = match apply(
                        &RefactorApplyParams {
                            plan: plan_value,
                            plan_path: None,
                            confirm: Some(true),
                            allow_dirty_worktree: Some(true),
                            allow_unregistered_paths: p.allow_unregistered_paths,
                            cwd: None,
                            force_path: None,
                        },
                        projects,
                    ) {
                        Ok(apply_text) => apply_text,
                        Err(err) => {
                            let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                            let rollback_errors = restore_snapshots_from(&snapshots, cursor);
                            return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                status: "step_failed".to_string(),
                                title: p.title.clone(),
                                dry_run: false,
                                steps: reports,
                                files_written,
                                rolled_back: rollback_errors.is_empty(),
                                error: Some(err.to_string()),
                                rollback_errors,
                                obligations: capture_ctx.obligation_reports(),
                            })?);
                        }
                    };
                    let apply_response: RefactorApplyResponse =
                        match serde_json::from_str(&apply_text) {
                            Ok(response) => response,
                            Err(err) => {
                                let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                                let rollback_errors =
                                    restore_snapshots_from(&snapshots, cursor);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(err.to_string()),
                                    rollback_errors,
                                    obligations: capture_ctx.obligation_reports(),
                                })?);
                            }
                        };
                    let status = apply_response.status.clone();
                    let validations = apply_response.validations.clone();
                    let step_written = apply_response.files_written.clone();
                    files_written.extend(step_written.iter().cloned());
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "plan".to_string(),
                        status: status.clone(),
                        kind: Some(refactor_plan.kind),
                        title: Some(refactor_plan.title),
                        files: step_written,
                        validations,
                        error: apply_response.error.clone(),
                        captured_diagnostics_summary: None,
                    });
                    if status != "ok" {
                        let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                        let rollback_errors = restore_snapshots_from(&snapshots, cursor);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: false,
                            steps: reports,
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: apply_response.error,
                            rollback_errors,
                            obligations: capture_ctx.obligation_reports(),
                        })?);
                    }
                } else {
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "plan".to_string(),
                        status: "planned".to_string(),
                        kind: Some(refactor_plan.kind),
                        title: Some(refactor_plan.title),
                        files: step_files,
                        validations: Vec::new(),
                        error: None,
                        captured_diagnostics_summary: None,
                    });
                }
            }
            RefactorRunStep::Command {
                command,
                args,
                cwd,
                touches,
                required,
                capture,
                on_failure,
            } => {
                if !confirmed {
                    reports.push(RefactorRunStepReport {
                        index: idx,
                        op: "command".to_string(),
                        status: "planned".to_string(),
                        kind: None,
                        title: Some(command_display(command, args)),
                        files: touches.clone(),
                        validations: Vec::new(),
                        error: None,
                        captured_diagnostics_summary: None,
                    });
                    continue;
                }

                // Capture snapshot count before touches so the soft-fail cursor
                // points to the START of this step (includes the step's own touches).
                let step_snapshot_start = snapshots.len();
                let touched_paths = touches
                    .iter()
                    .map(|path| resolve_path(Some(&path_string(&project_dir)), path))
                    .collect::<Result<Vec<_>>>()?;
                for path in &touched_paths {
                    if p.allow_unregistered_paths != Some(true) {
                        ensure_path_in_registered_project(path, projects)?;
                    }
                    if !snapshot_paths.contains(path) {
                        if p.allow_dirty_worktree != Some(true) {
                            ensure_git_clean_for_path(path)?;
                        }
                        let original = match fs::read(path) {
                            Ok(bytes) => Some(bytes),
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                            Err(err) => {
                                let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                                let rollback_errors =
                                    restore_snapshots_from(&snapshots, cursor);
                                return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                    status: "step_failed".to_string(),
                                    title: p.title.clone(),
                                    dry_run: false,
                                    steps: reports,
                                    files_written,
                                    rolled_back: rollback_errors.is_empty(),
                                    error: Some(format!(
                                        "failed to read command touch {}: {err}",
                                        path.display()
                                    )),
                                    rollback_errors,
                                    obligations: capture_ctx.obligation_reports(),
                                })?);
                            }
                        };
                        snapshot_paths.insert(path.clone());
                        snapshots.push((path.clone(), original));
                    }
                }
                let command_result = match run_validation_command(&project_dir, command, args, cwd)
                {
                    Ok(result) => result,
                    Err(err) => {
                        let title = command_display(command, args);
                        reports.push(RefactorRunStepReport {
                            index: idx,
                            op: "command".to_string(),
                            status: "failed".to_string(),
                            kind: None,
                            title: Some(title.clone()),
                            files: touched_paths.iter().map(|path| path_string(path)).collect(),
                            validations: Vec::new(),
                            error: Some(format!("{err:#}")),
                            captured_diagnostics_summary: None,
                        });
                        let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                        let rollback_errors = restore_snapshots_from(&snapshots, cursor);
                        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                            status: "step_failed".to_string(),
                            title: p.title.clone(),
                            dry_run: false,
                            steps: reports,
                            files_written,
                            rolled_back: rollback_errors.is_empty(),
                            error: Some(format!("command failed: {title}")),
                            rollback_errors,
                            obligations: capture_ctx.obligation_reports(),
                        })?);
                    }
                };
                // Resolve effective failure mode: on_failure supersedes required (RX-F2b).
                let effective_on_failure = match on_failure.as_ref() {
                    Some(of) => of.clone(),
                    None => {
                        if required.unwrap_or(true) {
                            OnFailure::Required
                        } else {
                            OnFailure::Optional
                        }
                    }
                };
                let captured_diag_summary = if let Some(CaptureSpec::RustcJson) = capture {
                    let parsed = parse_rustc_json_output(&command_result.stdout);
                    let summary = CapturedDiagnosticsSummary {
                        count: parsed.len(),
                        severity_counts: parsed
                            .iter()
                            .fold(HashMap::new(), |mut acc, d| {
                                *acc.entry(d.level.clone()).or_insert(0) += 1;
                                acc
                            }),
                    };
                    capture_ctx.stash("last".to_string(), parsed);
                    Some(summary)
                } else {
                    None
                };
                files_written.extend(touched_paths.iter().map(|path| path_string(path)));
                let step_status = if command_result.success {
                    "ok".to_string()
                } else {
                    match effective_on_failure {
                        OnFailure::Required => "failed".to_string(),
                        OnFailure::Optional => "failed_optional".to_string(),
                        OnFailure::ContinueForRepair => "soft_failed".to_string(),
                    }
                };
                reports.push(RefactorRunStepReport {
                    index: idx,
                    op: "command".to_string(),
                    status: step_status.clone(),
                    kind: None,
                    title: Some(command_display(command, args)),
                    files: touched_paths.iter().map(|path| path_string(path)).collect(),
                    validations: Vec::new(),
                    error: command_result.error,
                    captured_diagnostics_summary: captured_diag_summary,
                });
                if !command_result.success {
                    match effective_on_failure {
                        OnFailure::Required => {
                            let cursor = capture_ctx.first_soft_fail_snapshot_idx();
                            let rollback_errors =
                                restore_snapshots_from(&snapshots, cursor);
                            return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
                                status: "step_failed".to_string(),
                                title: p.title.clone(),
                                dry_run: false,
                                steps: reports,
                                files_written,
                                rolled_back: rollback_errors.is_empty(),
                                error: Some(format!(
                                    "command failed: {}",
                                    command_display(command, args)
                                )),
                                rollback_errors,
                                obligations: capture_ctx.obligation_reports(),
                            })?);
                        }
                        OnFailure::Optional => {
                            // Continue; step already logged as failed_optional.
                        }
                        OnFailure::ContinueForRepair => {
                            // Open a repair obligation and continue (RX-F2b).
                            // The cursor points to the START of this step so that
                            // the step's own touches are included in rollback.
                            capture_ctx.open_obligation(
                                "last".to_string(),
                                idx,
                                step_snapshot_start,
                            );
                        }
                    }
                }
            }
        }
    }

    // Terminal-success policy (RX-F2b): commit only if every obligation is
    // Consumed or LeftOver. Any Open obligation triggers rollback from the
    // first soft-fail cursor. Consumed/LeftOver obligations are STILL live
    // rollback anchors until this terminal-success gate is passed.
    if confirmed && capture_ctx.has_any_obligation() && !capture_ctx.all_obligations_resolved() {
        let cursor = capture_ctx.first_soft_fail_snapshot_idx();
        let rollback_errors = restore_snapshots_from(&snapshots, cursor);
        return Ok(serde_json::to_string_pretty(&RefactorRunResponse {
            status: "obligations_unresolved".to_string(),
            title: p.title.clone(),
            dry_run: false,
            steps: reports,
            files_written,
            rolled_back: rollback_errors.is_empty(),
            error: Some("run ended with unresolved repair obligations".to_string()),
            rollback_errors,
            obligations: capture_ctx.obligation_reports(),
        })?);
    }

    Ok(serde_json::to_string_pretty(&RefactorRunResponse {
        status: if confirmed { "ok" } else { "planned" }.to_string(),
        title: p.title.clone(),
        dry_run: !confirmed,
        steps: reports,
        files_written,
        rolled_back: false,
        error: None,
        rollback_errors: Vec::new(),
        obligations: capture_ctx.obligation_reports(),
    })?)
}

fn append_report(
    mut reports: Vec<RefactorRunStepReport>,
    report: RefactorRunStepReport,
) -> Vec<RefactorRunStepReport> {
    reports.push(report);
    reports
}

struct CommandStepResult {
    success: bool,
    error: Option<String>,
    /// Raw stdout bytes from the command, always collected (RX-F2a).
    stdout: Vec<u8>,
}

/// Lifecycle state of a repair obligation (RX-F2b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObligationStatus {
    /// Opened by a soft-fail step; not yet resolved.
    Open,
    /// All diagnostics were consumed by a repair step (acceptable for commit).
    Consumed,
    /// Some diagnostics remain but were explicitly acknowledged as leftovers
    /// (still acceptable for commit).
    LeftOver,
}

/// A repair obligation opened by a `ContinueForRepair` command step (RX-F2b).
/// The obligation stays live until the run reaches terminal success.
#[derive(Debug, Clone)]
pub struct Obligation {
    pub ref_name: String,
    pub opened_at_step_idx: usize,
    /// Number of diagnostics captured when the obligation was opened.
    pub captured_count: usize,
    pub status: ObligationStatus,
    /// Non-zero only when status is `LeftOver`.
    pub leftover_count: usize,
}

/// Serializable summary of one obligation for the run response.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ObligationReport {
    pub ref_name: String,
    pub opened_at_step_idx: usize,
    pub captured_count: usize,
    pub leftover_count: usize,
    /// `"open"`, `"consumed"`, or `"left_over"`.
    pub status: String,
}

/// Per-run scratch for captured diagnostic output (RX-F2a/F2b).
/// Keyed by a named ref string; the default ref is `"last"`.
#[derive(Default)]
pub struct RunCaptureContext {
    captures: HashMap<String, Vec<RustcDiagnostic>>,
    /// Repair obligations opened by ContinueForRepair steps (RX-F2b).
    obligations: Vec<Obligation>,
    /// Index into the run's snapshots vec at the time the first soft fail opened.
    /// Rollback on non-terminal-success goes to this position.
    first_soft_fail_snapshot_idx: Option<usize>,
}

impl RunCaptureContext {
    fn stash(&mut self, ref_name: String, diagnostics: Vec<RustcDiagnostic>) {
        self.captures.insert(ref_name, diagnostics);
    }

    /// Retrieve the diagnostics stashed under a named ref (for F2b use).
    #[allow(dead_code)]
    pub fn get(&self, ref_name: &str) -> Option<&[RustcDiagnostic]> {
        self.captures.get(ref_name).map(|v| v.as_slice())
    }

    /// Open a repair obligation for a soft-failed step (RX-F2b).
    fn open_obligation(&mut self, ref_name: String, step_idx: usize, snapshot_idx: usize) {
        let captured_count = self.captures.get(&ref_name).map_or(0, |v| v.len());
        self.obligations.push(Obligation {
            ref_name,
            opened_at_step_idx: step_idx,
            captured_count,
            status: ObligationStatus::Open,
            leftover_count: 0,
        });
        if self.first_soft_fail_snapshot_idx.is_none() {
            self.first_soft_fail_snapshot_idx = Some(snapshot_idx);
        }
    }

    /// Mark the first Open obligation for `ref_name` as Consumed or LeftOver.
    /// Called by repair plan kinds (RX-C1) or test stubs (RX-F2b).
    #[allow(dead_code)]
    pub fn mark_obligation_consumed(&mut self, ref_name: &str, leftover_count: usize) {
        if let Some(ob) = self
            .obligations
            .iter_mut()
            .find(|ob| ob.ref_name == ref_name && ob.status == ObligationStatus::Open)
        {
            ob.leftover_count = leftover_count;
            ob.status = if leftover_count > 0 {
                ObligationStatus::LeftOver
            } else {
                ObligationStatus::Consumed
            };
        }
    }

    fn has_any_obligation(&self) -> bool {
        !self.obligations.is_empty()
    }

    /// True when every obligation is Consumed or LeftOver (none Open).
    fn all_obligations_resolved(&self) -> bool {
        self.obligations
            .iter()
            .all(|ob| ob.status != ObligationStatus::Open)
    }

    /// Snapshot-list index where rollback should begin (first soft-fail cursor).
    fn first_soft_fail_snapshot_idx(&self) -> Option<usize> {
        self.first_soft_fail_snapshot_idx
    }

    fn obligation_reports(&self) -> Vec<ObligationReport> {
        self.obligations
            .iter()
            .map(|ob| ObligationReport {
                ref_name: ob.ref_name.clone(),
                opened_at_step_idx: ob.opened_at_step_idx,
                captured_count: ob.captured_count,
                leftover_count: ob.leftover_count,
                status: match ob.status {
                    ObligationStatus::Open => "open".to_string(),
                    ObligationStatus::Consumed => "consumed".to_string(),
                    ObligationStatus::LeftOver => "left_over".to_string(),
                },
            })
            .collect()
    }
}

/// A single `compiler-message` entry from `cargo --message-format=json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustcDiagnostic {
    pub level: String,
    /// The short diagnostic code (e.g. `"E0308"`), if any.
    pub code: Option<String>,
    pub message: String,
    /// Span objects from the cargo message; stored as raw JSON for forward-compat.
    pub spans: Vec<serde_json::Value>,
    /// Child diagnostics / suggestions; stored as raw JSON.
    pub children: Vec<serde_json::Value>,
}

/// Parse `cargo --message-format=json` stdout into `RustcDiagnostic` entries.
/// Malformed or non-`compiler-message` lines are silently dropped.
pub fn parse_rustc_json_output(stdout: &[u8]) -> Vec<RustcDiagnostic> {
    let text = String::from_utf8_lossy(stdout);
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = val.get("message") else {
            continue;
        };
        let level = msg
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let message = msg
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let spans = msg
            .get("spans")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let children = msg
            .get("children")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        out.push(RustcDiagnostic { level, code, message, spans, children });
    }
    out
}

fn run_validation_command(
    project_dir: &Path,
    command: &str,
    args: &[String],
    cwd: &Option<String>,
) -> Result<CommandStepResult> {
    let working_dir = match cwd.as_deref() {
        Some(cwd) => resolve_path(Some(&path_string(project_dir)), cwd)?,
        None => project_dir.to_path_buf(),
    };
    if command.chars().any(char::is_whitespace) {
        return Ok(CommandStepResult {
            success: false,
            error: Some("command must be an executable path without whitespace; put arguments in args, e.g. command=\"cargo\", args=[\"fmt\"], not command=\"cargo fmt\"".to_string()),
            stdout: Vec::new(),
        });
    }
    let mut cmd = Command::new(command);
    cmd.args(args).current_dir(&working_dir);
    for key in BLACKBOX_SERVICE_ENV_VARS {
        cmd.env_remove(key);
    }
    let output = cmd.output().with_context(|| {
        format!(
            "running validation command `{}` in {}",
            command_display(command, args),
            working_dir.display()
        )
    })?;
    let raw_stdout = output.stdout;
    if output.status.success() {
        return Ok(CommandStepResult {
            success: true,
            error: None,
            stdout: raw_stdout,
        });
    }
    let stdout_text = String::from_utf8_lossy(&raw_stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(CommandStepResult {
        success: false,
        error: Some(format!(
            "exit status: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            truncate_for_report(&stdout_text, 4000),
            truncate_for_report(&stderr, 4000)
        )),
        stdout: raw_stdout,
    })
}

fn command_display(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_for_report(value: &str, max_chars: usize) -> String {
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    if max_chars < 32 {
        return value.chars().take(max_chars).collect();
    }
    let marker = "\n[truncated middle]\n";
    let budget = max_chars.saturating_sub(marker.chars().count());
    let head_len = budget / 2;
    let tail_len = budget - head_len;
    let head = value.chars().take(head_len).collect::<String>();
    let tail = value
        .chars()
        .skip(char_count.saturating_sub(tail_len))
        .collect::<String>();
    format!("{head}{marker}{tail}")
}

fn plan_move_file(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for move_file"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }
    if !source_path.is_file() {
        bail!("source file does not exist: {}", source_path.display());
    }
    if target_path.exists() {
        bail!("target already exists: {}", target_path.display());
    }
    let original = fs::read(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let validations = parse_validation_step_for_path(&target_path);
    let plan = RefactorPlan {
        title: format!(
            "move file {} to {}",
            path_string(&source_path),
            path_string(&target_path)
        ),
        kind: "move_file".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: vec![FileMove {
            source_path: path_string(&source_path),
            target_path: path_string(&target_path),
            original_sha256: sha256_hex(&original),
        }],
        edits: Vec::new(),
        validations,
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_replace_text(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let old_text = p
        .old_text
        .as_deref()
        .ok_or_else(|| anyhow!("old_text is required for replace_text"))?;
    if old_text.is_empty() {
        bail!("old_text must not be empty");
    }
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for replace_text"))?;
    let matches = source.match_indices(old_text).collect::<Vec<_>>();
    if matches.is_empty() {
        bail!("old_text was not found in {}", source_path.display());
    }
    if p.replace_all != Some(true) && matches.len() > 1 {
        bail!(
            "old_text matched {} times in {}; pass replace_all=true or use a more specific old_text",
            matches.len(),
            source_path.display()
        );
    }
    let edits = if p.replace_all == Some(true) {
        matches
            .iter()
            .map(|(start, text)| TextEdit {
                byte_start: *start,
                byte_end: start + text.len(),
                replacement: new_text.to_string(),
            })
            .collect::<Vec<_>>()
    } else {
        let (start, text) = matches[0];
        vec![TextEdit {
            byte_start: start,
            byte_end: start + text.len(),
            replacement: new_text.to_string(),
        }]
    };
    let plan = RefactorPlan {
        title: format!(
            "replace exact text in {} ({} occurrence(s))",
            path_string(&source_path),
            edits.len()
        ),
        kind: "replace_text".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_write_file(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path).unwrap_or_default();
    let new_text = p
        .new_text
        .as_deref()
        .ok_or_else(|| anyhow!("new_text is required for write_file"))?;
    let plan = RefactorPlan {
        title: format!("write complete file {}", path_string(&source_path)),
        kind: "write_file".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: source.len(),
                replacement: new_text.to_string(),
            }],
            new_text: None,
        }],
        validations: parse_validation_step_for_path(&source_path),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn plan_ensure_toml_table(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let source = fs::read_to_string(&source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let table = p
        .toml_table
        .as_deref()
        .ok_or_else(|| anyhow!("toml_table is required for ensure_toml_table"))?;
    validate_toml_table_name(table)?;
    let entries = p
        .toml_entries
        .as_ref()
        .filter(|entries| !entries.is_empty())
        .ok_or_else(|| anyhow!("toml_entries is required for ensure_toml_table"))?;
    let replacement = ensure_toml_table_content(&source, table, entries)?;
    replacement
        .parse::<toml::Value>()
        .with_context(|| format!("planned TOML for {} is invalid", source_path.display()))?;
    let plan = RefactorPlan {
        title: format!(
            "ensure TOML table [{table}] in {}",
            path_string(&source_path)
        ),
        kind: "ensure_toml_table".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        edits: vec![FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(source.as_bytes()),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: source.len(),
                replacement,
            }],
            new_text: None,
        }],
        validations: Vec::new(),
        items: Vec::new(),
        leftovers: Vec::new(),
        captured_variables: Vec::new(),
        remaining_source_accessors: Vec::new(),
        remaining_source_constant_refs: Vec::new(),
        external_calls: Vec::new(),
        inherited_dependencies: Vec::new(),
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
    };

    validate_plan_shape(&plan)?;
    Ok(serde_json::to_string_pretty(&plan)?)
}

fn find_node<'tree>(
    root: Node<'tree>,
    mut predicate: impl FnMut(Node<'tree>) -> bool,
) -> Option<Node<'tree>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if predicate(node) {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    None
}

fn router_matches(parsed: &ParsedSource, impl_node: Node<'_>, router_name: Option<&str>) -> bool {
    let Some(router_name) = router_name else {
        return true;
    };
    let leading = leading_trivia_start(&parsed.source, impl_node);
    let exact_router = format!("router={router_name}");
    attached_attributes(&parsed.source, leading, impl_node.start_byte())
        .iter()
        .map(|attr| {
            attr.chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        })
        .any(|attr| {
            let router_arg = attr.contains(&format!("{exact_router})"))
                || attr.contains(&format!("{exact_router},"));
            attr.contains("tool_router") && router_arg
        })
}

fn impl_declaration_list(impl_node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = impl_node.walk();
    let body = impl_node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "declaration_list");
    body
}

pub(crate) fn resolve_path(project_dir: Option<&str>, path: &str) -> Result<PathBuf> {
    let path = PathBuf::from(path);
    let full = if path.is_absolute() {
        path
    } else if let Some(project_dir) = project_dir {
        PathBuf::from(project_dir).join(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(full)
}

fn resolve_project_dir(project_dir: Option<&str>) -> Result<PathBuf> {
    let root = match project_dir {
        Some(project_dir) => PathBuf::from(project_dir),
        None => std::env::current_dir()?,
    };
    root.canonicalize()
        .with_context(|| format!("failed to canonicalize project_dir {}", root.display()))
}

fn chunk_file_for_refs(
    abs_path: &Path,
    rel_path: &Path,
    project_id: &str,
    rel_path_hash: &str,
) -> Result<Vec<chunker::Chunk>> {
    let bytes =
        fs::read(abs_path).with_context(|| format!("failed to read {}", abs_path.display()))?;
    let sniff_len = bytes.len().min(4096);
    let mut chunks = Vec::new();
    for candidate in chunker::default_registry() {
        if !candidate.claims(rel_path, &bytes[..sniff_len]) {
            continue;
        }
        chunks = candidate.chunk(rel_path, &bytes)?.0;
        break;
    }
    if chunks.is_empty() && !bytes.is_empty() {
        bail!("no chunker claimed {}", rel_path.display());
    }
    let chunks = chunks
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.project_id = project_id.to_string();
            chunk.file_path = rel_path.to_path_buf();
            chunk.rel_path_hash = rel_path_hash.to_string();
            chunk.chunk_hash = sha256_hex(chunk.content.as_bytes());
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect::<Vec<_>>();
    Ok(bound_chunks_for_refs(&chunks))
}

fn bound_chunks_for_refs(chunks: &[chunker::Chunk]) -> Vec<chunker::Chunk> {
    chunks
        .iter()
        .flat_map(|chunk| {
            if chunk.content.len() <= chunker::MAX_CHUNK_BYTES {
                return vec![chunk.clone()];
            }
            let mut out = Vec::new();
            let mut start = 0usize;
            while start < chunk.content.len() {
                let mut end = (start + chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
                while !chunk.content.is_char_boundary(end) {
                    end -= 1;
                }
                let mut split = chunk.clone();
                split.content = chunk.content[start..end].to_string();
                split.byte_start = chunk.byte_start + start as u64;
                split.byte_end = chunk.byte_start + end as u64;
                split.chunk_hash = sha256_hex(split.content.as_bytes());
                split.occurrence_idx = out.len() as u32;
                out.push(split);
                start = end;
            }
            out
        })
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

fn excerpt(text: &str, max_chars: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(max_chars).collect()
}

fn parse_source_file(path: &Path) -> Result<ParsedSource> {
    let language =
        language_for_path(path).ok_or_else(|| anyhow!("unsupported source file extension"))?;
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let tree = parse_source(language, &source)?;
    Ok(ParsedSource {
        path: path.to_path_buf(),
        language,
        source,
        tree,
    })
}

fn parse_source(language: &str, source: &str) -> Result<Tree> {
    let mut parser = parser_for_language(language)?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter {language} parser returned no tree"))
}

fn generic_top_level_items(parsed: &ParsedSource) -> Vec<SyntaxItem> {
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .map(|node| syntax_item(parsed, node))
        .collect()
}

fn syntax_item(parsed: &ParsedSource, node: Node<'_>) -> SyntaxItem {
    syntax_item_with_kind(parsed, node, node.kind())
}

fn syntax_item_with_kind(parsed: &ParsedSource, node: Node<'_>, kind: &str) -> SyntaxItem {
    let name = item_name(node, &parsed.source, parsed.language);
    let leading_trivia_start = leading_trivia_start(&parsed.source, node);
    let trailing_trivia_end = trailing_trivia_end(&parsed.source, node.end_byte());
    let attributes = attached_attributes(&parsed.source, leading_trivia_start, node.start_byte());
    let (line_start, _) = line_col(&parsed.source, node.start_byte());
    let (line_end, _) = line_col(&parsed.source, node.end_byte());
    let display_name = name.clone().unwrap_or_else(|| "(unnamed)".to_string());
    SyntaxItem {
        plan_local_id: format!(
            "{}:{}:{}:{}:{}",
            path_string(&parsed.path),
            node.start_byte(),
            node.end_byte(),
            kind,
            display_name
        ),
        kind: kind.to_string(),
        name,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        leading_trivia_start,
        trailing_trivia_end,
        line_start,
        line_end,
        attributes,
    }
}

fn is_top_level_item(kind: &str) -> bool {
    matches!(
        kind,
        "mod_item"
            | "use_declaration"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "function_item"
            | "impl_item"
            | "macro_definition"
            | "const_item"
            | "static_item"
            | "type_item"
    )
}

fn item_name(node: Node<'_>, source: &str, language: &str) -> Option<String> {
    if language == "rust" && node.kind() == "impl_item" {
        return node
            .utf8_text(source.as_bytes())
            .ok()
            .and_then(|text| text.split('{').next())
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty());
    }
    node.child_by_field_name("name")
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_string)
        .or_else(|| {
            node.utf8_text(source.as_bytes())
                .ok()
                .and_then(first_identifier_after_keyword)
        })
}

fn first_identifier_after_keyword(text: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "fn",
        "function",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "type",
        "const",
        "let",
        "var",
        "def",
        "module",
    ];
    let mut prev_was_keyword = false;
    for token in text.split(|ch: char| ch != '_' && !ch.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        if prev_was_keyword {
            return Some(token.to_string());
        }
        prev_was_keyword = KEYWORDS.contains(&token);
    }
    None
}

fn leading_trivia_start(source: &str, node: Node<'_>) -> usize {
    let mut line_start = line_start_before(source, node.start_byte());
    let mut prev = node.prev_named_sibling();
    while let Some(sibling) = prev {
        let gap = source
            .get(sibling.end_byte()..line_start)
            .unwrap_or_default();
        if !gap.trim().is_empty() || !is_attachable_trivia_node(sibling.kind()) {
            break;
        }
        line_start = line_start_before(source, sibling.start_byte());
        prev = sibling.prev_named_sibling();
    }

    let mut current = line_start;
    let mut attached = false;
    while current > 0 {
        let prev_end = current.saturating_sub(1);
        let prev_start = line_start_before(source, prev_end);
        let line = &source[prev_start..current];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        let is_attr_or_doc = trimmed.starts_with("#[")
            || trimmed.starts_with("#![")
            || trimmed.starts_with("///")
            || trimmed.starts_with("//!");
        let is_comment = trimmed.starts_with("//");
        if is_attr_or_doc || (attached && is_comment) {
            line_start = prev_start;
            current = prev_start;
            attached = true;
        } else {
            break;
        }
    }
    line_start
}

fn is_attachable_trivia_node(kind: &str) -> bool {
    matches!(kind, "attribute_item" | "line_comment" | "block_comment")
}

fn trailing_trivia_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = end;
    let mut newline_count = 0usize;
    while idx < bytes.len() {
        match bytes[idx] {
            b' ' | b'\t' | b'\r' => idx += 1,
            b'\n' => {
                idx += 1;
                newline_count += 1;
                if newline_count >= 2 {
                    break;
                }
            }
            _ => break,
        }
    }
    idx
}

fn attached_attributes(source: &str, trivia_start: usize, item_start: usize) -> Vec<String> {
    source[trivia_start..item_start]
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("#[") || line.starts_with("#!["))
        .map(str::to_string)
        .collect()
}

fn line_start_before(source: &str, idx: usize) -> usize {
    source[..idx.min(source.len())]
        .rfind('\n')
        .map(|pos| pos + 1)
        .unwrap_or(0)
}

pub(super) fn line_col(source: &str, idx: usize) -> (usize, usize) {
    let idx = idx.min(source.len());
    let line = source[..idx].bytes().filter(|b| *b == b'\n').count() + 1;
    let col = idx - line_start_before(source, idx) + 1;
    (line, col)
}

fn select_items<'a>(
    items: &'a [SyntaxItem],
    names: Option<&[String]>,
    kinds: Option<&[String]>,
) -> Result<Vec<&'a SyntaxItem>> {
    let has_names = names.is_some_and(|xs| !xs.is_empty());
    let has_kinds = kinds.is_some_and(|xs| !xs.is_empty());
    if !has_names && !has_kinds {
        bail!("at least one of item_names or item_kinds is required");
    }
    let name_set = names.map(|xs| xs.iter().map(String::as_str).collect::<HashSet<_>>());
    let kind_set = kinds.map(|xs| xs.iter().map(String::as_str).collect::<HashSet<_>>());
    let selected = items
        .iter()
        .filter(|item| {
            name_set
                .as_ref()
                .is_none_or(|set| item.name.as_deref().is_some_and(|name| set.contains(name)))
        })
        .filter(|item| {
            kind_set
                .as_ref()
                .is_none_or(|set| set.contains(item.kind.as_str()))
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        bail!("no matching Rust items found");
    }
    if let Some(names) = names {
        for expected in names {
            if !selected
                .iter()
                .any(|item| item.name.as_deref() == Some(expected.as_str()))
            {
                bail!("requested item `{expected}` was not found");
            }
        }
    }
    Ok(selected)
}

fn validate_plan_shape(plan: &RefactorPlan) -> Result<()> {
    if plan.edits.is_empty() && plan.file_moves.is_empty() {
        bail!("plan has no edits or file moves");
    }
    let mut paths = HashSet::new();
    for file_move in &plan.file_moves {
        if file_move.source_path == file_move.target_path {
            bail!("file move source and target must differ");
        }
        if !paths.insert(file_move.source_path.clone()) {
            bail!("duplicate refactor path {}", file_move.source_path);
        }
        if !paths.insert(file_move.target_path.clone()) {
            bail!("duplicate refactor path {}", file_move.target_path);
        }
    }
    for edit in &plan.edits {
        if !paths.insert(edit.path.clone()) {
            bail!("duplicate refactor path {}", edit.path);
        }
        ensure_non_overlapping(&edit.edits)
            .with_context(|| format!("overlapping edits in {}", edit.path))?;
    }
    Ok(())
}

fn parse_validation_step_for_path(path: &Path) -> Vec<ValidationStep> {
    if language_for_path(path).is_some() {
        vec![ValidationStep::TreeSitterNoErrors {
            path: path_string(path),
            byte_range: None,
        }]
    } else {
        Vec::new()
    }
}

fn validate_toml_table_name(table: &str) -> Result<()> {
    if table.trim() != table || table.is_empty() {
        bail!("toml_table must be a non-empty top-level table name");
    }
    if table
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
    {
        bail!("toml_table must be an unquoted top-level TOML table name");
    }
    Ok(())
}

fn ensure_toml_table_content(
    source: &str,
    table: &str,
    entries: &BTreeMap<String, serde_json::Value>,
) -> Result<String> {
    source
        .parse::<toml::Value>()
        .context("source TOML is invalid")?;
    let formatted_entries = entries
        .iter()
        .map(|(key, value)| {
            validate_toml_key(key)?;
            Ok((key.as_str(), toml_literal(value)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let header = format!("[{table}]");
    let mut out = source.to_string();
    let Some((section_start, section_end)) = toml_top_level_section_range(source, &header) else {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") && !out.trim().is_empty() {
            out.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        for (key, value) in formatted_entries {
            out.push_str(key);
            out.push_str(" = ");
            out.push_str(&value);
            out.push('\n');
        }
        return Ok(out);
    };

    let section = &source[section_start..section_end];
    let mut section_out = section.to_string();
    for (key, value) in formatted_entries {
        let replacement_line = format!("{key} = {value}");
        if let Some((line_start, line_end)) = toml_key_line_range(&section_out, key) {
            section_out.replace_range(line_start..line_end, &replacement_line);
        } else {
            if !section_out.ends_with('\n') {
                section_out.push('\n');
            }
            section_out.push_str(&replacement_line);
            section_out.push('\n');
        }
    }
    out.replace_range(section_start..section_end, &section_out);
    Ok(out)
}

fn validate_toml_key(key: &str) -> Result<()> {
    if key.trim() != key || key.is_empty() {
        bail!("TOML entry keys must be non-empty bare keys");
    }
    if key
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
    {
        bail!("unsupported TOML key `{key}`; only bare keys are supported");
    }
    Ok(())
}

fn toml_top_level_section_range(source: &str, header: &str) -> Option<(usize, usize)> {
    let mut found_start = None;
    for (line_start, line) in source.split_inclusive('\n').scan(0usize, |offset, line| {
        let start = *offset;
        *offset += line.len();
        Some((start, line))
    }) {
        let trimmed = line.trim();
        if found_start.is_none() {
            if trimmed == header {
                found_start = Some(line_start);
            }
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return found_start.map(|start| (start, line_start));
        }
    }
    found_start.map(|start| (start, source.len()))
}

fn toml_key_line_range(section: &str, key: &str) -> Option<(usize, usize)> {
    let mut offset = 0usize;
    for line in section.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            let without_comment = trimmed.split('#').next().unwrap_or_default();
            if let Some((lhs, _)) = without_comment.split_once('=') {
                if lhs.trim() == key {
                    let line_end = offset + line.trim_end_matches(['\r', '\n']).len();
                    return Some((offset, line_end));
                }
            }
        }
        offset += line.len();
    }
    None
}

fn toml_literal(value: &serde_json::Value) -> Result<String> {
    Ok(match value {
        serde_json::Value::String(value) => serde_json::to_string(value)?,
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(values) => {
            let values = values
                .iter()
                .map(toml_literal)
                .collect::<Result<Vec<_>>>()?;
            format!("[{}]", values.join(", "))
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => {
            bail!("unsupported TOML value {value}; use string, bool, number, or array")
        }
    })
}

fn restore_snapshots(snapshots: &[(PathBuf, Option<Vec<u8>>)]) -> Vec<String> {
    let mut errors = Vec::new();
    for (path, original) in snapshots.iter().rev() {
        let result = match original {
            Some(bytes) => write_atomic(path, bytes),
            None => match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
            },
        };
        if let Err(err) = result {
            errors.push(format!("{}: {err:#}", path.display()));
        }
    }
    errors
}

/// Roll back snapshots starting at `cursor` (the first-soft-fail snapshot index).
/// When `cursor` is None, rolls back all snapshots (same as `restore_snapshots`).
/// This implements the first_live_soft_fail_cursor rollback policy (RX-F2b).
fn restore_snapshots_from(
    snapshots: &[(PathBuf, Option<Vec<u8>>)],
    cursor: Option<usize>,
) -> Vec<String> {
    let start = cursor.unwrap_or(0);
    restore_snapshots(&snapshots[start..])
}

fn ensure_non_overlapping(edits: &[TextEdit]) -> Result<()> {
    let mut ranges = edits
        .iter()
        .map(|edit| (edit.byte_start, edit.byte_end))
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!(
                "overlapping edits: {}..{} overlaps {}..{}",
                pair[0].0,
                pair[0].1,
                pair[1].0,
                pair[1].1
            );
        }
    }
    Ok(())
}

pub(crate) fn apply_text_edits(source: &str, edits: &[TextEdit]) -> Result<String> {
    ensure_non_overlapping(edits)?;
    let mut out = source.to_string();
    let mut sorted = edits.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|edit| edit.byte_start);
    for edit in sorted.into_iter().rev() {
        if edit.byte_start > edit.byte_end || edit.byte_end > out.len() {
            bail!("invalid edit range {}..{}", edit.byte_start, edit.byte_end);
        }
        if !out.is_char_boundary(edit.byte_start) || !out.is_char_boundary(edit.byte_end) {
            bail!(
                "edit range {}..{} is not UTF-8 aligned",
                edit.byte_start,
                edit.byte_end
            );
        }
        out.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
    }
    Ok(out)
}

fn read_original_for_edit(path: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound && expected_sha256 == sha256_hex(&[]) =>
        {
            Ok(Vec::new())
        }
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn ensure_path_in_registered_project(path: &Path, projects: &[ProjectRecord]) -> Result<()> {
    if projects.is_empty() {
        bail!("refactor apply requires at least one registered project");
    }
    let scoped = canonicalize_existing_or_parent(path)?;
    let allowed = projects.iter().any(|project| {
        let root = Path::new(&project.canonical_path);
        scoped == root || scoped.starts_with(root)
    });
    if !allowed {
        bail!(
            "refusing to apply {}: path is outside registered projects",
            path.display()
        );
    }
    Ok(())
}

fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path)
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
    let mut ancestor = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| anyhow!("{} has no existing parent directory", path.display()))?;
    }
    let canonical_ancestor = fs::canonicalize(ancestor)
        .with_context(|| format!("canonicalizing parent {}", ancestor.display()))?;
    let suffix = path
        .strip_prefix(ancestor)
        .with_context(|| format!("resolving missing suffix for {}", path.display()))?;
    Ok(canonical_ancestor.join(suffix))
}

fn ensure_git_clean_for_path(path: &Path) -> Result<()> {
    let Some(root) = crate::entity_ref::git_root_for_path(path) else {
        return Ok(());
    };
    let rel = path.strip_prefix(&root).unwrap_or(path);
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .arg("status")
        .arg("--porcelain")
        .arg("--")
        .arg(rel)
        .output()
        .with_context(|| format!("checking git status for {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "git status failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !output.stdout.is_empty() {
        bail!(
            "refusing to apply {}: file is dirty in git; pass allow_dirty_worktree=true to override",
            path.display()
        );
    }
    Ok(())
}

fn validate_rewritten_files(files: &[(PathBuf, Vec<u8>)]) -> Result<Vec<ParseValidationResult>> {
    files
        .iter()
        .filter_map(|(path, bytes)| {
            let language = language_for_path(path)?;
            Some((path, bytes, language))
        })
        .map(|(path, bytes, language)| {
            let source = std::str::from_utf8(bytes)
                .with_context(|| format!("{} is not valid utf-8", path.display()))?;
            let tree = parse_source(language, source)?;
            let (report, error_locations) = parse_report_with_locations(tree.root_node());
            let error_excerpts = if report.has_error || report.error_nodes > 0 || report.missing_nodes > 0 {
                build_parse_error_excerpts(source, &error_locations)
            } else {
                Vec::new()
            };
            Ok(ParseValidationResult {
                path: path_string(path),
                has_error: report.has_error,
                error_nodes: report.error_nodes,
                missing_nodes: report.missing_nodes,
                error_excerpts,
            })
        })
        .collect()
}

pub(crate) fn parse_report(root: Node<'_>) -> ParseReport {
    parse_report_with_locations(root).0
}

/// Cap on how many error/missing node locations we surface per file.
const PARSE_ERROR_EXCERPT_LIMIT: usize = 5;

#[derive(Clone, Copy)]
struct ParseErrorLocation {
    byte_start: usize,
    byte_end: usize,
    is_missing: bool,
}

fn parse_report_with_locations(
    root: Node<'_>,
) -> (ParseReport, Vec<ParseErrorLocation>) {
    let mut report = ParseReport {
        has_error: root.has_error(),
        error_nodes: 0,
        missing_nodes: 0,
    };
    let mut locations = Vec::new();
    collect_parse_report(root, &mut report, &mut locations);
    (report, locations)
}

fn collect_parse_report(
    node: Node<'_>,
    report: &mut ParseReport,
    locations: &mut Vec<ParseErrorLocation>,
) {
    let is_err = node.is_error();
    let is_missing = node.is_missing();
    if is_err {
        report.error_nodes += 1;
    }
    if is_missing {
        report.missing_nodes += 1;
    }
    if (is_err || is_missing) && locations.len() < PARSE_ERROR_EXCERPT_LIMIT {
        locations.push(ParseErrorLocation {
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            is_missing,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_parse_report(child, report, locations);
    }
}

fn build_parse_error_excerpts(
    source: &str,
    locations: &[ParseErrorLocation],
) -> Vec<ParseErrorExcerpt> {
    locations
        .iter()
        .map(|loc| {
            let (line, column) = line_col_1based(source, loc.byte_start);
            let snippet = render_snippet_window(source, line, 1);
            ParseErrorExcerpt {
                kind: if loc.is_missing { "missing" } else { "error" }.to_string(),
                line,
                column,
                byte_start: loc.byte_start,
                byte_end: loc.byte_end,
                snippet,
            }
        })
        .collect()
}

fn line_col_1based(source: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = byte_offset.min(source.len());
    let prefix = &source[..clamped];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let last_newline = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = source[last_newline..clamped].chars().count() + 1;
    (line, column)
}

/// Render a `context`-line window above and below `line` (1-based) with a
/// `NNN | ` line-number gutter. Trims trailing whitespace.
fn render_snippet_window(source: &str, line: usize, context: usize) -> String {
    let start = line.saturating_sub(context).max(1);
    let lines: Vec<&str> = source.split('\n').collect();
    let end = (line + context).min(lines.len());
    if start > lines.len() {
        return String::new();
    }
    let width = end.to_string().len();
    let mut out = String::new();
    for (idx, ln) in lines.iter().enumerate().skip(start - 1).take(end - start + 1) {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{:>width$} | {}", idx + 1, ln.trim_end(), width = width));
    }
    out
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path)
        .map_err(|err| anyhow!("persist failed: {}", err.error))?;
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
