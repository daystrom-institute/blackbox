//! EX-G2 `extract_elixir_module`.
//!
//! Move named `def` / `defp` / `defmacro` items from a source module to a new
//! module file. The new file is created with a `defmodule <target_module_name>
//! do ... end` wrapper holding the moved items plus their attached attributes
//! (`@doc`, `@spec`, `@impl`, `@dialyzer`, `@deprecated`, `@tag`, `@since`).
//!
//! v1 scope:
//!  - Target file MUST NOT exist (no merge mode yet).
//!  - Moved items leave their callsite in the source module unchanged; an
//!    `external_calls` field in the deep-analysis report lists in-module call
//!    sites that may need rewriting (operator follow-up; v2 atom
//!    `update_elixir_callers` will mechanize this).
//!  - Defs with the same name are moved together (all arities of `hello`
//!    move as one unit when `item_names` contains `"hello"`).
//!  - Module attributes (top-level `@my_const`) are NOT moved unless
//!    explicitly named.
//!  - Aliases/imports/requires are NOT auto-copied; the operator passes
//!    them in `target_prelude` (Java-style) or relies on a follow-up
//!    `elixir_organize_aliases` on the target.
//!
//! Refusals:
//!  - `error.bad_input(code=no_defmodule)` — source has no top-level defmodule.
//!  - `error.bad_input(code=target_exists)` — target file already exists.
//!  - `error.bad_input(code=item_not_found)` — a requested item name has no
//!    matching def in source.
//!  - `error.bad_input(code=use_at_scope)` — source has a non-default `use Foo`
//!    at module scope (anything other than `use GenServer` is "non-default"
//!    in v1; full project-vs-stdlib detection is v2). Refused unless
//!    `acknowledge_use_at_scope: true`.
//!  - `error.bad_input(code=quote_in_moved)` — moved item body contains a
//!    `quote do ... end` block. Refused unless `acknowledge_quote_in_moved`.
//!  - `error.bad_input(code=defmacro_move)` — moved item is `defmacro`.
//!    Refused unless `acknowledge_defmacro_move`.

use std::collections::HashSet;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{call_target_name, def_name_and_arity, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool, toml_str_array,
};

/// Attributes recognized as "attached" to the following def. When walking
/// backward from a def, consecutive `@<name>` directives whose `name` is in
/// this list are moved with the def. Other attributes stay on the source.
const ATTACHED_ATTR_NAMES: &[&str] = &[
    "doc",
    "spec",
    "impl",
    "dialyzer",
    "deprecated",
    "tag",
    "since",
    "moduledoc", // would never be attached but listed for completeness
    "typedoc",
    "callback",
    "macrocallback",
];

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    moved_items: Vec<MovedItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    in_module_call_sites: Vec<InModuleCallSite>,
}

#[derive(Debug, Serialize)]
struct MovedItem {
    name: String,
    arity_set: Vec<usize>,
    clause_count: usize,
    attached_attributes: usize,
    /// Was any moved clause a `defmacro`?
    is_macro: bool,
    /// Was any moved clause inside a `quote` block?
    contains_quote: bool,
}

#[derive(Debug, Serialize)]
struct InModuleCallSite {
    line: usize,
    column: usize,
    caller: String,
    excerpt: String,
}

pub(crate) fn plan_extract_module(p: &RefactorPlanParams) -> Result<String> {
    // ── inputs ────────────────────────────────────────────────────────────────
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_elixir_module"))
        .and_then(|t| resolve_path(p.project_dir.as_deref(), t))?;
    if target_path.exists() {
        bail!(
            "error.bad_input(code=target_exists): target file {} already exists; merge mode not implemented in v1",
            target_path.display()
        );
    }
    let target_module_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (target defmodule name) is required for extract_elixir_module"))?;

    let item_names: Vec<String> = p.item_names.as_deref().unwrap_or(&[]).to_vec();
    if item_names.is_empty() {
        bail!("item_names is required (list of function names to move)");
    }

    let ack_use = toml_bool(&p.toml_entries, "acknowledge_use_at_scope");
    let ack_quote = toml_bool(&p.toml_entries, "acknowledge_quote_in_moved");
    let ack_macro = toml_bool(&p.toml_entries, "acknowledge_defmacro_move");
    let target_prelude = toml_str_array(&p.toml_entries, "target_prelude");

    // ── parse ─────────────────────────────────────────────────────────────────
    let parsed = parse_elixir_file(&source_path)?;
    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {} has no top-level defmodule",
            source_path.display()
        )
    })?;
    let body_stmts = defmodule_body_statements(defmodule, &parsed.source);

    // ── use_at_scope check ────────────────────────────────────────────────────
    let use_at_scope = body_stmts
        .iter()
        .filter_map(|n| call_target_name(*n, &parsed.source).map(|name| (name, *n)))
        .find(|(name, _)| *name == "use");
    if let Some((_, use_call)) = use_at_scope {
        if !ack_use {
            let (line, _) = byte_to_line_col(&parsed.source, use_call.start_byte());
            let preview = parsed.source[use_call.byte_range()]
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            bail!(
                "error.bad_input(code=use_at_scope): source has `{}` at line {} (use injects callbacks/imports/macros the planner cannot see); pass acknowledge_use_at_scope=true to proceed",
                preview, line
            );
        }
    }

    // ── classify body into items + attached attributes ───────────────────────
    let classified = classify_body(&body_stmts, &parsed.source);

    // ── select items by name ─────────────────────────────────────────────────
    let wanted: HashSet<&str> = item_names.iter().map(String::as_str).collect();
    let mut moved: Vec<MovedItem> = Vec::new();
    let mut selected_indices: Vec<usize> = Vec::new();
    let mut by_name: std::collections::BTreeMap<String, Vec<usize>> = std::collections::BTreeMap::new();
    for (idx, entry) in classified.iter().enumerate() {
        if let ClassifiedKind::Definition { name, .. } = &entry.kind {
            if wanted.contains(name.as_str()) {
                selected_indices.push(idx);
                by_name.entry(name.clone()).or_default().push(idx);
            }
        }
    }

    // Verify every requested name found at least one clause.
    let found_names: HashSet<&str> = by_name.keys().map(String::as_str).collect();
    let missing: Vec<&&str> = wanted.iter().filter(|n| !found_names.contains(*n)).collect();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|n| (**n).to_string()).collect();
        bail!(
            "error.bad_input(code=item_not_found): no def/defp/defmacro found for: {}",
            names.join(", ")
        );
    }

    // ── per-item refusals (macro / quote) ────────────────────────────────────
    for (name, idxs) in &by_name {
        let mut arity_set = std::collections::BTreeSet::new();
        let mut clause_count = 0usize;
        let mut attached_attrs = 0usize;
        let mut is_macro = false;
        let mut contains_quote = false;
        for &i in idxs {
            clause_count += 1;
            attached_attrs += classified[i].attached_attr_idxs.len();
            if let ClassifiedKind::Definition { arity, is_macro: m, contains_quote: q, .. } = &classified[i].kind {
                arity_set.insert(*arity);
                is_macro |= *m;
                contains_quote |= *q;
            }
        }
        if is_macro && !ack_macro {
            let line = classified[*idxs.first().unwrap()].node.start_byte();
            let (line_num, _) = byte_to_line_col(&parsed.source, line);
            bail!(
                "error.bad_input(code=defmacro_move): item `{name}` includes a defmacro clause (line {line_num}); pass acknowledge_defmacro_move=true to proceed"
            );
        }
        if contains_quote && !ack_quote {
            let line = classified[*idxs.first().unwrap()].node.start_byte();
            let (line_num, _) = byte_to_line_col(&parsed.source, line);
            bail!(
                "error.bad_input(code=quote_in_moved): item `{name}` has a clause body containing a quote block (line {line_num}); pass acknowledge_quote_in_moved=true to proceed"
            );
        }
        moved.push(MovedItem {
            name: name.clone(),
            arity_set: arity_set.into_iter().collect(),
            clause_count,
            attached_attributes: attached_attrs,
            is_macro,
            contains_quote,
        });
    }

    // ── compute byte ranges to remove from source ────────────────────────────
    // For each selected def, remove from the start of the FIRST attached
    // attribute (or the def's leading_trivia_start) to the end of the def
    // (including trailing newline). Order ranges in reverse so we can apply
    // edits without index shifting.
    let mut remove_ranges: Vec<(usize, usize)> = Vec::new();
    for &idx in &selected_indices {
        let entry = &classified[idx];
        let start = entry
            .attached_attr_idxs
            .first()
            .map(|i| classified[*i].node.start_byte())
            .unwrap_or_else(|| entry.node.start_byte());
        // include trailing newline + any indent before next stmt by extending
        // to end of line(s)
        let end = trailing_newline_end(&parsed.source, entry.node.end_byte());
        remove_ranges.push((start, end));
    }
    remove_ranges.sort_by_key(|(s, _)| *s);

    // Coalesce overlapping/contiguous ranges.
    let coalesced = coalesce_ranges(&remove_ranges);

    let mut source_edits: Vec<TextEdit> = coalesced
        .iter()
        .map(|(start, end)| TextEdit {
            byte_start: *start,
            byte_end: *end,
            replacement: String::new(),
        })
        .collect();
    source_edits.sort_by_key(|e| e.byte_start);

    // ── build target file content ────────────────────────────────────────────
    let mut target_body_parts: Vec<String> = Vec::new();
    for &idx in &selected_indices {
        let entry = &classified[idx];
        let start = entry
            .attached_attr_idxs
            .first()
            .map(|i| classified[*i].node.start_byte())
            .unwrap_or_else(|| entry.node.start_byte());
        let end = entry.node.end_byte();
        // Re-indent each line by 2 spaces relative to the source's current
        // indentation. The source items are at "  " (2 spaces) inside the
        // defmodule; target items must also be at "  " inside their new
        // defmodule. We can simply preserve source indentation: extract verbatim.
        let chunk = parsed.source[start..end].to_string();
        target_body_parts.push(chunk);
    }
    let prelude_lines: Vec<String> = target_prelude.into_iter().collect();

    let mut target_content = String::new();
    target_content.push_str(&format!("defmodule {} do\n", target_module_name));
    for line in &prelude_lines {
        for sub in line.lines() {
            target_content.push_str("  ");
            target_content.push_str(sub);
            target_content.push('\n');
        }
    }
    if !prelude_lines.is_empty() {
        target_content.push('\n');
    }
    let body_text = target_body_parts.join("\n\n");
    // Re-indent body to two spaces if it's not already.
    let reindented = reindent_to_two_spaces(&body_text);
    target_content.push_str(&reindented);
    if !target_content.ends_with('\n') {
        target_content.push('\n');
    }
    target_content.push_str("end\n");

    let target_edit = TextEdit {
        byte_start: 0,
        byte_end: 0,
        replacement: target_content.clone(),
    };
    let target_file_edit = FileEdit {
        path: target_path.to_string_lossy().into_owned(),
        original_sha256: sha256_hex(b""),
        edits: vec![target_edit],
        new_text: Some(target_content),
    };

    let source_file_edit = FileEdit {
        path: source_path.to_string_lossy().into_owned(),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: source_edits,
        new_text: None,
    };

    // EX-V6 v1 floor: verify the post-edit source and the target file body
    // both parse cleanly.
    super::roundtrip::verify_edits_parse_clean(&parsed.source, &source_file_edit.edits)?;
    if let Some(target_text) = target_file_edit.new_text.as_deref() {
        super::roundtrip::verify_parse_clean(target_text)?;
    }

    // ── in-module call site report (caller rewrite advisory) ─────────────────
    let in_module_call_sites = scan_in_module_call_sites(&parsed.source, defmodule, &by_name);

    let plan = RefactorPlan {
        title: format!(
            "extract_elixir_module: {} → {}",
            source_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            target_module_name
        ),
        kind: "extract_elixir_module".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        edits: vec![source_file_edit, target_file_edit],
        validations: vec![
            ValidationStep::TreeSitterNoErrors {
                path: source_path.to_string_lossy().into_owned(),
                byte_range: None,
            },
            ValidationStep::TreeSitterNoErrors {
                path: target_path.to_string_lossy().into_owned(),
                byte_range: None,
            },
        ],
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
        moved_items: moved,
        in_module_call_sites,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

// ---------------------------------------------------------------------------
// Body classification
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ClassifiedStmt<'tree> {
    node: Node<'tree>,
    kind: ClassifiedKind,
    /// Indices (into the classified-stmt array) of attribute statements that
    /// attach to this definition.
    attached_attr_idxs: Vec<usize>,
}

#[derive(Debug)]
enum ClassifiedKind {
    Definition {
        name: String,
        arity: usize,
        is_macro: bool,
        contains_quote: bool,
    },
    Attribute {
        name: String,
    },
    Other,
}

fn classify_body<'tree>(body: &[Node<'tree>], source: &str) -> Vec<ClassifiedStmt<'tree>> {
    let mut classified = Vec::with_capacity(body.len());
    for &stmt in body {
        let target = call_target_name(stmt, source);
        let kind = match target {
            Some(name) if matches!(name, "def" | "defp" | "defmacro" | "defmacrop") => {
                let is_macro = name.starts_with("defmacro");
                let (fname, arity) = def_name_and_arity(stmt, source).unwrap_or_else(|| {
                    // Fall back to "unknown name/0" — won't match user input, hence safe.
                    (String::from("__unknown__"), 0)
                });
                let contains_quote = stmt_contains_kind(stmt, "call", source, |s| s == "quote");
                ClassifiedKind::Definition {
                    name: fname,
                    arity,
                    is_macro,
                    contains_quote,
                }
            }
            _ if stmt.kind() == "unary_operator" => {
                // Module attribute, e.g. `@doc "..."`.
                let attr_name = unary_attr_name(stmt, source).unwrap_or_default();
                ClassifiedKind::Attribute { name: attr_name }
            }
            _ => ClassifiedKind::Other,
        };
        classified.push(ClassifiedStmt {
            node: stmt,
            kind,
            attached_attr_idxs: Vec::new(),
        });
    }

    // Walk backward from each Definition to find the consecutive run of
    // attached attributes.
    for i in 0..classified.len() {
        if !matches!(classified[i].kind, ClassifiedKind::Definition { .. }) {
            continue;
        }
        let mut attached = Vec::new();
        let mut j = i;
        while j > 0 {
            j -= 1;
            match &classified[j].kind {
                ClassifiedKind::Attribute { name } if is_attached_attribute_name(name) => {
                    attached.push(j);
                }
                _ => break,
            }
        }
        attached.reverse();
        classified[i].attached_attr_idxs = attached;
    }

    classified
}

fn unary_attr_name(node: Node<'_>, source: &str) -> Option<String> {
    // unary_operator wraps a `call` whose target identifier is the attribute name.
    let mut cursor = node.walk();
    let inner = node.named_children(&mut cursor).next()?;
    if inner.kind() != "call" {
        return None;
    }
    call_target_name(inner, source).map(String::from)
}

fn is_attached_attribute_name(name: &str) -> bool {
    ATTACHED_ATTR_NAMES.contains(&name)
}

/// Public-def restricted variant used by `add_elixir_facade_delegations`.
pub(super) fn def_name_and_arity_public(def_call: Node<'_>, source: &str) -> Option<(String, usize)> {
    if call_target_name(def_call, source)? != "def" {
        return None;
    }
    def_name_and_arity(def_call, source)
}

fn stmt_contains_kind(node: Node<'_>, kind: &str, source: &str, target_filter: impl Fn(&str) -> bool) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            if let Some(name) = call_target_name(n, source) {
                if target_filter(name) {
                    return true;
                }
            }
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            stack.push(c);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Caller-scan
// ---------------------------------------------------------------------------

fn scan_in_module_call_sites(
    source: &str,
    defmodule_call: Node<'_>,
    moved: &std::collections::BTreeMap<String, Vec<usize>>,
) -> Vec<InModuleCallSite> {
    let names: HashSet<&str> = moved.keys().map(String::as_str).collect();
    let Some(do_block) = super::call_do_block(defmodule_call) else {
        return Vec::new();
    };
    let mut sites = Vec::new();
    let mut stack = vec![do_block];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" {
            if let Some(name) = call_target_name(n, source) {
                if names.contains(name) {
                    // Skip the call when it's actually the *definition* of the
                    // moved fn (def name(...)). The caller filter is: parent
                    // shouldn't be a `def`-style call.
                    if !is_definition_call(n, source) {
                        let (line, col) = byte_to_line_col(source, n.start_byte());
                        let excerpt: String = source[n.byte_range()]
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        sites.push(InModuleCallSite {
                            line,
                            column: col,
                            caller: name.to_string(),
                            excerpt,
                        });
                    }
                }
            }
        }
        let mut cursor = n.walk();
        for c in n.named_children(&mut cursor) {
            stack.push(c);
        }
    }
    sites
}

fn is_definition_call(call: Node<'_>, source: &str) -> bool {
    // True when `call` is the inner signature call of a `def`/`defp`/etc.
    // i.e., its parent is `arguments` whose parent's target is `def*`.
    let Some(parent) = call.parent() else {
        return false;
    };
    if parent.kind() != "arguments" {
        return false;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    matches!(
        call_target_name(grandparent, source),
        Some("def") | Some("defp") | Some("defmacro") | Some("defmacrop")
    )
}

// ---------------------------------------------------------------------------
// Byte-range helpers
// ---------------------------------------------------------------------------

fn coalesce_ranges(sorted: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &(s, e) in sorted {
        if let Some(last) = out.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        out.push((s, e));
    }
    out
}

fn trailing_newline_end(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut idx = end;
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    if idx < bytes.len() && bytes[idx] == b'\n' {
        idx += 1;
    }
    idx
}

fn reindent_to_two_spaces(body: &str) -> String {
    // Strip leading two spaces from each line if present (source items are
    // indented inside their defmodule by 2 spaces; we want them at 2 spaces
    // inside the new defmodule, so this is a no-op for canonical Elixir code).
    // For now, just return verbatim — re-indentation is conservative.
    body.to_string()
}

fn byte_to_line_col(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte.min(source.len())];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = prefix
        .rfind('\n')
        .map(|n| byte.saturating_sub(n + 1))
        .unwrap_or(byte)
        + 1;
    (line, col)
}
