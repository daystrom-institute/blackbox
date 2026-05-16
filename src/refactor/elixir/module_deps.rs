//! EX-G7 `elixir_module_dependency_analysis`.
//!
//! Build an inter-module call graph for a target file or directory.
//! Analysis-only: emits no FileEdits. Used as a precondition to
//! `elixir-shatter-dispatch-table` and `elixir-split-genserver` decisions.
//!
//! v1 strategy: in-process via tree-sitter (no `mix xref` shell-out).
//!   - Walk every `*.ex` (and optionally `*.exs`) under `source` (file or dir).
//!   - For each file: extract the top-level `defmodule X.Y.Z` name; count
//!     lines and public defs; record `alias` directives.
//!   - For each file: walk the body, collect `Module.fn(args)` calls. The
//!     literal `Module` path is used as the call target (alias resolution is
//!     v2; an aliased `Foo` referring to `App.Foo` is recorded as `Foo` and
//!     the operator reconciles).
//!   - Edges aggregate per (from_module, to_module).
//!   - Simple cycle detection via DFS.
//!
//! Output is the JSON shape declared in
//! `design/refactor-tools/elixir/refactor-elixir-expansion.md` (EX-G7).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use super::{call_target_name, defmodule_body_statements, parse_elixir, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, ValidationStep,
    resolve_path,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    nodes: Vec<NodeReport>,
    edges: Vec<EdgeReport>,
    cycles: Vec<Vec<String>>,
    fan_in_max: BTreeMap<String, usize>,
    compile_time_edges: Vec<EdgeReport>,
}

#[derive(Debug, Serialize)]
struct NodeReport {
    module: String,
    file: String,
    loc: usize,
    publics: usize,
}

#[derive(Debug, Serialize)]
struct EdgeReport {
    from: String,
    to: String,
    kind: String, // "runtime" | "compile_time"
    call_count: usize,
}

pub(crate) fn plan_module_dependency_analysis(p: &RefactorPlanParams) -> Result<String> {
    let source_root = resolve_path(p.project_dir.as_deref(), &p.source)?;
    if !source_root.exists() {
        bail!(
            "error.bad_input(code=source_not_found): {}",
            source_root.display()
        );
    }
    let include_exs = matches!(
        p.toml_entries.as_ref().and_then(|e| e.get("include_exs")),
        Some(serde_json::Value::Bool(true))
    );

    // ── walk files ───────────────────────────────────────────────────────────
    let files = collect_elixir_files(&source_root, include_exs)?;

    // ── per-file analysis ───────────────────────────────────────────────────
    let mut nodes: Vec<NodeReport> = Vec::new();
    let mut edges: HashMap<(String, String, EdgeKind), usize> = HashMap::new();
    let mut module_set: HashSet<String> = HashSet::new();
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
        let module_name = defmodule_full_name(defmod, &src);
        let Some(module_name) = module_name else {
            continue;
        };

        let body = defmodule_body_statements(defmod, &src);
        let loc = src.lines().count();
        let publics = body
            .iter()
            .filter(|n| call_target_name(**n, &src) == Some("def"))
            .count();
        nodes.push(NodeReport {
            module: module_name.clone(),
            file: file.to_string_lossy().into_owned(),
            loc,
            publics,
        });
        module_set.insert(module_name.clone());

        // Compile-time edges: aliases / imports / requires reference modules.
        for stmt in &body {
            let Some(name) = call_target_name(*stmt, &src) else {
                continue;
            };
            if matches!(name, "alias" | "import" | "require") {
                let targets = extract_module_refs_in_call(*stmt, &src);
                for target in targets {
                    *edges
                        .entry((module_name.clone(), target, EdgeKind::CompileTime))
                        .or_default() += 1;
                }
            }
        }

        // Runtime edges: all `Module.fn(args)` calls inside the defmodule body.
        // We collect them via tree walk.
        let mut stack = vec![defmod];
        while let Some(n) = stack.pop() {
            if n.kind() == "call" {
                if let Some(target_module) = call_target_module_path(n, &src) {
                    *edges
                        .entry((module_name.clone(), target_module, EdgeKind::Runtime))
                        .or_default() += 1;
                }
            }
            let mut cursor = n.walk();
            for c in n.named_children(&mut cursor) {
                stack.push(c);
            }
        }
    }

    // ── compile_time_edges report ────────────────────────────────────────────
    let mut compile_time_edges: Vec<EdgeReport> = Vec::new();
    let mut runtime_edges: Vec<EdgeReport> = Vec::new();
    for ((from, to, kind), count) in &edges {
        let er = EdgeReport {
            from: from.clone(),
            to: to.clone(),
            kind: match kind {
                EdgeKind::Runtime => "runtime".to_string(),
                EdgeKind::CompileTime => "compile_time".to_string(),
            },
            call_count: *count,
        };
        match kind {
            EdgeKind::CompileTime => compile_time_edges.push(er),
            EdgeKind::Runtime => runtime_edges.push(er),
        }
    }
    runtime_edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    compile_time_edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));

    // ── fan-in-max ──────────────────────────────────────────────────────────
    let mut fan_in: BTreeMap<String, usize> = BTreeMap::new();
    for ((_, to, _), _) in &edges {
        *fan_in.entry(to.clone()).or_default() += 1;
    }
    // Keep top 20.
    let mut top: Vec<(String, usize)> = fan_in.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let fan_in_max: BTreeMap<String, usize> = top.into_iter().take(20).collect();

    // ── cycle detection (only consider intra-project edges) ─────────────────
    let mut adj: HashMap<String, BTreeSet<String>> = HashMap::new();
    for ((from, to, _), _) in &edges {
        if module_set.contains(from) && module_set.contains(to) && from != to {
            adj.entry(from.clone()).or_default().insert(to.clone());
        }
    }
    let cycles = find_cycles(&adj);

    nodes.sort_by(|a, b| a.module.cmp(&b.module));

    let plan = RefactorPlan {
        title: format!("elixir_module_dependency_analysis: {}", source_root.display()),
        kind: "elixir_module_dependency_analysis".to_string(),
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
        nodes,
        edges: runtime_edges,
        cycles,
        fan_in_max,
        compile_time_edges,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum EdgeKind {
    Runtime,
    CompileTime,
}

// ---------------------------------------------------------------------------
// File walking
// ---------------------------------------------------------------------------

fn collect_elixir_files(root: &Path, include_exs: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if root.is_file() {
        let ok = is_elixir_file(root, include_exs);
        if ok {
            out.push(root.to_path_buf());
        }
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip _build, deps, .git, .claude
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if matches!(name, "_build" | "deps" | ".git" | ".claude" | "node_modules") {
                    continue;
                }
                stack.push(path);
            } else if is_elixir_file(&path, include_exs) {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn is_elixir_file(path: &Path, include_exs: bool) -> bool {
    let ext = path.extension().and_then(|s| s.to_str());
    matches!(ext, Some("ex")) || (include_exs && matches!(ext, Some("exs")))
}

// ---------------------------------------------------------------------------
// Module-ref extraction
// ---------------------------------------------------------------------------

/// Extract the dotted module name from a `defmodule X.Y.Z do ... end` call.
fn defmodule_full_name(defmod_call: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let args = super::call_arguments(defmod_call)?;
    let mut cursor = args.walk();
    let alias = args.named_children(&mut cursor).next()?;
    if alias.kind() != "alias" {
        return None;
    }
    Some(source[alias.byte_range()].to_string())
}

/// For `alias Foo.{A, B}` extract the module names `Foo.A`, `Foo.B`.
/// For `alias Foo.Bar` extract `Foo.Bar`.
/// For `import Foo` extract `Foo`.
fn extract_module_refs_in_call(call: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let Some(args) = super::call_arguments(call) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![args];
    while let Some(n) = stack.pop() {
        if n.kind() == "alias" {
            out.push(source[n.byte_range()].to_string());
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            stack.push(c);
        }
    }
    out
}

/// For `Foo.Bar.baz(args)` extract the module path `Foo.Bar` (left of the
/// final dot before the lowercase function name). Returns None for non-cross-
/// module calls.
fn call_target_module_path(call: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    // The grammar: `Foo.Bar.baz(args)` is a `call` whose first named child is
    // a `dot { alias Foo.Bar, identifier baz }`. Bare local calls (`baz(x)`)
    // have an identifier as first child without a dot.
    let target = call.named_child(0)?;
    if target.kind() != "dot" {
        return None;
    }
    // dot has at least two named children: left (module path) + right (id).
    let mut cursor = target.walk();
    let left = target.named_children(&mut cursor).next()?;
    if left.kind() != "alias" {
        return None;
    }
    Some(source[left.byte_range()].to_string())
}

// ---------------------------------------------------------------------------
// Cycle detection
// ---------------------------------------------------------------------------

fn find_cycles(adj: &HashMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();

    // Iterate roots in stable order.
    let mut roots: Vec<&String> = adj.keys().collect();
    roots.sort();
    for root in roots {
        if visited.contains(root) {
            continue;
        }
        dfs(root, adj, &mut visited, &mut stack, &mut on_stack, &mut cycles);
    }
    // Deduplicate by normalized cycle (rotate to canonical start).
    let mut seen: HashSet<Vec<String>> = HashSet::new();
    let mut unique = Vec::new();
    for c in cycles {
        let canon = canonical_rotation(c);
        if seen.insert(canon.clone()) {
            unique.push(canon);
        }
    }
    unique
}

fn dfs(
    node: &str,
    adj: &HashMap<String, BTreeSet<String>>,
    visited: &mut HashSet<String>,
    stack: &mut Vec<String>,
    on_stack: &mut HashSet<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    if on_stack.contains(node) {
        // Found a cycle — slice from where node first appears.
        if let Some(pos) = stack.iter().position(|s| s == node) {
            let cycle: Vec<String> = stack[pos..].iter().cloned().chain(std::iter::once(node.to_string())).collect();
            cycles.push(cycle);
        }
        return;
    }
    if visited.contains(node) {
        return;
    }
    on_stack.insert(node.to_string());
    stack.push(node.to_string());
    if let Some(neighbors) = adj.get(node) {
        for next in neighbors {
            dfs(next, adj, visited, stack, on_stack, cycles);
        }
    }
    stack.pop();
    on_stack.remove(node);
    visited.insert(node.to_string());
}

fn canonical_rotation(mut cycle: Vec<String>) -> Vec<String> {
    if cycle.is_empty() {
        return cycle;
    }
    // Drop trailing repeated node if equal to first.
    if cycle.len() > 1 && cycle[0] == cycle[cycle.len() - 1] {
        cycle.pop();
    }
    // Rotate so smallest member is first.
    if let Some(min_idx) = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, s)| (*s).clone())
        .map(|(i, _)| i)
    {
        cycle.rotate_left(min_idx);
    }
    cycle
}
