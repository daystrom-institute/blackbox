//! EX-G18 `elixir_with_clause_extract`.
//!
//! Extract a contiguous prefix (or suffix, or arbitrary slice) of a `with`
//! block's clauses into a separate function. The extracted function returns
//! `{:ok, intermediate} | {:error, reason}` so the parent `with`'s
//! failure-arm semantics are preserved.
//!
//! v1 scope:
//!  - Operator names the `with` block by anchor (`anchor_line`,
//!    `anchor_column`) and the clause indices to extract (1-based,
//!    inclusive: `extract_start_clause`, `extract_end_clause`).
//!  - The extracted function takes the bindings introduced BEFORE the
//!    extracted range as arguments, and the LAST extracted clause's binding
//!    becomes the `{:ok, _}` payload.
//!  - The `else` block stays with the parent; v1 does not move `else` arms.
//!  - Refusals: anchor not in a `with`, range out of bounds, `else_arm_residue`
//!    (any non-trivial else patterns; v1 conservative — require
//!    `acknowledge_else_arm_residue: true`).

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex, toml_bool,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    clause_count: usize,
    extracted_clauses: Vec<String>,
    has_else_arm: bool,
}

pub(crate) fn plan_with_clause_extract(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;
    let toml = p.toml_entries.as_ref();
    let line = toml
        .and_then(|m| m.get("anchor_line"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.anchor_line is required"))? as usize;
    let column =
        toml.and_then(|m| m.get("anchor_column"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("toml_entries.anchor_column is required"))? as usize;
    let start_idx = toml
        .and_then(|m| m.get("extract_start_clause"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.extract_start_clause (1-based) is required"))?
        as usize;
    let end_idx = toml
        .and_then(|m| m.get("extract_end_clause"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.extract_end_clause (1-based) is required"))?
        as usize;
    let extracted_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (extracted function name) is required"))?
        .to_string();
    let ack_else_residue = toml_bool(&p.toml_entries, "acknowledge_else_arm_residue");

    // Locate the `with` call at the anchor.
    let anchor = line_col_to_byte(&parsed.source, line, column);
    let with_call = find_with_call_at(&parsed.tree, &parsed.source, anchor)
        .ok_or_else(|| anyhow!("error.bad_input(code=no_with_block): no `with` at anchor"))?;

    let clauses = collect_with_clauses(with_call, &parsed.source);
    if start_idx == 0 || end_idx == 0 {
        bail!("error.bad_input(code=invalid_range): clause indices are 1-based");
    }
    if end_idx < start_idx {
        bail!("error.bad_input(code=range_inverted): end {end_idx} < start {start_idx}");
    }
    if end_idx > clauses.len() {
        bail!(
            "error.bad_input(code=range_out_of_bounds): end {end_idx} > clause count {}",
            clauses.len()
        );
    }
    let has_else = clauses.iter().any(|c| c.is_else);
    if has_else && !ack_else_residue {
        bail!(
            "error.bad_input(code=else_arm_residue): with block has else arms; pass acknowledge_else_arm_residue=true to proceed (v1 leaves else with parent)"
        );
    }

    // Slice clauses (1-based inclusive → 0-based exclusive end).
    let extracted = &clauses[start_idx - 1..end_idx];
    let preserved_pre = &clauses[..start_idx - 1];
    let preserved_post = &clauses[end_idx..];

    // Inputs to extracted fn: variables bound in preserved_pre that are used
    // by extracted (heuristic v1: just take all preserved_pre bindings as
    // params).
    let params: Vec<String> = preserved_pre
        .iter()
        .filter_map(|c| c.lhs_binding.clone())
        .collect();

    // Build the extracted function: returns the last extracted clause's
    // binding wrapped in {:ok, _}, or :error on failure.
    let extracted_name_clone = extracted_name.clone();
    let last_binding = extracted
        .last()
        .and_then(|c| c.lhs_binding.clone())
        .unwrap_or_else(|| "result".to_string());

    let inner = extracted
        .iter()
        .map(|c| c.text.clone())
        .collect::<Vec<_>>()
        .join(",\n         ");
    let extracted_fn = format!(
        "  defp {extracted_name}({params_csv}) do\n    with {inner} do\n      {{:ok, {last_binding}}}\n    end\n  end\n",
        params_csv = params.join(", ")
    );

    // Rewrite the with block: replace the extracted range with
    // `{:ok, last_binding} <- extracted_name(params)`.
    let replacement_clause = format!(
        "{{:ok, {last_binding}}} <- {extracted_name_clone}({})",
        params.join(", ")
    );

    let mut rebuilt = String::new();
    rebuilt.push_str("with ");
    let mut first = true;
    for c in preserved_pre {
        if !first {
            rebuilt.push_str(",\n     ");
        }
        rebuilt.push_str(&c.text);
        first = false;
    }
    if !first {
        rebuilt.push_str(",\n     ");
    }
    rebuilt.push_str(&replacement_clause);
    for c in preserved_post {
        if c.is_else {
            // Keep else arms verbatim.
            rebuilt.push_str(",\n");
            rebuilt.push_str(&c.text);
        } else {
            rebuilt.push_str(",\n     ");
            rebuilt.push_str(&c.text);
        }
    }

    let with_edit = TextEdit {
        byte_start: with_call.start_byte(),
        byte_end: with_call.end_byte(),
        replacement: rebuilt,
    };

    // Insert extracted_fn before defmodule's closing end.
    let defmod = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {}",
            source_path.display()
        )
    })?;
    let insert_at = defmodule_body_end(defmod, &parsed.source);
    let insert_edit = TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement: format!("\n{extracted_fn}"),
    };

    let mut edits = vec![with_edit, insert_edit];
    edits.sort_by_key(|e| e.byte_start);

    // EX-V6 v1 floor: post-edit source parses cleanly.
    super::roundtrip::verify_edits_parse_clean(&parsed.source, &edits)?;

    let plan = RefactorPlan {
        title: format!(
            "elixir_with_clause_extract: {} → {}",
            source_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            extracted_name
        ),
        kind: "elixir_with_clause_extract".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![FileEdit {
            path: source_path.to_string_lossy().into_owned(),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits,
            new_text: None,
        }],
        validations: vec![ValidationStep::TreeSitterNoErrors {
            path: source_path.to_string_lossy().into_owned(),
            byte_range: None,
        }],
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
        clause_count: clauses.len(),
        extracted_clauses: extracted.iter().map(|c| c.text.clone()).collect(),
        has_else_arm: has_else,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

#[derive(Debug, Clone)]
struct WithClause {
    text: String,
    lhs_binding: Option<String>,
    is_else: bool,
}

fn line_col_to_byte(source: &str, line: usize, col: usize) -> usize {
    let mut current_line = 1usize;
    let mut line_start = 0usize;
    for (i, b) in source.bytes().enumerate() {
        if current_line == line {
            return line_start + col.saturating_sub(1);
        }
        if b == b'\n' {
            current_line += 1;
            line_start = i + 1;
        }
    }
    source.len()
}

fn find_with_call_at<'tree>(
    tree: &'tree tree_sitter::Tree,
    source: &str,
    anchor: usize,
) -> Option<Node<'tree>> {
    let root = tree.root_node();
    let mut hit: Option<Node<'tree>> = None;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "call" && n.start_byte() <= anchor && anchor < n.end_byte() {
            if let Some(target) = n.named_child(0) {
                if target.kind() == "identifier" && &source[target.byte_range()] == "with" {
                    hit = Some(n);
                }
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
    hit
}

fn collect_with_clauses(with_call: Node<'_>, source: &str) -> Vec<WithClause> {
    // with X <- Y, Z <- W do ... else ... end — clauses live as arguments of
    // the with call. The keywords `do:`/`else:` siblings may be present.
    let mut clauses = Vec::new();
    let Some(args) = super::call_arguments(with_call) else {
        return clauses;
    };
    let mut cur = args.walk();
    for c in args.named_children(&mut cur) {
        if c.kind() == "keywords" {
            // `do: ..., else: ...` keyword args; treat else as a clause for
            // refusal purposes.
            let mut ck = c.walk();
            for pair in c.named_children(&mut ck) {
                let text = source[pair.byte_range()].to_string();
                if text.trim_start().starts_with("else") {
                    clauses.push(WithClause {
                        text,
                        lhs_binding: None,
                        is_else: true,
                    });
                }
            }
            continue;
        }
        let text = source[c.byte_range()].to_string();
        let lhs_binding = extract_lhs_binding(&text);
        clauses.push(WithClause {
            text,
            lhs_binding,
            is_else: false,
        });
    }
    clauses
}

fn extract_lhs_binding(clause_text: &str) -> Option<String> {
    // Heuristic: the LHS is the content before the leftmost `<-`.
    if let Some(idx) = clause_text.find("<-") {
        let lhs = clause_text[..idx].trim();
        // For simple bindings like `{:ok, x}` extract `x`; for plain
        // identifier `x` use it directly.
        if let Some(rest) = lhs.strip_prefix("{:ok,") {
            let cleaned = rest.trim_end_matches('}').trim();
            if !cleaned.is_empty() {
                return Some(cleaned.to_string());
            }
        }
        if !lhs.is_empty() {
            return Some(lhs.to_string());
        }
    }
    None
}

fn defmodule_body_end(defmod_call: Node<'_>, source: &str) -> usize {
    let Some(do_block) = super::call_do_block(defmod_call) else {
        return defmod_call.end_byte();
    };
    let end_byte = do_block.end_byte();
    let bytes = source.as_bytes();
    let mut idx = end_byte.saturating_sub(3);
    while idx > 0 && bytes[idx - 1] != b'\n' && bytes[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    idx
}
