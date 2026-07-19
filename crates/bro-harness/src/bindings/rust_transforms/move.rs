//! `rust.extractItems` and `rust.inlineModToFile` - the move/extract bindings.
//!
//! Ports of the v1 `extract_rust_items_to_submodule` (compound) +
//! `extract_rust_items` + `move_rust_items_with_local_deps` +
//! `extract_rust_section` (all folded into `rust.extractItems`) and
//! `inline_mod_to_file_submodule` (`rust.inlineModToFile`). Each is a thin
//! adapter over `bbox_refactor::plan` (design §3.1, §7): it runs the v1
//! analysis/synthesis verbatim, strips the MCP/plan-apply envelope, and
//! returns `{changes, creates, findings}` for the edits algebra. NEVER
//! writes; the cell feeds `changes` into `edits.merge` and `creates` into
//! `edits.createFile`, then `edits.apply`.
//!
//! `rust.extractItems` knob boundary (design §8.3): the five knobs select
//! the SYNTHESIS SHAPE; the host dependency analysis runs ALWAYS and reports
//! in `findings` (never knob-gated). Plain mode (knobs unset) delegates to
//! `extract_rust_items`; compound mode (the default) delegates to
//! `extract_rust_items_to_submodule` which bakes visibility bumps + the
//! `mod`/`use` decls into one pass.

use std::sync::Arc;

use async_trait::async_trait;
use bbox_refactor::RefactorPlanParams;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde::Deserialize;
use serde_json::{Value, json};

use super::helpers::{
    PlanProjection, done_hint, plan_to_changes_creates, relativize, resolve_workspace_file,
};
use crate::bindings::ledger::ProvenanceLedger;

// ───────────────────────────── extractItems ─────────────────────────────

/// `rust.extractItems` - move top-level Rust items into a (new) submodule.
///
/// Plain extract when wiring knobs unset; compound mode (default) does
/// scaffolded target + `mod <name>;` in parent + visibility bumps on moved
/// items AND their struct fields + auto-pruned `use <module>::{...};`
/// re-import. Per §8.3, capture/borrow analysis runs always and reports in
/// `findings`; the knobs only select the synthesis shape.
pub struct RustExtractItems(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
struct ExtractItemsInput {
    /// Source (parent) module file, workspace-relative.
    source: String,
    /// Target submodule file, workspace-relative. Required.
    target: String,
    /// Top-level item names to move. Required.
    #[serde(default, rename = "itemNames", alias = "item_names")]
    item_names: Vec<String>,
    /// Optional syntax item kinds (narrow ambiguous names).
    #[serde(default, rename = "itemKinds", alias = "item_kinds")]
    item_kinds: Option<Vec<String>>,
    /// Module name for the parent's `mod <name>;` declaration. Defaults to
    /// the target file stem (must match it when given).
    #[serde(default, rename = "moduleName", alias = "module_name")]
    module_name: Option<String>,
    /// Visibility floor for moved items AND struct fields. Compound mode
    /// only. Defaults to `pub(super)`.
    #[serde(default)]
    visibility: Option<String>,
    /// Prelude inserted at the top of the new file. Compound mode only.
    /// Defaults to `use super::*;`.
    #[serde(default, rename = "targetPrelude", alias = "target_prelude")]
    target_prelude: Option<String>,
    /// Knob (§8.3): move the exclusive private dependency closure of the
    /// seed items instead of just the seeds. Selects
    /// `move_rust_items_with_local_deps` synthesis shape.
    #[serde(default, rename = "withLocalDeps", alias = "with_local_deps")]
    with_local_deps: Option<bool>,
    /// Knob (§8.3): section addressing mode. `{start_marker, end_marker}` or
    /// `{start_line, end_line}` select `extract_rust_section` synthesis.
    #[serde(default, rename = "section", alias = "section")]
    section: Option<SectionBounds>,
    /// Knob (§8.3): append to an existing non-empty target instead of
    /// refusing. Compound mode only.
    #[serde(default, rename = "mergeIntoExistingTarget", alias = "merge_into_existing_target")]
    merge_into_existing_target: Option<bool>,
    /// Knob (§8.3): visibility of the auto-emitted parent re-export.
    /// Compound mode only. `private` (default), `pub`, `pub(crate)`,
    /// `pub(super)`.
    #[serde(default, rename = "useDeclVisibility", alias = "use_decl_visibility")]
    use_decl_visibility: Option<String>,
    /// Knob (§8.3): explicit subset of item_names to re-export. Compound
    /// mode only. Defaults to auto-prune (only names still referenced in
    /// the post-deletion source).
    #[serde(default, rename = "useDeclItems", alias = "use_decl_items")]
    use_decl_items: Option<Vec<String>>,
    /// Return findings + metadata but zero `changes`/`creates`.
    #[serde(default, rename = "previewOnly", alias = "preview_only")]
    preview_only: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct SectionBounds {
    #[serde(default, rename = "startMarker", alias = "start_marker")]
    start_marker: Option<String>,
    #[serde(default, rename = "endMarker", alias = "end_marker")]
    end_marker: Option<String>,
    #[serde(default, rename = "startLine", alias = "start_line")]
    start_line: Option<u64>,
    #[serde(default, rename = "endLine", alias = "end_line")]
    end_line: Option<u64>,
}

impl RustExtractItems {
    fn build_params(&self, root: &std::path::Path, input: &ExtractItemsInput) -> Result<(RefactorPlanParams, Mode), String> {
        let source_abs = resolve_workspace_file(root, &input.source, "rust.extractItems")?;
        let target_abs = resolve_workspace_file(root, &input.target, "rust.extractItems")?;
        let mut entries = std::collections::BTreeMap::new();
        if let Some(true) = input.merge_into_existing_target {
            entries.insert("merge_into_existing_target".to_string(), Value::Bool(true));
        }
        if let Some(vis) = input.use_decl_visibility.as_deref() {
            entries.insert(
                "use_decl_visibility".to_string(),
                Value::String(vis.to_string()),
            );
        }
        if let Some(items) = input.use_decl_items.as_ref() {
            entries.insert(
                "use_decl_items".to_string(),
                Value::Array(items.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }

        // Mode resolution precedence: section > with_local_deps > compound
        // (default). Plain mode is NOT the default for extractItems: the
        // compound shape is the v1 default and what splits a monster module
        // in one pass. Plain (knobs all unset AND no compound-specific
        // fields) would drop the mod/use wiring, which is almost never
        // desired; but when `section`/`with_local_deps`/visibility/prelude/
        // use-decl knobs are ALL unset we still use compound because the
        // defaults (`pub(super)`, `use super::*;`) are the right call.
        let mode = if input.section.is_some() {
            Mode::Section
        } else if input.with_local_deps == Some(true) {
            Mode::WithLocalDeps
        } else {
            Mode::Compound
        };

        let kind = match mode {
            Mode::Section => "extract_rust_section",
            Mode::WithLocalDeps => "move_rust_items_with_local_deps",
            Mode::Compound => "extract_rust_items_to_submodule",
        };
        // `extract_rust_section` does not take item_names directly (it derives
        // them from bounds); passing them is harmless because the planner
        // ignores them. But an empty item_names on section mode is valid.
        let item_names: Vec<String> = if matches!(mode, Mode::Section) && input.item_names.is_empty() {
            // Section mode: item_names optional (bounds drive selection).
            // Pass a placeholder the planner tolerates; the section planner
            // overrides item_names from bounds anyway.
            Vec::new()
        } else if input.item_names.is_empty() {
            return Err(
                "rust.extractItems: itemNames is required (or pass section bounds for section mode)"
                    .to_string(),
            );
        } else {
            input.item_names.clone()
        };

        // Section mode entries: the v1 planner reads toml_entries for bounds.
        if let Some(section) = input.section.as_ref() {
            if let Some(m) = section.start_marker.as_deref() {
                entries.insert("start_marker".to_string(), Value::String(m.to_string()));
            }
            if let Some(m) = section.end_marker.as_deref() {
                entries.insert("end_marker".to_string(), Value::String(m.to_string()));
            }
            if let Some(l) = section.start_line {
                entries.insert("start_line".to_string(), Value::Number(l.into()));
            }
            if let Some(l) = section.end_line {
                entries.insert("end_line".to_string(), Value::Number(l.into()));
            }
        }

        let params = RefactorPlanParams {
            kind: kind.to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: Some(target_abs.to_string_lossy().into_owned()),
            item_names: Some(item_names),
            item_kinds: input.item_kinds.clone(),
            module_name: input.module_name.clone(),
            visibility: input.visibility.clone(),
            target_prelude: input.target_prelude.clone(),
            toml_entries: if entries.is_empty() { None } else { Some(entries) },
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        Ok((params, mode))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compound,
    WithLocalDeps,
    Section,
}

#[async_trait]
impl Tool for RustExtractItems {
    fn name(&self) -> &str {
        "rust.extractItems"
    }
    fn description(&self) -> &str {
        "Move top-level Rust items into a (new) submodule. Compound mode (default): scaffolded target + `mod <name>;` in parent + visibility bumps on moved items and struct fields + auto-pruned `use <module>::{...};` re-import. Knobs: withLocalDeps (move exclusive private dependency closure), section (marker/line bounds), mergeIntoExistingTarget, useDeclVisibility, useDeclItems. Dependency analysis runs always and reports in findings. NEVER writes: feed {changes, creates} into edits.merge/createFile."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source (parent) module file, workspace-relative." },
                "target": { "type": "string", "description": "Target submodule file, workspace-relative. Required." },
                "itemNames": { "type": "array", "items": { "type": "string" }, "description": "Top-level item names to move. Required unless section bounds select the region." },
                "itemKinds": { "type": "array", "items": { "type": "string" }, "description": "Optional syntax item kinds to narrow ambiguous names." },
                "moduleName": { "type": "string", "description": "Module name for the parent `mod <name>;` declaration. Defaults to the target file stem (must match it)." },
                "visibility": { "type": "string", "description": "Visibility floor for moved items AND struct fields. Defaults to `pub(super)`." },
                "targetPrelude": { "type": "string", "description": "Prelude at the top of the new file. Defaults to `use super::*;`." },
                "withLocalDeps": { "type": "boolean", "description": "Move the exclusive private dependency closure of the seed items." },
                "section": {
                    "type": "object",
                    "properties": {
                        "startMarker": { "type": "string" },
                        "endMarker": { "type": "string" },
                        "startLine": { "type": "integer", "minimum": 1 },
                        "endLine": { "type": "integer", "minimum": 1 }
                    },
                    "description": "Section addressing mode: select items by source-region bounds."
                },
                "mergeIntoExistingTarget": { "type": "boolean", "description": "Append to an existing non-empty target instead of refusing." },
                "useDeclVisibility": { "type": "string", "enum": ["private", "pub", "pub(crate)", "pub(super)"], "description": "Visibility of the auto-emitted parent re-export." },
                "useDeclItems": { "type": "array", "items": { "type": "string" }, "description": "Explicit subset of itemNames to re-export. Defaults to auto-prune." },
                "previewOnly": { "type": "boolean", "description": "Return findings + metadata but zero changes/creates." }
            },
            "required": ["source", "target", "itemNames"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations { read_only: true, destructive: false }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "extractItems".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let root = cx.root.clone();
        let args: ExtractItemsInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("rust.extractItems: bad input: {e}")),
        };
        let preview_only = args.preview_only.unwrap_or(false);
        let (params, mode) = match self.build_params(&root, &args) {
            Ok(tuple) => tuple,
            Err(e) => return ToolResult::Error(format!("rust.extractItems: {e}")),
        };

        // Always run the dependency graph analysis (§8.3: analysis never
        // knob-gated). This is the same analyze_top_level the
        // move-with-local-deps planner uses internally; we surface it in
        // findings so the cell sees the closure even in plain/compound mode.
        let source_abs = resolve_workspace_file(&root, &args.source, "rust.extractItems").ok();
        let analysis_findings = match source_abs.as_ref() {
            Some(path) => run_dependency_analysis(path, &root, &args, mode),
            None => Vec::new(),
        };

        let plan_json = match bbox_refactor::plan(&params) {
            Ok(json) => json,
            Err(e) => {
                let msg = e.to_string();
                return ToolResult::Error(format!("rust.extractItems: {msg}{}", done_hint(&msg)));
            }
        };
        let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
            Ok(plan) => plan,
            Err(e) => return ToolResult::Error(format!("rust.extractItems: plan decode: {e}")),
        };
        if plan.plan_status != bbox_refactor::PlanStatus::Planned {
            return ToolResult::Error(format!(
                "rust.extractItems: planner returned {:?} - {}",
                plan.plan_status,
                plan.leftovers.join("; ")
            ));
        }
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(&root, "rust.extractItems", &plan.edits, preview_only) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.extractItems: {e}")),
        };

        // Record host-authored changes at syntax_only so edits.apply can
        // compute semantic_status lineage (the planner is tree-sitter
        // backed; no LSP authority here).
        if !preview_only {
            super::helpers::record_in_ledger(&self.0, "rust.extractItems", &changes);
        }

        // Findings: planner leftovers + the always-on dependency analysis.
        let mut findings: Vec<Value> = Vec::new();
        for note in &plan.leftovers {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        findings.extend(analysis_findings);
        for item in &plan.items {
            let mut finding = serde_json::to_value(item).unwrap_or_default();
            finding["finding"] = json!("moved_item");
            findings.push(finding);
        }

        let mode_str = match mode {
            Mode::Compound => "compound",
            Mode::WithLocalDeps => "with_local_deps",
            Mode::Section => "section",
        };
        ToolResult::Json(json!({
            "title": plan.title,
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "preview_only": preview_only,
            "mode": mode_str,
            "would_change_files": would_change_files,
            "would_create_files": would_create_files,
            "provenance": "syntax_only",
        }))
    }
}

/// Run the top-level dependency analysis (always on, §8.3) and return its
/// closure/edge summary as findings. Failure is non-fatal: the planner's own
/// analysis already ran; this is the surfaced-for-the-cell copy.
fn run_dependency_analysis(
    source_abs: &std::path::Path,
    root: &std::path::Path,
    input: &ExtractItemsInput,
    mode: Mode,
) -> Vec<Value> {
    // Analyze ALL items (pass item_names=None) so the full edge graph is
    // visible; analyze_top_level only records edges between selected items,
    // and the closure computation needs edges to non-seed items too.
    let graph = bbox_refactor::rust_top_level_deps::analyze_top_level(
        source_abs,
        Some(&root.to_string_lossy()),
        None,
        input.item_kinds.as_deref(),
    );
    let graph = match graph {
        Ok(graph) => graph,
        Err(_) => return Vec::new(),
    };
    let mut findings: Vec<Value> = Vec::new();
    // Surface the seed closure when with_local_deps is requested (the
    // planner computed it internally; this is the cell-visible copy).
    if mode == Mode::WithLocalDeps && !input.item_names.is_empty() {
        let seeds: std::collections::BTreeSet<String> =
            input.item_names.iter().cloned().collect();
        let mut outgoing: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut incoming: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for edge in &graph.edges {
            outgoing.entry(edge.from.clone()).or_default().push(edge.to.clone());
            incoming.entry(edge.to.clone()).or_default().push(edge.from.clone());
        }
        let external_refs = graph.external_references.iter().fold(
            std::collections::HashMap::<String, usize>::new(),
            |mut acc, reference| {
                *acc.entry(reference.item.clone()).or_default() += 1;
                acc
            },
        );
        let mut closure = seeds.clone();
        let mut shared = std::collections::BTreeSet::new();
        loop {
            let mut changed = false;
            for source in closure.clone() {
                for target in outgoing.get(&source).into_iter().flatten() {
                    if closure.contains(target) {
                        continue;
                    }
                    let has_external = external_refs.get(target).copied().unwrap_or(0) > 0;
                    let only_in_closure = incoming
                        .get(target)
                        .is_none_or(|sources| sources.iter().all(|s| closure.contains(s)));
                    if !has_external && only_in_closure {
                        closure.insert(target.clone());
                        changed = true;
                    } else {
                        shared.insert(target.clone());
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let added: Vec<String> = closure
            .iter()
            .filter(|name| !seeds.contains(name.as_str()))
            .cloned()
            .collect();
        findings.push(json!({
            "finding": "local_dependency_closure",
            "seeds": input.item_names,
            "added": added,
            "shared_or_external": shared.into_iter().collect::<Vec<_>>(),
        }));
    }
    // Always surface external references (bounded: analyze_top_level caps
    // MAX_EXTERNAL_REFS_PER_ITEM and MAX_EXTERNAL_REF_SCAN_ITEMS).
    if !graph.external_references.is_empty() {
        findings.push(json!({
            "finding": "external_references",
            "count": graph.external_references.len(),
            "samples": graph.external_references.iter().take(20).collect::<Vec<_>>(),
        }));
    }
    if !graph.suggested_clusters.is_empty() {
        findings.push(json!({
            "finding": "suggested_clusters",
            "clusters": graph.suggested_clusters,
        }));
    }
    for warning in &graph.warnings {
        findings.push(json!({ "finding": "analysis_warning", "detail": warning }));
    }
    findings
}

// ─────────────────────────── inlineModToFile ────────────────────────────

/// `rust.inlineModToFile` - inline `mod foo { ... }` body to a sibling
/// submodule file; outer attrs (`#[cfg(test)]`) stay attached to the
/// retained `mod foo;` declaration. Target path is auto-derived when not
/// given (`parent.rs` + `mod tests` -> `parent/tests.rs`; `lib.rs`/
/// `main.rs`/`mod.rs` -> flat sibling).
pub struct RustInlineModToFile(pub Arc<ProvenanceLedger>);

#[derive(Deserialize)]
struct InlineModInput {
    /// Source file containing the inline `mod foo { ... }`, workspace-relative.
    source: String,
    /// The single mod name to inline.
    #[serde(default, rename = "moduleName", alias = "module_name", alias = "modName")]
    module_name: Option<String>,
    /// Optional explicit target path. Auto-derived from source when unset.
    #[serde(default)]
    target: Option<String>,
    /// Back-compat: item_names[0] accepted as the mod name (v1 shape).
    #[serde(default, rename = "itemNames", alias = "item_names")]
    item_names: Option<Vec<String>>,
    /// Return findings + metadata but zero `changes`/`creates`.
    #[serde(default, rename = "previewOnly", alias = "preview_only")]
    preview_only: Option<bool>,
}

#[async_trait]
impl Tool for RustInlineModToFile {
    fn name(&self) -> &str {
        "rust.inlineModToFile"
    }
    fn description(&self) -> &str {
        "Inline the body of an inline `mod foo { ... }` into a sibling submodule file and replace the block with `mod foo;`. Outer attributes like #[cfg(test)] stay attached. Target auto-derived (parent.rs -> parent/<name>.rs; lib.rs/main.rs/mod.rs -> flat sibling). Refuses non-empty targets. NEVER writes: feed {changes, creates} into edits.merge/createFile."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string", "description": "Source file containing the inline `mod foo { ... }`, workspace-relative." },
                "moduleName": { "type": "string", "description": "The mod name to inline. Accepts modName alias." },
                "target": { "type": "string", "description": "Optional explicit target path. Auto-derived from source when unset." },
                "previewOnly": { "type": "boolean", "description": "Return findings + metadata but zero changes/creates." }
            },
            "required": ["source", "moduleName"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations { read_only: true, destructive: false }
    }
    fn namespace_binding(&self) -> Option<(String, String)> {
        Some(("rust".to_string(), "inlineModToFile".to_string()))
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let root = cx.root.clone();
        let args: InlineModInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("rust.inlineModToFile: bad input: {e}")),
        };
        let preview_only = args.preview_only.unwrap_or(false);
        let mod_name = match args
            .module_name
            .as_deref()
            .map(str::to_string)
            .or_else(|| {
                args.item_names
                    .as_ref()
                    .and_then(|names| names.first().cloned())
            }) {
            Some(name) => name,
            None => {
                return ToolResult::Error(
                    "rust.inlineModToFile: moduleName is required".to_string(),
                );
            }
        };
        let source_abs = match resolve_workspace_file(&root, &args.source, "rust.inlineModToFile") {
            Ok(path) => path,
            Err(e) => return ToolResult::Error(format!("rust.inlineModToFile: {e}")),
        };
        let target_abs = match args.target.as_deref() {
            Some(target) => match resolve_workspace_file(&root, target, "rust.inlineModToFile") {
                Ok(path) => path,
                Err(e) => return ToolResult::Error(format!("rust.inlineModToFile: {e}")),
            },
            None => source_abs.clone(), // planner derives when target==source marker; but v1 needs None to derive
        };
        // To get auto-derivation we must pass target=None to the planner when
        // the caller omitted it. Reconstruct: if args.target was None, pass None.
        let target_param: Option<String> = if args.target.is_some() {
            Some(target_abs.to_string_lossy().into_owned())
        } else {
            None
        };

        let params = RefactorPlanParams {
            kind: "inline_mod_to_file_submodule".to_string(),
            source: source_abs.to_string_lossy().into_owned(),
            target: target_param,
            item_names: Some(vec![mod_name]),
            project_dir: Some(root.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let plan_json = match bbox_refactor::plan(&params) {
            Ok(json) => json,
            Err(e) => {
                let msg = e.to_string();
                return ToolResult::Error(format!(
                    "rust.inlineModToFile: {msg}{}",
                    done_hint(&msg)
                ));
            }
        };
        let plan: bbox_refactor::RefactorPlan = match serde_json::from_str(&plan_json) {
            Ok(plan) => plan,
            Err(e) => return ToolResult::Error(format!("rust.inlineModToFile: plan decode: {e}")),
        };
        let PlanProjection {
            changes,
            creates,
            would_change_files,
            would_create_files,
        } = match plan_to_changes_creates(&root, "rust.inlineModToFile", &plan.edits, preview_only) {
            Ok(proj) => proj,
            Err(e) => return ToolResult::Error(format!("rust.inlineModToFile: {e}")),
        };
        if !preview_only {
            super::helpers::record_in_ledger(&self.0, "rust.inlineModToFile", &changes);
        }
        // Target path is in the plan edits; surface it for the cell.
        let target_rel = plan
            .edits
            .get(1)
            .and_then(|edit| relativize(&root, &edit.path).ok());
        let mut findings: Vec<Value> = Vec::new();
        for note in &plan.leftovers {
            findings.push(json!({ "finding": "note", "detail": note }));
        }
        ToolResult::Json(json!({
            "title": plan.title,
            "changes": changes,
            "creates": creates,
            "findings": findings,
            "preview_only": preview_only,
            "target": target_rel,
            "would_change_files": would_change_files,
            "would_create_files": would_create_files,
            "provenance": "syntax_only",
        }))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::bindings::code_facts::Span;

    fn ledger() -> Arc<ProvenanceLedger> {
        Arc::new(ProvenanceLedger::default())
    }

    fn cx_in(dir: &std::path::Path) -> ToolCx {
        ToolCx {
            root: dir.to_path_buf(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(std::sync::Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
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

    fn apply_changes(source: &str, result: &Value) -> String {
        let mut text_edits: Vec<bbox_refactor::TextEdit> = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| bbox_refactor::TextEdit {
                byte_start: change["span"]["byte_start"].as_u64().unwrap() as usize,
                byte_end: change["span"]["byte_end"].as_u64().unwrap() as usize,
                replacement: change["new_text"].as_str().unwrap().to_string(),
            })
            .collect();
        text_edits.sort_by_key(|edit| std::cmp::Reverse(edit.byte_start));
        let mut out = source.to_string();
        for edit in &text_edits {
            out.replace_range(edit.byte_start..edit.byte_end, &edit.replacement);
        }
        out
    }

    // extractItems: compound mode moves struct + fn, bumps visibility on
    // items and fields, wires mod + use decls, and converts the new target
    // file to a create.
    #[tokio::test]
    async fn extract_items_compound_moves_struct_and_fn_with_visibility_bump() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor")).unwrap();
        let src = "pub fn outer() -> Hidden {\n    let _ = helper();\n    Hidden { name: String::new(), kind: 0 }\n}\n\nstruct Hidden {\n    name: String,\n    kind: u32,\n}\n\nfn helper() -> usize { 42 }\n";
        std::fs::write(root.join("src/refactor/parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustExtractItems(ledger())
                .call(
                    json!({
                        "source": "src/refactor/parent.rs",
                        "target": "src/refactor/parent/child.rs",
                        "itemNames": ["Hidden", "helper"]
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["mode"], "compound", "{result}");
        assert_eq!(result["provenance"], "syntax_only", "{result}");

        // Target file lands as a create.
        let creates = result["creates"].as_array().unwrap();
        assert_eq!(creates.len(), 1, "{result}");
        assert_eq!(creates[0]["path"], "src/refactor/parent/child.rs");
        let target_content = creates[0]["content"].as_str().unwrap();
        assert!(target_content.contains("use super::*;"), "{target_content}");
        assert!(
            target_content.contains("pub(super) struct Hidden"),
            "struct visibility: {target_content}"
        );
        assert!(
            target_content.contains("pub(super) name: String"),
            "field visibility: {target_content}"
        );
        assert!(
            target_content.contains("pub(super) fn helper"),
            "fn visibility: {target_content}"
        );

        // Source changes: mod decl + use decl + deletions.
        let source_after = apply_changes(src, &result);
        assert!(source_after.contains("mod child;"), "{source_after}");
        assert!(
            source_after.contains("use child::{Hidden, helper};")
                || source_after.contains("use child::{helper, Hidden};"),
            "use decl: {source_after}"
        );
        assert!(!source_after.contains("struct Hidden"), "{source_after}");
        assert!(!source_after.contains("fn helper"), "{source_after}");

        // Findings carry the moved_item entries (planner's SyntaxItem list).
        let findings = result["findings"].as_array().unwrap();
        assert!(
            findings.iter().any(|finding| finding["finding"] == "moved_item"),
            "{findings:?}"
        );
    }

    // extractItems: target-exists refusal is the DONE signal (not a retry).
    #[tokio::test]
    async fn extract_items_target_exists_is_done_signal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor/parent")).unwrap();
        std::fs::write(root.join("src/refactor/parent.rs"), "fn x() {}\n").unwrap();
        std::fs::write(
            root.join("src/refactor/parent/child.rs"),
            "// pre-existing\nfn keep() {}\n",
        )
        .unwrap();
        let cx = cx_in(&root);
        match RustExtractItems(ledger())
            .call(
                json!({
                    "source": "src/refactor/parent.rs",
                    "target": "src/refactor/parent/child.rs",
                    "itemNames": ["x"]
                }),
                &cx,
            )
            .await
        {
            ToolResult::Error(message) => {
                let message = message.to_string();
                assert!(
                    message.contains("already exists and is non-empty"),
                    "expected target-exists refusal, got: {message}"
                );
                assert!(
                    message.contains("the work is DONE"),
                    "expected DONE-signal hint, got: {message}"
                );
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    // extractItems: mergeIntoExistingTarget appends instead of refusing.
    #[tokio::test]
    async fn extract_items_merge_into_existing_target_appends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor/parent")).unwrap();
        std::fs::write(
            root.join("src/refactor/parent.rs"),
            "fn caller() { added(); }\nfn added() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/refactor/parent/child.rs"),
            "use super::*;\n\npub(super) fn already_here() {}\n",
        )
        .unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustExtractItems(ledger())
                .call(
                    json!({
                        "source": "src/refactor/parent.rs",
                        "target": "src/refactor/parent/child.rs",
                        "itemNames": ["added"],
                        "mergeIntoExistingTarget": true
                    }),
                    &cx,
                )
                .await,
        );
        let creates = result["creates"].as_array().unwrap();
        // merge_into_existing_target: target is NOT a new file; its edits land
        // as changes against the existing content hash, not as a create.
        assert!(creates.is_empty(), "{result}");
        // The appended content lands in a change against the existing file.
        let target_changes: Vec<&Value> = result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|change| change["span"]["file"] == "src/refactor/parent/child.rs")
            .collect();
        assert!(!target_changes.is_empty(), "{result}");
        let appended = target_changes[0]["new_text"].as_str().unwrap();
        assert!(appended.contains("pub(super) fn added"), "{appended}");
    }

    // extractItems: withLocaldeps surfaces the closure in findings.
    #[tokio::test]
    async fn extract_items_with_local_deps_reports_closure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor/parent")).unwrap();
        // helper_one calls helper_two (exclusive dep); helper_two has no
        // external refs. Closure of {helper_one} should add helper_two.
        std::fs::write(
            root.join("src/refactor/parent.rs"),
            "fn entry() { helper_one(); }\nfn helper_one() { helper_two(); }\nfn helper_two() -> u32 { 7 }\n",
        )
        .unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustExtractItems(ledger())
                .call(
                    json!({
                        "source": "src/refactor/parent.rs",
                        "target": "src/refactor/parent/child.rs",
                        "itemNames": ["helper_one"],
                        "withLocalDeps": true
                    }),
                    &cx,
                )
                .await,
        );
        assert_eq!(result["mode"], "with_local_deps", "{result}");
        let findings = result["findings"].as_array().unwrap();
        let closure = findings
            .iter()
            .find(|finding| finding["finding"] == "local_dependency_closure");
        let closure = closure.expect("local_dependency_closure finding");
        let added = closure["added"].as_array().unwrap();
        assert!(
            added.iter().any(|name| name == "helper_two"),
            "closure should add helper_two: {closure}"
        );
    }

    // inlineModToFile: body extracted, outer attr preserved, de-indented.
    #[tokio::test]
    async fn inline_mod_to_file_extracts_body_and_keeps_outer_attr() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor")).unwrap();
        let src = "pub fn outer() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn one() { outer(); }\n}\n";
        std::fs::write(root.join("src/refactor/java.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustInlineModToFile(ledger())
                .call(
                    json!({
                        "source": "src/refactor/java.rs",
                        "moduleName": "tests"
                    }),
                    &cx,
                )
                .await,
        );
        // Auto-derived target: src/refactor/java/tests.rs
        assert_eq!(result["target"], "src/refactor/java/tests.rs", "{result}");
        let creates = result["creates"].as_array().unwrap();
        assert_eq!(creates.len(), 1, "{result}");
        let target = creates[0]["content"].as_str().unwrap();
        assert!(target.contains("use super::*;"), "{target}");
        assert!(target.contains("fn one()"), "{target}");
        // De-indented: `use super::*;` at column 0, not column 4.
        assert!(target.lines().any(|line| line == "use super::*;"), "{target}");
        // Source: inline block replaced with `;`, outer attr kept.
        let source_after = apply_changes(src, &result);
        assert!(
            source_after.contains("#[cfg(test)]\nmod tests;"),
            "outer attr must stay attached: {source_after}"
        );
        assert!(!source_after.contains("mod tests {"), "{source_after}");
    }

    // inlineModToFile: refuses a non-empty pre-existing target.
    #[tokio::test]
    async fn inline_mod_to_file_refuses_non_empty_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("source.rs"), "mod foo {\n    fn x() {}\n}\n").unwrap();
        std::fs::write(
            root.join("custom_target.rs"),
            "// pre-existing\nfn keep() {}\n",
        )
        .unwrap();
        let cx = cx_in(&root);
        match RustInlineModToFile(ledger())
            .call(
                json!({
                    "source": "source.rs",
                    "moduleName": "foo",
                    "target": "custom_target.rs"
                }),
                &cx,
            )
            .await
        {
            ToolResult::Error(message) => {
                let message = message.to_string();
                assert!(
                    message.contains("already exists and is non-empty"),
                    "expected non-empty refusal, got: {message}"
                );
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    // Spans are hash-anchored at the source content sha.
    #[tokio::test]
    async fn extract_items_span_is_hash_anchored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/refactor/parent")).unwrap();
        let src = "fn caller() { x(); }\nfn x() {}\n";
        std::fs::write(root.join("src/refactor/parent.rs"), src).unwrap();
        let cx = cx_in(&root);
        let result = json_of(
            RustExtractItems(ledger())
                .call(
                    json!({
                        "source": "src/refactor/parent.rs",
                        "target": "src/refactor/parent/child.rs",
                        "itemNames": ["x"]
                    }),
                    &cx,
                )
                .await,
        );
        let source_sha = bbox_refactor::sha256_hex(src.as_bytes());
        for change in result["changes"].as_array().unwrap() {
            if change["span"]["file"] == "src/refactor/parent.rs" {
                assert_eq!(change["span"]["content_sha256"], source_sha, "{change}");
            }
        }
        // Spot-check the Span type round-trips through the ledger.
        let first = &result["changes"][0];
        let span: Span = serde_json::from_value(first["span"].clone()).unwrap();
        assert!(!span.file.is_empty());
    }
}
