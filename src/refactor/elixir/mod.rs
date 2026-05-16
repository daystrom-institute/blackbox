//! Elixir refactor plan kinds.
//!
//! Per `design/refactor-tools/elixir/refactor-elixir-expansion.md` the Elixir
//! surface lives behind the same `bbox_refactor_plan` MCP entry point as Rust
//! and Java. This module owns the per-plan-kind implementations; dispatch
//! routing happens in `src/refactor/mod.rs::plan_dispatch`.
//!
//! Two AST lanes per EX-V6:
//!  - **Writable lane** — `Code.string_to_quoted_with_comments!/2` round-trip
//!    via the daemon-managed escript helper (Open Question 1 resolution).
//!    Required for every plan kind that emits `FileEdit`s.
//!  - **Analysis lane** — tree-sitter Elixir grammar through
//!    `tree_sitter_language_pack::get_parser("elixir")`. Used for
//!    analysis-only plan kinds and for cheap structural inventory inside
//!    writable plans (the actual edits still round-trip the writable lane).
//!
//! In v1 the writable lane lives behind a feature flag — the escript helper
//! is added in a later milestone. Plan kinds that don't yet use it operate
//! syntactically on tree-sitter output and rely on EX-V6 being enforced at
//! apply time once the helper exists. Each plan kind documents which lane
//! it operates in.

use anyhow::{Result, anyhow};
use tree_sitter::{Node, Tree};

use super::ParsedSource;
use crate::chunker::code::parser_for_language;

pub(crate) mod codegen_audit;
pub(crate) mod compile_fix;
pub(crate) mod credo_fix;
pub(crate) mod dialyzer;
pub(crate) mod extract_behaviour;
pub(crate) mod extract_module;
pub(crate) mod facade;
pub(crate) mod genserver_callback;
pub(crate) mod genserver_state;
pub(crate) mod helper;
pub(crate) mod inline_module;
pub(crate) mod module_deps;
pub(crate) mod move_across_apps;
pub(crate) mod organize_aliases;
pub(crate) mod pipe_chain;
pub(crate) mod public_api_guard;
pub(crate) mod rename;
pub(crate) mod roundtrip;
pub(crate) mod split_clauses;
pub(crate) mod test_fixture;
pub(crate) mod with_clause;

pub(crate) use codegen_audit::plan_codegen_audit;
pub(crate) use compile_fix::plan_compile_fix_round;
pub(crate) use credo_fix::plan_credo_fix_round;
pub(crate) use dialyzer::plan_dialyzer_attribution;
pub(crate) use extract_behaviour::plan_extract_behaviour;
pub(crate) use extract_module::plan_extract_module;
pub(crate) use facade::plan_facade_delegations;
pub(crate) use genserver_callback::plan_extract_genserver_callback_group;
pub(crate) use genserver_state::plan_genserver_state_audit;
pub(crate) use inline_module::plan_inline_module;
pub(crate) use module_deps::plan_module_dependency_analysis;
pub(crate) use move_across_apps::plan_move_across_apps;
pub(crate) use organize_aliases::plan_organize_aliases;
pub(crate) use pipe_chain::plan_pipe_chain_extract;
pub(crate) use public_api_guard::plan_public_api_guard;
pub(crate) use rename::plan_rename_symbol;
pub(crate) use split_clauses::plan_split_clauses_by_tag;
pub(crate) use test_fixture::plan_test_fixture_extract;
pub(crate) use with_clause::plan_with_clause_extract;

// Shared helper used by sibling submodules + tests for line/col reporting.
pub(crate) fn byte_to_line_col(source: &str, byte: usize) -> (usize, usize) {
    let prefix = &source[..byte.min(source.len())];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = prefix
        .rfind('\n')
        .map(|n| byte.saturating_sub(n + 1))
        .unwrap_or(byte)
        + 1;
    (line, col)
}

// ---------------------------------------------------------------------------
// AST lane plumbing
// ---------------------------------------------------------------------------

/// Parse an Elixir source file through tree-sitter.
///
/// Analysis-only path; never emit FileEdits based purely on this parse unless
/// the plan kind is itself analysis-only. Writable plan kinds must additionally
/// round-trip through the escript writable lane (EX-V6).
pub(super) fn parse_elixir(source: &str) -> Result<Tree> {
    let mut parser = parser_for_language("elixir")?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter elixir parser returned no tree"))
}

/// Read and parse an Elixir source file. Mirrors `parse_source_file` in
/// `mod.rs` but bound to the elixir grammar; returns a `ParsedSource` so
/// downstream helpers (`syntax_item`, `line_col`, attribute capture) work
/// uniformly across languages.
pub(super) fn parse_elixir_file(path: &std::path::Path) -> Result<ParsedSource> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("failed to read {}: {e}", path.display()))?;
    let tree = parse_elixir(&source)?;
    Ok(ParsedSource {
        path: path.to_path_buf(),
        language: "elixir",
        source,
        tree,
    })
}

// ---------------------------------------------------------------------------
// Tree shape primitives
// ---------------------------------------------------------------------------
//
// tree-sitter-elixir represents Elixir's "everything is a function call"
// uniformly: `defmodule X do ... end`, `alias Foo`, `def f, do: ...` are all
// `call` nodes whose target identifier is `defmodule`, `alias`, `def`, etc.
// The do/end body is a `do_block` field on the call.
//
// Common node kinds we care about:
//   - `source`           — root
//   - `call`             — any function call; target is `identifier` field
//   - `arguments`        — call arguments list
//   - `do_block`         — body of `defmodule` / `def` / etc.
//   - `alias`            — capitalized module ref (Foo, Foo.Bar)
//   - `identifier`       — lowercase identifier or function name
//   - `atom`             — `:atom` literal
//   - `binary_operator`  — `|>`, `==`, etc.

/// Returns the `identifier`/`alias` text that names the target of a `call`
/// node — e.g., `"alias"` for `alias Foo.Bar`, `"defmodule"` for
/// `defmodule Foo do ... end`. Returns `None` for non-call nodes or calls
/// whose first named child isn't a bare identifier/alias.
///
/// tree-sitter-elixir represents the call shape as
/// `call { identifier; arguments; [do_block?] }` without field names; we
/// rely on the first named child being the target.
pub(crate) fn call_target_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    if node.kind() != "call" {
        return None;
    }
    let target = node.named_child(0)?;
    let kind = target.kind();
    if kind == "identifier" || kind == "alias" {
        return Some(&source[target.byte_range()]);
    }
    None
}

/// Iterate the top-level statements inside a `defmodule X do ... end` body.
/// Returns an empty list if the node isn't a `defmodule` call or has no body.
pub(crate) fn defmodule_body_statements<'tree>(
    defmodule_call: Node<'tree>,
    source: &str,
) -> Vec<Node<'tree>> {
    if call_target_name(defmodule_call, source) != Some("defmodule") {
        return Vec::new();
    }
    let Some(do_block) = call_do_block(defmodule_call) else {
        return Vec::new();
    };
    let mut cursor = do_block.walk();
    do_block
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "end")
        .collect()
}

/// Find the `do_block` child of a `call` node (e.g., for `defmodule … do … end`
/// or `def foo do … end`). Returns `None` for inline `do:` keyword-arg form
/// or call nodes without a block body.
pub(crate) fn call_do_block<'tree>(call: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = call.walk();
    call.named_children(&mut cursor).find(|n| n.kind() == "do_block")
}

/// Return the `arguments` child of a `call` node, if any.
#[allow(dead_code)] // used by later milestones (extract_module, split_clauses)
pub(crate) fn call_arguments<'tree>(call: Node<'tree>) -> Option<Node<'tree>> {
    let mut cursor = call.walk();
    call.named_children(&mut cursor)
        .find(|n| n.kind() == "arguments")
}

/// Extract the `(name, arity)` of a `def`/`defp`/`defmacro`/`defmacrop` call.
///
/// Handles:
///   - bare name with no args: `def hello, do: ...`        → ("hello", 0)
///   - parenthesized args:     `def hello(x, y), do: ...`  → ("hello", 2)
///   - guarded sig:            `def hello(x) when ...`     → ("hello", 1)
pub(crate) fn def_name_and_arity(def_call: Node<'_>, source: &str) -> Option<(String, usize)> {
    let arguments = call_arguments(def_call)?;
    let mut arg_cursor = arguments.walk();
    let sig = arguments.named_children(&mut arg_cursor).next()?;
    Some(sig_name_arity(sig, source))
}

fn sig_name_arity(sig: Node<'_>, source: &str) -> (String, usize) {
    match sig.kind() {
        "identifier" => (source[sig.byte_range()].to_string(), 0),
        "call" => {
            let mut cursor = sig.walk();
            let mut iter = sig.named_children(&mut cursor);
            let name_node = iter.next();
            let args = iter.next();
            let name = name_node
                .map(|n| source[n.byte_range()].to_string())
                .unwrap_or_default();
            let arity = args
                .map(|a| {
                    let mut c = a.walk();
                    a.named_children(&mut c).count()
                })
                .unwrap_or(0);
            (name, arity)
        }
        "binary_operator" => {
            // `hello(x) when guard(x)` — recurse into the left side
            let mut cursor = sig.walk();
            let left = sig.named_children(&mut cursor).next();
            match left {
                Some(l) => sig_name_arity(l, source),
                None => (String::from("__unknown__"), 0),
            }
        }
        _ => (String::from("__unknown__"), 0),
    }
}

/// Find the (single) top-level `defmodule` call in a source tree, if any.
/// Elixir convention is one defmodule per file; nested defmodules are rare
/// but legal — this returns the first top-level one.
pub(crate) fn top_level_defmodule<'tree>(tree: &'tree Tree, source: &str) -> Option<Node<'tree>> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| call_target_name(*child, source) == Some("defmodule"))
}

#[cfg(test)]
mod tests;
