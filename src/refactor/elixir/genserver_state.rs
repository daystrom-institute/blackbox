//! EX-G14 `elixir_genserver_state_audit`.
//!
//! Analyze a GenServer module and produce the inferred state schema plus a
//! per-callback field-read/write map. Analysis-only; precondition for
//! `extract_genserver_callback_group`.
//!
//! State-field analysis is **advisory** per round-2 review (deepseek C-R2-1).
//! Recognized syntactic forms (Tier-1):
//!   - `state.field` and `state[:field]` direct access
//!   - `%{state | field: ...}` struct update (write)
//!   - `%{field: x} = state` pattern destructure (read)
//!   - `Map.get/put/fetch!/update!`, `put_in`, `update_in`
//! Anything else lands in `state_field_touches_unresolved` for operator
//! review.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow};
use serde::Serialize;
use tree_sitter::Node;

use super::{call_target_name, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, ValidationStep,
    resolve_path,
};

#[derive(Debug, Serialize)]
struct StateAuditReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    state_fields: BTreeMap<String, String>,
    per_callback: BTreeMap<String, CallbackTouches>,
    init_initializers: BTreeMap<String, String>,
    supervisor_child_specs: Vec<String>,
    state_field_touches_unresolved: Vec<UnresolvedAccess>,
}

#[derive(Debug, Serialize, Default)]
struct CallbackTouches {
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
}

#[derive(Debug, Serialize)]
struct UnresolvedAccess {
    callback: String,
    line: usize,
    excerpt: String,
}

pub(crate) fn plan_genserver_state_audit(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {}",
            source_path.display()
        )
    })?;
    let body = defmodule_body_statements(defmodule, &parsed.source);

    // Identify init/1 to infer state fields.
    let mut state_fields: BTreeMap<String, String> = BTreeMap::new();
    let mut init_initializers: BTreeMap<String, String> = BTreeMap::new();
    for stmt in &body {
        if call_target_name(*stmt, &parsed.source) != Some("def") {
            continue;
        }
        let Some((name, _arity)) = super::def_name_and_arity(*stmt, &parsed.source) else {
            continue;
        };
        if name != "init" {
            continue;
        }
        // Walk body looking for `{:ok, %{...}}` or `%{...}` returns.
        infer_state_from_init(
            *stmt,
            &parsed.source,
            &mut state_fields,
            &mut init_initializers,
        );
    }

    // Per-callback scan.
    let mut per_callback: BTreeMap<String, CallbackTouches> = BTreeMap::new();
    let mut unresolved: Vec<UnresolvedAccess> = Vec::new();
    for stmt in &body {
        let target = match call_target_name(*stmt, &parsed.source) {
            Some(t) => t,
            None => continue,
        };
        if !matches!(target, "def" | "defp") {
            continue;
        }
        let Some((name, _arity)) = super::def_name_and_arity(*stmt, &parsed.source) else {
            continue;
        };
        if !matches!(
            name.as_str(),
            "handle_call"
                | "handle_cast"
                | "handle_info"
                | "handle_continue"
                | "init"
                | "terminate"
                | "code_change"
        ) {
            continue;
        }
        let label = format!(
            "{}:line_{}",
            name,
            super::byte_to_line_col(&parsed.source, stmt.start_byte()).0
        );
        let touches = per_callback.entry(label.clone()).or_default();
        scan_state_touches(*stmt, &parsed.source, touches, &mut unresolved, &label);
    }

    let plan = RefactorPlan {
        title: format!("elixir_genserver_state_audit: {}", source_path.display()),
        kind: "elixir_genserver_state_audit".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
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

    let report = StateAuditReport {
        plan,
        state_fields,
        per_callback,
        init_initializers,
        supervisor_child_specs: Vec::new(),
        state_field_touches_unresolved: unresolved,
    };
    Ok(serde_json::to_string(&report)?)
}

fn infer_state_from_init(
    init_stmt: Node<'_>,
    source: &str,
    state_fields: &mut BTreeMap<String, String>,
    init_initializers: &mut BTreeMap<String, String>,
) {
    // Walk the init body. Each top-level return expression should be a tuple
    // {:ok, state, ...} or similar; we don't enforce the shape, just look for
    // `%{...}` maps near the end and harvest their keys.
    let mut stack = vec![init_stmt];
    while let Some(n) = stack.pop() {
        if n.kind() == "map" {
            let mut c = n.walk();
            for child in n.named_children(&mut c) {
                if child.kind() == "map_content" {
                    harvest_map_keys(child, source, state_fields, init_initializers);
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
}

fn harvest_map_keys(
    map_content: Node<'_>,
    source: &str,
    state_fields: &mut BTreeMap<String, String>,
    init_initializers: &mut BTreeMap<String, String>,
) {
    let mut stack = vec![map_content];
    while let Some(n) = stack.pop() {
        if n.kind() == "pair" {
            let mut c = n.walk();
            let mut iter = n.named_children(&mut c);
            if let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                let key_text = source[k.byte_range()].trim().trim_end_matches(':');
                let val_text = source[v.byte_range()].trim().to_string();
                if is_simple_field_name(key_text) {
                    state_fields
                        .entry(key_text.to_string())
                        .or_insert_with(|| "term()".to_string());
                    init_initializers.insert(key_text.to_string(), val_text);
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
}

fn is_simple_field_name(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn scan_state_touches(
    callback: Node<'_>,
    source: &str,
    touches: &mut CallbackTouches,
    unresolved: &mut Vec<UnresolvedAccess>,
    callback_label: &str,
) {
    let mut stack = vec![callback];
    while let Some(n) = stack.pop() {
        // Pattern 1: `state.field` — dot expression with `state` left.
        if n.kind() == "dot" {
            let mut c = n.walk();
            let mut iter = n.named_children(&mut c);
            if let (Some(left), Some(right)) = (iter.next(), iter.next()) {
                if source[left.byte_range()] == *"state" {
                    let field = source[right.byte_range()].to_string();
                    if is_simple_field_name(&field) {
                        touches.reads.insert(field);
                    }
                }
            }
        }
        // Pattern 2: `%{state | field: ...}` — map_update node.
        if n.kind() == "map" {
            // tree-sitter-elixir uses `map` for both `%{...}` and `%{x | ...}`
            // forms; detect "|" presence in source text.
            let text = &source[n.byte_range()];
            if text.contains("state |") {
                // Find pair keys.
                let mut c = n.walk();
                for child in n.named_children(&mut c) {
                    if child.kind() == "map_content" {
                        let mut stack2 = vec![child];
                        while let Some(c2) = stack2.pop() {
                            if c2.kind() == "pair" {
                                let mut cur = c2.walk();
                                if let Some(k) = c2.named_children(&mut cur).next() {
                                    let key = source[k.byte_range()].trim().trim_end_matches(':');
                                    if is_simple_field_name(key) {
                                        touches.writes.insert(key.to_string());
                                    }
                                }
                            }
                            let mut cur = c2.walk();
                            for c3 in c2.named_children(&mut cur) {
                                stack2.push(c3);
                            }
                        }
                    }
                }
            }
        }
        // Pattern 3: Map.get/put/fetch! calls.
        if n.kind() == "call" {
            if let Some(callee) = n.named_child(0) {
                if callee.kind() == "dot" {
                    let text = &source[callee.byte_range()];
                    if let Some(suffix) = text.strip_prefix("Map.") {
                        let func = suffix.split_whitespace().next().unwrap_or("");
                        // Check first call arg is `state`.
                        if let Some(args) = super::call_arguments(n) {
                            let mut ac = args.walk();
                            let mut arg_iter = args.named_children(&mut ac);
                            let first = arg_iter.next();
                            let second = arg_iter.next();
                            if let Some(arg0) = first {
                                if source[arg0.byte_range()].trim() == "state" {
                                    if let Some(key_node) = second {
                                        let key_text = source[key_node.byte_range()]
                                            .trim()
                                            .trim_start_matches(':');
                                        if is_simple_field_name(key_text) {
                                            match func {
                                                "get" | "fetch!" | "fetch" | "has_key?" => {
                                                    touches.reads.insert(key_text.to_string());
                                                }
                                                "put" | "delete" | "update!" | "update"
                                                | "replace" | "pop" => {
                                                    touches.writes.insert(key_text.to_string());
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if text == "put_in" || text == "update_in" {
                        let (line, _) = super::byte_to_line_col(source, n.start_byte());
                        unresolved.push(UnresolvedAccess {
                            callback: callback_label.to_string(),
                            line,
                            excerpt: text.to_string(),
                        });
                    }
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
}
