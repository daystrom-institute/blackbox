//! EX-G8 `elixir_public_api_guard`.
//!
//! Inventory `def`s reachable from a marked facade module (or the
//! non-`@moduledoc false` modules under a source directory) and report
//! the delta against a proposed plan. Analysis-only.
//!
//! Inputs:
//!  - `source` (file OR directory)
//!  - `toml_entries.facade_modules` (optional list of module-name strings
//!    treated as the public boundary; defaults to "everything not
//!    @moduledoc false")
//!  - `toml_entries.proposed_changes` (optional list of structured
//!    descriptions of edits being considered; for v1 we accept a list of
//!    `{"module": "Foo.Bar", "name": "fn_name", "arity": 2, "action":
//!    "remove" | "rename" | "add"}` items)
//!
//! Output is advisory per EX-V5: this kind reports, doesn't decide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::{
    call_target_name, def_name_and_arity, defmodule_body_statements, parse_elixir,
    top_level_defmodule,
};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, ValidationStep,
    resolve_path,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    public_items_touched: BTreeMap<String, Vec<String>>,
    public_api_delta_summary: DeltaSummary,
    facade_re_exports_affected: BTreeSet<String>,
    advisory_severity: String,
}

#[derive(Debug, Serialize, Default)]
struct DeltaSummary {
    added: Vec<String>,
    removed: Vec<String>,
    renamed: Vec<String>,
}

pub(crate) fn plan_public_api_guard(p: &RefactorPlanParams) -> Result<String> {
    let source_root = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let facade_modules: BTreeSet<String> = match p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("facade_modules"))
    {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => BTreeSet::new(),
    };
    let proposed_changes: Vec<ProposedChange> = match p
        .toml_entries
        .as_ref()
        .and_then(|m| m.get("proposed_changes"))
    {
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().filter_map(ProposedChange::from_json).collect()
        }
        _ => Vec::new(),
    };

    // Walk all .ex files.
    let files = super::module_deps::collect_elixir_files_pub(&source_root, false)?;

    let mut public_items: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    let mut moduledoc_false: BTreeSet<String> = BTreeSet::new();

    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(tree) = parse_elixir(&src) else {
            continue;
        };
        let Some(defmod) = top_level_defmodule(&tree, &src) else {
            continue;
        };
        let Some(module_name) = super::module_deps::defmodule_full_name_pub(defmod, &src) else {
            continue;
        };
        let body = defmodule_body_statements(defmod, &src);
        if has_moduledoc_false(&body, &src) {
            moduledoc_false.insert(module_name.clone());
            continue;
        }
        // If facade_modules is non-empty, restrict to those.
        if !facade_modules.is_empty() && !facade_modules.contains(&module_name) {
            continue;
        }
        let mut items: Vec<(String, usize)> = Vec::new();
        for stmt in &body {
            if call_target_name(*stmt, &src) == Some("def") {
                if let Some((name, arity)) = def_name_and_arity(*stmt, &src) {
                    items.push((name, arity));
                }
            }
            // defdelegate counts too (it's how facades expose surface).
            if call_target_name(*stmt, &src) == Some("defdelegate") {
                if let Some((name, arity)) = parse_defdelegate_sig(*stmt, &src) {
                    items.push((name, arity));
                }
            }
        }
        public_items.insert(module_name, items);
    }

    let public_items_touched: BTreeMap<String, Vec<String>> = public_items
        .iter()
        .map(|(m, items)| {
            (
                m.clone(),
                items.iter().map(|(n, a)| format!("{n}/{a}")).collect(),
            )
        })
        .collect();

    // ── compute delta from proposed_changes ──────────────────────────────────
    let mut summary = DeltaSummary::default();
    let mut facade_affected: BTreeSet<String> = BTreeSet::new();
    for change in &proposed_changes {
        let key = format!("{}.{}/{}", change.module, change.name, change.arity);
        match change.action.as_str() {
            "remove" => {
                summary.removed.push(key.clone());
                if facade_modules.contains(&change.module) {
                    facade_affected.insert(change.module.clone());
                }
            }
            "rename" => {
                summary.renamed.push(format!(
                    "{} → {}",
                    key,
                    change.rename_to.as_deref().unwrap_or("???")
                ));
                if facade_modules.contains(&change.module) {
                    facade_affected.insert(change.module.clone());
                }
            }
            "add" => summary.added.push(key),
            _ => {}
        }
    }
    let severity = if !summary.removed.is_empty() || !summary.renamed.is_empty() {
        "high"
    } else if !summary.added.is_empty() {
        "low"
    } else {
        "none"
    }
    .to_string();

    let plan = RefactorPlan {
        title: format!("elixir_public_api_guard: {}", source_root.display()),
        kind: "elixir_public_api_guard".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        edits: Vec::<FileEdit>::new(),
        validations: Vec::<ValidationStep>::new(),
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
    let wrapped = PlanWithReport {
        plan,
        public_items_touched,
        public_api_delta_summary: summary,
        facade_re_exports_affected: facade_affected,
        advisory_severity: severity,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

#[derive(Debug, Clone)]
struct ProposedChange {
    module: String,
    name: String,
    arity: usize,
    action: String,
    rename_to: Option<String>,
}

impl ProposedChange {
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        let obj = v.as_object()?;
        Some(Self {
            module: obj.get("module")?.as_str()?.to_string(),
            name: obj.get("name")?.as_str()?.to_string(),
            arity: obj.get("arity")?.as_u64()? as usize,
            action: obj.get("action")?.as_str()?.to_string(),
            rename_to: obj
                .get("rename_to")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

fn has_moduledoc_false(body: &[tree_sitter::Node<'_>], source: &str) -> bool {
    body.iter().any(|stmt| {
        if stmt.kind() != "unary_operator" {
            return false;
        }
        let mut c = stmt.walk();
        let Some(inner) = stmt.named_children(&mut c).next() else {
            return false;
        };
        if call_target_name(inner, source) != Some("moduledoc") {
            return false;
        }
        let text = &source[stmt.byte_range()];
        text.contains("false")
    })
}

fn parse_defdelegate_sig(call: tree_sitter::Node<'_>, source: &str) -> Option<(String, usize)> {
    // Reuse same parsing as facade.rs by walking signature.
    let args = super::call_arguments(call)?;
    let mut c = args.walk();
    let sig = args.named_children(&mut c).next()?;
    Some(
        super::extract_module::def_name_and_arity_public(call, source).unwrap_or_else(|| {
            let text = &source[sig.byte_range()];
            crude_sig_parse(text)
        }),
    )
}

fn crude_sig_parse(text: &str) -> (String, usize) {
    if let Some(paren) = text.find('(') {
        let name = text[..paren].trim().to_string();
        let inside = &text[paren + 1..];
        let close = inside.rfind(')').unwrap_or(inside.len());
        let args = &inside[..close];
        if args.trim().is_empty() {
            return (name, 0);
        }
        let arity = args.split(',').filter(|s| !s.trim().is_empty()).count();
        (name, arity)
    } else {
        (text.trim().to_string(), 0)
    }
}

// ---------------------------------------------------------------------------
// Borrowed file walker
// ---------------------------------------------------------------------------
//
// Reuses `module_deps::collect_elixir_files_pub` which we expose here.

#[allow(dead_code)]
fn _unused_path_param(_: &Path) {}
