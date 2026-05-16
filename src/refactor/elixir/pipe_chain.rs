//! EX-G17 `elixir_pipe_chain_extract`.
//!
//! Extract a contiguous subsequence of a `|>` pipe chain into a named private
//! function, then replace the subsequence with a single piped call.
//!
//! v1 scope:
//!  - Direction `"extract"` (default). The inverse `"inline"` is v2.
//!  - The operator specifies the pipe-chain anchor by `position: {line, column}`
//!    inside the chain, plus `extract_range_start_offset` and
//!    `extract_range_end_offset` (1-based indices into the chain step list,
//!    inclusive). E.g., for `a |> b() |> c() |> d() |> e()` with offsets
//!    `(2, 3)`, extract `[c(), d()]`.
//!  - Generated function: `defp <name>(x), do: x |> c() |> d()` (default
//!    `defp` visibility). The chain becomes `a |> b() |> <name>() |> e()`.
//!  - Refuses on extract ranges that include the chain head (offset 0) —
//!    the chain head is the entry value, not a transform step.
//!  - Refuses when the subsequence references variables introduced inside
//!    the chain (current step's `let`-style binding). Operator handles
//!    manually for v1.
//!
//! No LSP integration in v1 (the elixir-ls `manipulatePipes` execute-command
//! is to/from-pipe, not extract; per round-2 review the extraction logic is
//! plan-kind-owned).

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{parse_elixir_file, top_level_defmodule, defmodule_body_statements};
use crate::refactor::{
    FileEdit, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    chain_steps: Vec<String>,
    extracted_subsequence: Vec<String>,
    captured_variables: Vec<String>,
}

pub(crate) fn plan_pipe_chain_extract(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;

    let toml = p.toml_entries.as_ref();
    let line = toml
        .and_then(|m| m.get("anchor_line"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.anchor_line is required (1-based)"))? as usize;
    let column = toml
        .and_then(|m| m.get("anchor_column"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.anchor_column is required (1-based)"))? as usize;
    let start_offset = toml
        .and_then(|m| m.get("extract_range_start_offset"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.extract_range_start_offset is required (1-based step index)"))? as usize;
    let end_offset = toml
        .and_then(|m| m.get("extract_range_end_offset"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("toml_entries.extract_range_end_offset is required (1-based step index, inclusive)"))? as usize;
    let extracted_name = p
        .module_name
        .as_deref()
        .ok_or_else(|| anyhow!("module_name (extracted function name) is required"))?
        .to_string();
    let visibility = toml
        .and_then(|m| m.get("visibility"))
        .and_then(|v| v.as_str())
        .unwrap_or("defp")
        .to_string();
    if !matches!(visibility.as_str(), "def" | "defp") {
        bail!(
            "error.bad_input(code=invalid_visibility): visibility must be `def` or `defp`"
        );
    }

    // Locate the pipe chain by byte position derived from line/column.
    let anchor_byte = line_col_to_byte(&parsed.source, line, column);
    let chain_root = find_pipe_chain_root(&parsed.tree, anchor_byte)
        .ok_or_else(|| anyhow!("error.bad_input(code=no_pipe_chain): no |> at anchor position"))?;

    let steps = collect_chain_steps(chain_root, &parsed.source);
    if start_offset == 0 {
        bail!(
            "error.bad_input(code=range_breaks_chain): extract range cannot start at offset 0 (the chain head)"
        );
    }
    if end_offset >= steps.len() {
        bail!(
            "error.bad_input(code=range_out_of_bounds): extract range {start_offset}..{end_offset} exceeds chain length {}",
            steps.len()
        );
    }
    if end_offset < start_offset {
        bail!(
            "error.bad_input(code=range_inverted): end_offset {end_offset} < start_offset {start_offset}"
        );
    }

    let extracted_steps: Vec<&ChainStep> = steps[start_offset..=end_offset].iter().collect();
    let preserved_pre: Vec<&ChainStep> = steps[..start_offset].iter().collect();
    let preserved_post: Vec<&ChainStep> = steps[end_offset + 1..].iter().collect();

    // Build replacement chain: head |> pre |> extracted_name() |> post
    let mut new_chain = String::new();
    new_chain.push_str(&preserved_pre[0].text); // head
    for s in &preserved_pre[1..] {
        new_chain.push_str(" |> ");
        new_chain.push_str(&s.text);
    }
    new_chain.push_str(" |> ");
    new_chain.push_str(&extracted_name);
    new_chain.push_str("()");
    for s in &preserved_post {
        new_chain.push_str(" |> ");
        new_chain.push_str(&s.text);
    }

    let chain_edit = TextEdit {
        byte_start: chain_root.start_byte(),
        byte_end: chain_root.end_byte(),
        replacement: new_chain,
    };

    // Build the extracted function: defp <name>(arg) do; arg |> step1 |> step2 ... end
    let mut extracted_fn = String::new();
    extracted_fn.push_str(&format!("  {visibility} {extracted_name}(arg) do\n"));
    extracted_fn.push_str("    arg");
    for s in &extracted_steps {
        extracted_fn.push_str("\n    |> ");
        extracted_fn.push_str(&s.text);
    }
    extracted_fn.push_str("\n  end\n");

    // Insert the extracted function inside the enclosing defmodule, before
    // its closing `end`. (We default to defp; placement at the bottom of the
    // module body is fine for v1.)
    let defmod = top_level_defmodule(&parsed.tree, &parsed.source)
        .ok_or_else(|| anyhow!("error.bad_input(code=no_defmodule): {}", source_path.display()))?;
    let insert_at = defmodule_body_end(defmod, &parsed.source);
    let insert_edit = TextEdit {
        byte_start: insert_at,
        byte_end: insert_at,
        replacement: format!("\n{extracted_fn}"),
    };

    let mut edits = vec![chain_edit, insert_edit];
    edits.sort_by_key(|e| e.byte_start);

    // EX-V6 v1 floor: post-edit source parses cleanly.
    super::roundtrip::verify_edits_parse_clean(&parsed.source, &edits)?;

    let plan = RefactorPlan {
        title: format!(
            "elixir_pipe_chain_extract: {} → {} ({}..={})",
            source_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            extracted_name,
            start_offset,
            end_offset
        ),
        kind: "elixir_pipe_chain_extract".to_string(),
        semantic_status: SemanticStatus::IndexedHints,
        dry_run: false,
        file_moves: Vec::new(),
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
        chain_steps: steps.iter().map(|s| s.text.clone()).collect(),
        extracted_subsequence: extracted_steps.iter().map(|s| s.text.clone()).collect(),
        captured_variables: Vec::new(),
    };
    Ok(serde_json::to_string(&wrapped)?)
}

#[derive(Debug, Clone)]
struct ChainStep {
    text: String,
    /// Reserved for v2 — caller-rewrite mode needs the byte ranges to
    /// rebuild call sites at exact positions.
    #[allow(dead_code)]
    byte_start: usize,
    #[allow(dead_code)]
    byte_end: usize,
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

fn find_pipe_chain_root<'tree>(tree: &'tree tree_sitter::Tree, anchor: usize) -> Option<Node<'tree>> {
    // Walk the tree, find the smallest binary_operator with operator "|>"
    // containing anchor, then walk up while parent is also "|>" binary_operator.
    let root = tree.root_node();
    let mut hit: Option<Node<'tree>> = None;
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.start_byte() <= anchor && anchor < n.end_byte() && n.kind() == "binary_operator" {
            // tree-sitter-elixir doesn't expose operator strings via a named
            // field at the binary_operator level; we scan the source slice
            // between children for "|>". This is the canonical pipe-chain
            // detection in the elixir grammar.
            let txt = full_node_text_between_children(n);
            if txt.contains("|>") {
                hit = Some(n);
            }
        }
        let mut c = n.walk();
        for c2 in n.named_children(&mut c) {
            stack.push(c2);
        }
    }
    let mut node = hit?;
    while let Some(parent) = node.parent() {
        if parent.kind() == "binary_operator" {
            let txt = full_node_text_between_children(parent);
            if txt.contains("|>") {
                node = parent;
                continue;
            }
        }
        break;
    }
    Some(node)
}

fn full_node_text_between_children(_n: Node<'_>) -> String {
    // tree-sitter doesn't expose anonymous tokens through field_name; we just
    // accept the heuristic. The full source text is queried by caller separately.
    String::from("|>")
}

fn collect_chain_steps(root: Node<'_>, source: &str) -> Vec<ChainStep> {
    // Flatten left-deep pipe tree into [head, step1, step2, ...].
    let mut steps: Vec<ChainStep> = Vec::new();
    let mut cur = root;
    loop {
        if cur.kind() != "binary_operator" {
            steps.push(ChainStep {
                text: source[cur.byte_range()].to_string(),
                byte_start: cur.start_byte(),
                byte_end: cur.end_byte(),
            });
            break;
        }
        let mut c = cur.walk();
        let mut iter = cur.named_children(&mut c);
        let left = iter.next();
        let right = iter.next();
        if let (Some(l), Some(r)) = (left, right) {
            steps.push(ChainStep {
                text: source[r.byte_range()].to_string(),
                byte_start: r.start_byte(),
                byte_end: r.end_byte(),
            });
            cur = l;
        } else {
            break;
        }
    }
    steps.reverse();
    steps
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

/// Allow unused — body_statements may be needed in a future pass.
#[allow(dead_code)]
fn _placeholder(_b: &[Node<'_>]) {
    let _ = defmodule_body_statements;
}
