//! EX-V6 round-trip preservation check.
//!
//! Per `design/refactor-tools/elixir/refactor-elixir-expansion.md`: every plan
//! kind that emits FileEdits MUST verify round-trip identity as part of the
//! apply step. v1 ships two surfaces, both tree-sitter-based:
//!
//!   - **`verify_parse_clean(output)`** — the enforced invariant. The output
//!     text must parse without introducing new errors. This is the safety
//!     floor every writable plan kind goes through (catches the
//!     planner-emitted-syntactically-invalid-code failure mode).
//!
//!   - **`verify_structural_equivalence(input, output)`** — strict shape
//!     compare. ALL named children must match by kind + count + leaf text.
//!     Used by plan kinds that promise to preserve structure (currently
//!     none in v1; reserved for refactor-no-op or idempotent flows where
//!     drift would signal a planner bug).
//!
//! v2 will upgrade to `Code.string_to_quoted_with_comments!/2` via the
//! escript helper, threading the metadata strip/preserve list and
//! comment-anchor verification from the design. The v2 check additionally
//! honors `expected_comment_deletions` so plan kinds that legitimately
//! remove comments declare them in advance.

use anyhow::{Result, anyhow};
use tree_sitter::Node;

use super::parse_elixir;

/// The v1 enforced invariant: output text must parse without new errors.
///
/// Catches the planner-emitted-syntactically-invalid-code failure mode. Every
/// writable plan kind calls this before returning its plan.
pub(crate) fn verify_parse_clean(output: &str) -> Result<()> {
    let tree = parse_elixir(output)?;
    if has_parse_error(tree.root_node()) {
        return Err(anyhow!(
            "error.roundtrip_unstable: output text contains parse errors"
        ));
    }
    Ok(())
}

/// Convenience wrapper used by writable plan kinds: apply the proposed
/// `TextEdit`s to a copy of `source` (in reverse byte order to preserve
/// indices) and call [`verify_parse_clean`] on the result.
pub(crate) fn verify_edits_parse_clean(source: &str, edits: &[crate::TextEdit]) -> Result<()> {
    let mut probe = source.to_string();
    let mut sorted: Vec<&crate::TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.byte_start));
    for e in sorted {
        if e.byte_end > probe.len() {
            return Err(anyhow!(
                "error.roundtrip_unstable: edit byte_end {} exceeds source length {}",
                e.byte_end,
                probe.len()
            ));
        }
        probe.replace_range(e.byte_start..e.byte_end, &e.replacement);
    }
    verify_parse_clean(&probe)
}

/// Strict shape compare. Returns `Ok(())` when input and output trees match
/// by kind + named-child count + leaf text. Reserved for refactor-no-op /
/// idempotent flows; intentional restructure refactors don't use this surface
/// in v1 (the design's writable lane via `string_to_quoted_with_comments`
/// + `expected_comment_deletions` is the v2 path that supports declared
/// deletions).
#[allow(dead_code)] // reserved for v2 idempotent-flow hookup
pub(crate) fn verify_structural_equivalence(input: &str, output: &str) -> Result<()> {
    let input_tree = parse_elixir(input)?;
    let output_tree = parse_elixir(output)?;

    let input_has_error = has_parse_error(input_tree.root_node());
    let output_has_error = has_parse_error(output_tree.root_node());
    if output_has_error && !input_has_error {
        return Err(anyhow!(
            "error.roundtrip_unstable: output introduced parse errors that input did not have"
        ));
    }

    let diff = compare_structure(
        input_tree.root_node(),
        output_tree.root_node(),
        input,
        output,
        Vec::new(),
    );
    if let Some(d) = diff {
        return Err(anyhow!("error.roundtrip_unstable: {d}"));
    }
    Ok(())
}

fn has_parse_error(node: Node<'_>) -> bool {
    if node.has_error() {
        return true;
    }
    let mut cursor = node.walk();
    for c in node.named_children(&mut cursor) {
        if has_parse_error(c) {
            return true;
        }
    }
    false
}

/// DFS structural compare. Returns `Some(diff_description)` on mismatch.
/// Path is the kind-stack from root for diagnostic.
fn compare_structure(
    a: Node<'_>,
    b: Node<'_>,
    src_a: &str,
    src_b: &str,
    path: Vec<String>,
) -> Option<String> {
    if a.kind() != b.kind() {
        return Some(format!(
            "kind mismatch at {}: input {} vs output {}",
            path.join(" / "),
            a.kind(),
            b.kind()
        ));
    }

    // For leaf nodes (no named children), compare textual content. This catches
    // identifier renames, atom value changes, etc.
    let mut cursor_a = a.walk();
    let mut cursor_b = b.walk();
    let children_a: Vec<Node<'_>> = a.named_children(&mut cursor_a).collect();
    let children_b: Vec<Node<'_>> = b.named_children(&mut cursor_b).collect();

    if children_a.is_empty() && children_b.is_empty() {
        // Leaf: compare exact text.
        let text_a = &src_a[a.byte_range()];
        let text_b = &src_b[b.byte_range()];
        if text_a != text_b {
            return Some(format!(
                "leaf text mismatch at {} ({}): {:?} vs {:?}",
                path.join(" / "),
                a.kind(),
                text_a,
                text_b
            ));
        }
        return None;
    }

    if children_a.len() != children_b.len() {
        return Some(format!(
            "child count mismatch at {} ({}): input has {} children, output has {}",
            path.join(" / "),
            a.kind(),
            children_a.len(),
            children_b.len()
        ));
    }

    for (i, (ca, cb)) in children_a.iter().zip(children_b.iter()).enumerate() {
        let mut p = path.clone();
        p.push(format!("{}[{}]", a.kind(), i));
        if let Some(d) = compare_structure(*ca, *cb, src_a, src_b, p) {
            return Some(d);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_accepts_valid_output() {
        let src = "defmodule Foo do\n  alias Bar\n  def hi, do: :ok\nend\n";
        assert!(verify_parse_clean(src).is_ok());
    }

    #[test]
    fn parse_clean_rejects_mismatched_braces() {
        let output = "defmodule Foo do\n  def hi, do: {a, b\nend\n";
        assert!(verify_parse_clean(output).is_err());
    }

    #[test]
    fn parse_clean_rejects_unclosed_string() {
        let output = "defmodule Foo do\n  def hi, do: \"unclosed\nend\n";
        assert!(verify_parse_clean(output).is_err());
    }

    #[test]
    fn structural_equiv_identical_source_passes() {
        let src = "defmodule Foo do\n  alias Bar\n  def hi, do: :ok\nend\n";
        assert!(verify_structural_equivalence(src, src).is_ok());
    }

    #[test]
    fn structural_equiv_whitespace_normalize_passes() {
        let input = "defmodule Foo do\n  def hi, do: :ok\nend\n";
        let output = "defmodule Foo do\n    def hi, do: :ok\nend\n";
        assert!(verify_structural_equivalence(input, output).is_ok());
    }

    #[test]
    fn structural_equiv_kind_swap_fails() {
        let input = "defmodule Foo do\n  def hi, do: :ok\nend\n";
        let output = "defmodule Foo do\n  defp hi, do: :ok\nend\n";
        assert!(verify_structural_equivalence(input, output).is_err());
    }

    #[test]
    fn structural_equiv_dropped_clause_fails() {
        let input = "defmodule Foo do\n  def hi, do: :ok\n  def bye, do: :gone\nend\n";
        let output = "defmodule Foo do\n  def hi, do: :ok\nend\n";
        assert!(verify_structural_equivalence(input, output).is_err());
    }
}
