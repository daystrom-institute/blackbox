//! EX-G6 `elixir_organize_aliases`.
//!
//! Sort, dedupe, and collapse module-level `alias` / `import` / `require` /
//! `use` directives. Tree-sitter-grade; analysis-only output for v1 in the
//! sense that we don't touch the writable lane yet — but the edits are
//! syntactic and the round-trip risk is minimal (we replace one contiguous
//! directive region with a regenerated one).
//!
//! Behaviour:
//!  - Walk the `defmodule Foo do ... end` body.
//!  - Find the directive block: the contiguous run of `alias`/`import`/
//!    `require`/`use` calls at the top of the body (after `@moduledoc`).
//!  - Sort within each family alphabetically.
//!  - Dedupe (`alias Foo.Bar` + `alias Foo.Bar` → one).
//!  - Collapse `alias Foo.A; alias Foo.B; alias Foo.C` →
//!    `alias Foo.{A, B, C}`.
//!  - Expand grouped forms when only one member survives.
//!  - `use` directives stay in textual order (they have side effects on
//!    subsequent directives — `use Ecto.Schema` injects macros that earlier
//!    aliases might reference).
//!
//! Refusals:
//!  - `error.bad_input(code=directive_in_macro)` — a directive nests inside
//!    a `quote do ... end` or `if @attr do alias ... end` conditional.
//!  - `error.bad_input(code=no_defmodule)` — source file has no top-level
//!    defmodule.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use tree_sitter::Node;

use super::{call_target_name, defmodule_body_statements, parse_elixir_file, top_level_defmodule};
use crate::refactor::{
    FileEdit, ParsedSource, PlanStatus, RefactorPlan, RefactorPlanParams, SemanticStatus, TextEdit,
    ValidationStep, resolve_path, sha256_hex,
};

#[derive(Debug, Serialize)]
struct PlanWithReport {
    #[serde(flatten)]
    plan: RefactorPlan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directives_sorted: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directives_merged: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directives_dropped: Vec<String>,
}

/// Plan entry point. Returns the `RefactorPlan` JSON (with an organize-specific
/// extension).
pub(crate) fn plan_organize_aliases(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let parsed = parse_elixir_file(&source_path)?;

    let defmodule = top_level_defmodule(&parsed.tree, &parsed.source).ok_or_else(|| {
        anyhow!(
            "error.bad_input(code=no_defmodule): {} has no top-level defmodule",
            source_path.display()
        )
    })?;

    let body_stmts = defmodule_body_statements(defmodule, &parsed.source);
    let block = locate_directive_block(&body_stmts, &parsed.source)?;
    let Some(block) = block else {
        // No directives present — emit an empty plan with status `Planned`.
        let plan = empty_plan(&parsed);
        let wrapped = PlanWithReport {
            plan,
            directives_sorted: Vec::new(),
            directives_merged: Vec::new(),
            directives_dropped: Vec::new(),
        };
        return Ok(serde_json::to_string(&wrapped)?);
    };

    // Reject directives inside macro/conditional scopes by checking that no
    // directive's parent chain crosses a `quote` or non-defmodule `do_block`.
    for entry in &block.entries {
        if directive_in_macro_scope(entry.node, defmodule) {
            bail!(
                "error.bad_input(code=directive_in_macro): directive at line {} is inside a quote/conditional block",
                entry.line
            );
        }
    }

    let (rendered, report) = organize_block(&block, &parsed.source);

    // Build the replacement edit. We replace the original byte range with the
    // rendered text, preserving the leading indentation of the first entry.
    let leading_indent = leading_indent_of(&parsed.source, block.byte_start);
    let replacement = reindent(&rendered, &leading_indent);
    let edit = TextEdit {
        byte_start: block.byte_start,
        byte_end: block.byte_end,
        replacement,
    };
    let file_edit = FileEdit {
        path: source_path.to_string_lossy().into_owned(),
        original_sha256: sha256_hex(parsed.source.as_bytes()),
        edits: vec![edit],
        new_text: None,
    };

    // EX-V6 v1 floor: verify the proposed output parses cleanly. organize_aliases
    // intentionally restructures the alias section (merges, dedupes), so the
    // strict structural-equivalence check would refuse legitimate output.
    {
        let mut probe = parsed.source.clone();
        probe.replace_range(
            block.byte_start..block.byte_end,
            &file_edit.edits[0].replacement,
        );
        super::roundtrip::verify_parse_clean(&probe)?;
    }

    let plan = RefactorPlan {
        title: format!(
            "elixir_organize_aliases: {}",
            source_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        kind: "elixir_organize_aliases".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: vec![file_edit],
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
        operator_opt_outs_used: Vec::new(),
    };

    let wrapped = PlanWithReport {
        plan,
        directives_sorted: report.sorted,
        directives_merged: report.merged,
        directives_dropped: report.dropped,
    };
    Ok(serde_json::to_string(&wrapped)?)
}

fn empty_plan(parsed: &ParsedSource) -> RefactorPlan {
    RefactorPlan {
        title: format!(
            "elixir_organize_aliases: {} (no directives)",
            parsed
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        kind: "elixir_organize_aliases".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: false,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
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
        operator_opt_outs_used: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Directive block extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Family {
    Use,
    Alias,
    Import,
    Require,
}

impl Family {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "use" => Some(Self::Use),
            "alias" => Some(Self::Alias),
            "import" => Some(Self::Import),
            "require" => Some(Self::Require),
            _ => None,
        }
    }

    fn keyword(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Alias => "alias",
            Self::Import => "import",
            Self::Require => "require",
        }
    }
}

#[derive(Debug)]
struct DirectiveEntry<'tree> {
    family: Family,
    node: Node<'tree>,
    /// The full text after the keyword and any leading whitespace
    /// (e.g., for `alias Foo.{A, B}, warn: false` → `Foo.{A, B}, warn: false`).
    args_text: String,
    /// 1-based line number, for diagnostics.
    line: usize,
}

#[derive(Debug)]
struct DirectiveBlock<'tree> {
    entries: Vec<DirectiveEntry<'tree>>,
    byte_start: usize,
    byte_end: usize,
}

/// Find the maximal contiguous run of directive statements at the top of the
/// defmodule body (after any leading `@moduledoc` or other attributes).
/// Returns `Ok(None)` when no directives are present.
fn locate_directive_block<'tree>(
    body: &[Node<'tree>],
    source: &str,
) -> Result<Option<DirectiveBlock<'tree>>> {
    let mut start_idx = None;
    let mut entries = Vec::new();
    let mut current_byte_end = 0usize;

    for (idx, &stmt) in body.iter().enumerate() {
        let name = call_target_name(stmt, source);
        let family = name.and_then(Family::from_name);
        match family {
            Some(family) => {
                if start_idx.is_none() {
                    start_idx = Some(idx);
                }
                let args_text = directive_args_text(stmt, source);
                let (line, _) = byte_to_line_col(source, stmt.start_byte());
                entries.push(DirectiveEntry {
                    family,
                    node: stmt,
                    args_text,
                    line,
                });
                current_byte_end = stmt.end_byte();
            }
            None => {
                if start_idx.is_some() {
                    // End of contiguous run.
                    break;
                }
                // Skip attributes like @moduledoc until we either find directives
                // or run out — but only allow module attribute calls (`@`) and
                // unary_operator-shaped attributes through.
                if !is_module_attribute_node(stmt) {
                    // A non-directive, non-attribute statement before the directive
                    // block — bail (block already ended, just nothing in it).
                    break;
                }
            }
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }
    let byte_start = entries.first().unwrap().node.start_byte();
    Ok(Some(DirectiveBlock {
        entries,
        byte_start,
        byte_end: current_byte_end,
    }))
}

fn is_module_attribute_node(node: Node<'_>) -> bool {
    // Module attributes parse as `unary_operator` with `@` operator. Tree-sitter
    // elixir grammar treats `@moduledoc "..."` as `unary_operator { operator:
    // "@", operand: call }`.
    node.kind() == "unary_operator"
}

fn directive_args_text(call: Node<'_>, source: &str) -> String {
    // For `alias Foo.Bar` the call has an `arguments` child holding `Foo.Bar`.
    // For `alias Foo.{A, B}, warn: false` arguments holds the whole arg list.
    // The grammar puts the identifier (target) as named_child(0); we use the
    // byte range from end-of-keyword to end-of-call, trimmed.
    let Some(target) = call.named_child(0) else {
        return String::new();
    };
    let after_kw = target.end_byte();
    let end = call.end_byte();
    if after_kw >= end {
        return String::new();
    }
    source[after_kw..end].trim().to_string()
}

fn directive_in_macro_scope(node: Node<'_>, defmodule_call: Node<'_>) -> bool {
    // Walk up from `node` and verify the chain to `defmodule_call` only crosses
    // the defmodule's do_block. Any other do_block/quote in the chain means
    // we're inside a nested scope.
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        if parent == defmodule_call {
            return false;
        }
        if parent.kind() == "do_block" {
            // do_block of the defmodule call itself is fine; any other
            // do_block (a nested defmodule, def, fn, if, quote, etc.) is
            // a macro/conditional scope.
            if let Some(grandparent) = parent.parent() {
                if grandparent == defmodule_call {
                    cur = parent;
                    continue;
                }
            }
            return true;
        }
        cur = parent;
    }
    false
}

// ---------------------------------------------------------------------------
// Organize logic
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct OrganizeReport {
    sorted: Vec<String>,
    merged: Vec<String>,
    dropped: Vec<String>,
}

/// Render the organized directive block. Returns the rendered multi-line
/// string (without leading indentation — that's added by the caller) and a
/// report.
fn organize_block(block: &DirectiveBlock<'_>, _source: &str) -> (String, OrganizeReport) {
    let mut report = OrganizeReport::default();

    // Group entries by family while preserving textual order for `use`.
    let mut uses: Vec<String> = Vec::new();
    // For alias/import/require we collect (parent_path, member_or_none, suffix)
    // where suffix is the trailing `, warn: false`-style keyword list (if any).
    let mut grouped: BTreeMap<Family, Vec<ParsedDirective>> = BTreeMap::new();

    for entry in &block.entries {
        let raw = format!("{} {}", entry.family.keyword(), entry.args_text);
        match entry.family {
            Family::Use => uses.push(raw),
            family => {
                let parsed = ParsedDirective::parse(&entry.args_text);
                grouped.entry(family).or_default().push(parsed);
            }
        }
    }

    let mut lines: Vec<String> = Vec::new();

    // `use` first, textual order preserved.
    for u in &uses {
        lines.push(u.clone());
    }

    // alias, import, require in fixed order (alphabetical of keyword: alias,
    // import, require). BTreeMap iteration is by Family ordering.
    for (family, entries) in grouped {
        let lines_for_family = render_family(family, entries, &mut report);
        lines.extend(lines_for_family);
    }

    let rendered = lines.join("\n");
    (rendered, report)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedDirective {
    /// `Foo.Bar` for `alias Foo.Bar`, `Foo` for `alias Foo.{A, B}` etc.
    parent: String,
    /// `["A","B"]` for grouped form `alias Foo.{A, B}`; `["Bar"]` for single
    /// `alias Foo.Bar` (parent: `"Foo"`, members: `["Bar"]`); empty for bare
    /// `alias Foo` (parent: `"Foo"`, members empty — treat as single-module).
    members: Vec<String>,
    /// Trailing keyword args (`, warn: false`, `, as: Other`).
    suffix: Option<String>,
}

impl ParsedDirective {
    fn parse(args: &str) -> Self {
        // Split off the suffix at the FIRST top-level `,` after a `}` or after
        // the bare module ref. Conservative parser: scan once, track brace depth.
        let bytes = args.as_bytes();
        let mut depth = 0i32;
        let mut split_at = None;
        for (i, &b) in bytes.iter().enumerate() {
            match b {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b',' if depth == 0 => {
                    split_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let (main_str, suffix_opt) = match split_at {
            Some(i) => (&args[..i], Some(args[i + 1..].trim().to_string())),
            None => (args, None),
        };
        let main = main_str.trim();
        // Grouped form: `Foo.{A, B}` or `Foo.Bar.{A, B}`
        if let Some(brace) = main.find('{') {
            let parent = main[..brace].trim_end_matches('.').trim().to_string();
            let close = main.rfind('}').unwrap_or(main.len());
            let inner = &main[brace + 1..close];
            let members = inner
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            ParsedDirective {
                parent,
                members,
                suffix: suffix_opt,
            }
        } else if let Some(last_dot) = main.rfind('.') {
            // `Foo.Bar.Baz` — parent `Foo.Bar`, single member `Baz`.
            ParsedDirective {
                parent: main[..last_dot].to_string(),
                members: vec![main[last_dot + 1..].to_string()],
                suffix: suffix_opt,
            }
        } else {
            // Bare `Foo` — alias of top-level module. Treat as parent=Foo,
            // empty members so render keeps the bare form.
            ParsedDirective {
                parent: main.to_string(),
                members: Vec::new(),
                suffix: suffix_opt,
            }
        }
    }
}

fn render_family(
    family: Family,
    entries: Vec<ParsedDirective>,
    report: &mut OrganizeReport,
) -> Vec<String> {
    // Group by (parent, suffix) — only entries with no suffix and matching
    // parent can be merged into a grouped form.
    let mut by_parent: BTreeMap<(String, Option<String>), Vec<String>> = BTreeMap::new();
    // Bare (no member) directives go through unchanged but are deduped.
    let mut bares: BTreeMap<(String, Option<String>), ()> = BTreeMap::new();

    for entry in entries {
        if entry.members.is_empty() {
            bares.insert((entry.parent.clone(), entry.suffix.clone()), ());
            continue;
        }
        let key = (entry.parent.clone(), entry.suffix.clone());
        let bucket = by_parent.entry(key).or_default();
        for m in entry.members {
            if !bucket.contains(&m) {
                bucket.push(m);
            } else {
                report.dropped.push(format!(
                    "{} {}.{}",
                    family.keyword(),
                    entry.parent,
                    bucket.last().cloned().unwrap_or_default()
                ));
            }
        }
    }

    let mut lines = Vec::new();

    // Bare first (alphabetical by parent).
    for ((parent, suffix), ()) in bares {
        let s = match suffix.as_deref() {
            Some(s) => format!("{} {}, {}", family.keyword(), parent, s),
            None => format!("{} {}", family.keyword(), parent),
        };
        report.sorted.push(s.clone());
        lines.push(s);
    }

    // Grouped / single.
    for ((parent, suffix), mut members) in by_parent {
        members.sort();
        let line = if members.len() == 1 {
            match suffix.as_deref() {
                Some(s) => format!("{} {}.{}, {}", family.keyword(), parent, members[0], s),
                None => format!("{} {}.{}", family.keyword(), parent, members[0]),
            }
        } else {
            let inner = members.join(", ");
            let merged_line = match suffix.as_deref() {
                Some(s) => format!("{} {}.{{{}}}, {}", family.keyword(), parent, inner, s),
                None => format!("{} {}.{{{}}}", family.keyword(), parent, inner),
            };
            report.merged.push(merged_line.clone());
            merged_line
        };
        report.sorted.push(line.clone());
        lines.push(line);
    }

    lines
}

// ---------------------------------------------------------------------------
// Whitespace utilities
// ---------------------------------------------------------------------------

fn leading_indent_of(source: &str, byte: usize) -> String {
    // Walk backwards from `byte` to the start of the line; return the
    // whitespace prefix.
    let bytes = source.as_bytes();
    let mut start = byte;
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let line_slice = &bytes[start..byte];
    let mut indent = Vec::new();
    for &b in line_slice {
        if b == b' ' || b == b'\t' {
            indent.push(b);
        } else {
            break;
        }
    }
    String::from_utf8(indent).unwrap_or_default()
}

fn reindent(rendered: &str, indent: &str) -> String {
    rendered
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                // First line: caller's indent is already implicit in the byte
                // range we're replacing.
                line.to_string()
            } else {
                format!("{}{}", indent, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
