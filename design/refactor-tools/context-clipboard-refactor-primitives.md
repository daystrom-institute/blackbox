---
title: "Context Clipboard Refactor Primitives"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
tags:
  - refactor-tools
  - mcp
  - editing
date: 2026-05-19
status: "archived"
brief: "Refactor-plan integration for context-clipboard slice movement, shared selectors, and compound-run composition."
---

# Context Clipboard Refactor Primitives

## Problem

The raw MCP context-clipboard surface gives external agents an easier path than
shell text edits for moving contiguous blocks. Refactor agents need the same
mechanical capability, but through the guarded refactor persona surface:

```text
bbox_refactor_plan
bbox_refactor_apply
bbox_refactor_run
```

Without a refactor-plan primitive, agents that are correctly restricted to the
refactor surface still cannot compose a literal slice move into a transactional
run with compiler checks and rollback. They must either overuse semantic
language-specific extractors or drop back to whole-file rewrites.

Companion raw MCP design: [Context Clipboard
Tools](../surfaces/mcp/context-clipboard-tools.md).

## Thesis

Implement context-clipboard movement as a shared slice engine with two front
doors:

- raw operator-facing MCP tools such as `bbox_slice_move`;
- refactor-plan kinds such as `move_text_slice` that emit normal
  `RefactorPlan` values and compose inside `bbox_refactor_run`.

The raw MCP tools should ship first to measure external-agent adoption. The
refactor-plan primitive should follow using the same selector and insertion
types so behavior does not fork.

## Goals

- Add a generic refactor plan kind for literal text movement:
  `bbox_refactor_plan(kind="move_text_slice", ...)`.
- Reuse the same selection and insertion model as the raw MCP tools.
- Emit normal `RefactorPlan` JSON with `FileEdit` entries and parse validation.
- Compose inside `bbox_refactor_run` alongside command steps such as
  `cargo check`, `mvn test`, or `dotnet test`.
- Support same-file moves with correct byte adjustment.
- Keep language-specific semantic refactor tools distinct. This primitive moves
  text; it does not update imports, callers, packages, or visibility.

## Non-Goals

- Do not replace `extract_rust_items`, `extract_java_class`,
  `rust_ra_move_item_to_module`, or other semantic tools.
- Do not run formatters implicitly.
- Do not infer target insertion points from architectural intent.
- Do not add operator-authority opt-outs; this is a literal text operation.
  These plan kinds shall never be added to the operator-authority opt-out
  registry because they have no public API or representation impact by
  definition. If an AST-assisted future variant crosses that line, it must be a
  new plan kind with its own design.
- Do not allow refactor atoms to use non-refactor raw MCP edit tools as a
  substitute for the plan/apply envelope.

## Atom And Brofile Restrictions

Refactor-persona brofiles and refactor atoms should explicitly deny the raw
mutating MCP tools:

- `bbox_slice_move`
- `bbox_slice_copy`
- `bbox_slice_delete`
- `bbox_slice_insert_text`
- `bbox_slice_replace`

Refactor agents should use `bbox_refactor_plan(kind="move_text_slice")` and
related plan kinds instead. This preserves RX-V2-style discipline: mutating
refactor work stays inside the refactor plan/apply/run envelope.

## Plan Kinds

### `move_text_slice`

Cut a selected range from `source` and insert it into `target`.

```text
bbox_refactor_plan(
  kind="move_text_slice",
  project_dir="/repo/x",
  source="src/foo.rs",
  target="src/foo_tests.rs",
  source_range={ "type": "lines", "start_line": 120, "end_line": 180 },
  insert={ "type": "after_marker", "marker": "mod tests {" }
)
```

The plan emits:

- one source deletion edit;
- one target insertion edit;
- validation steps for supported source files;
- selected-text preview in `leftovers` or a dedicated future field;
- stable refusal codes on ambiguous ranges or invalid same-file moves;
- `semantic_status: SyntaxOnly`.

### `copy_text_slice`

Same as `move_text_slice`, but no source deletion edit.

### `delete_text_slice`

Delete a selected range through the refactor envelope.

This is the refactor-surface equivalent of a guarded `sed start,end d`, with
hash checks, dirty-file checks, parse validation, and rollback.

### `replace_text_slice`

Replace a destination range with either caller-supplied `new_text` or a selected
source range.

Exactly one replacement source is allowed. Supplying both refuses with
`error.replacement_ambiguous`; supplying neither refuses with
`error.replacement_missing`.

## RefactorPlanParams

Do not tunnel the selector model through `toml_entries` in v1. Add typed
top-level fields to `RefactorPlanParams` instead:

```rust
source_range: Option<SliceSelector>,
target_range: Option<SliceSelector>,
insert: Option<InsertSelector>,
new_text: Option<String>,
```

`toml_entries` remains useful for older narrow plan kinds, but this surface is
expected to be hot enough that a stringly-typed carrier would become immediate
technical debt and a future breaking change.

`target_range` is the destination range overwritten by `replace_text_slice`.
`insert` is the destination insertion point used by `move_text_slice` and
`copy_text_slice`.

## Shared Selector Model

The raw MCP tools and refactor-plan kinds should use the same internal model and
the same JSON projection.

```rust
enum SliceSelector {
    Lines { start_line: usize, end_line: usize },
    Markers {
        start_marker: String,
        end_marker: String,
        include_markers: bool,
    },
    ExactText { text: String },
    Bytes { start: usize, end: usize },
}

enum InsertSelector {
    Line { line: usize, placement: Placement },
    BeforeMarker { marker: String, occurrence: Option<usize> },
    AfterMarker { marker: String, occurrence: Option<usize> },
    Prepend,
    Append,
}

enum SliceOp {
    Move,
    Copy,
    Delete,
    Replace,
}
```

Wire examples:

```json
{ "type": "lines", "start_line": 10, "end_line": 25 }
{ "type": "markers", "start_marker": "...", "end_marker": "...", "include_markers": false }
{ "type": "exact_text", "text": "..." }
{ "type": "bytes", "start": 4310, "end": 6492 }
{ "type": "after_marker", "marker": "mod tests {", "occurrence": 2 }
```

Line ranges are 1-based and inclusive. Marker ranges are exclusive by default.
Markers are exact literal strings in v1. Ambiguous marker or exact-text matches
refuse unless an occurrence index is provided.

## Safety And Validation

The shared engine should produce a deterministic edit plan before either front
door applies anything:

1. Resolve project, source, and target paths.
2. Read source and target bytes.
3. Resolve the source slice into byte coordinates.
4. Resolve the insertion or replacement target into byte coordinates.
5. Reject empty selections and out-of-bounds ranges.
6. Reject insertion inside the selected range for same-file moves.
7. Adjust same-file insertion coordinates when deletion precedes insertion.
8. Build non-overlapping `TextEdit` sets per file.
9. Validate supported source files through existing parse-validation helpers.
10. Return normal `RefactorPlan` JSON.

`bbox_refactor_apply` already owns dirty-worktree checks, hash checks,
registered-project scope, cross-worktree apply refusal, atomic writes, and
rollback. The plan kind should lean on that instead of reimplementing apply-time
safety.

Implementation should reuse or factor the existing helpers in `src/refactor/`
for:

- registered-project path checks;
- dirty-file checks;
- SHA-256 calculation;
- parse-validation step creation;
- rewritten-file validation;
- file snapshots and snapshot restore.

Unsupported file types are allowed with `semantic_status: SyntaxOnly`, hash
checks, and rollback checks, matching the generic `replace_text`/`write_file`
spirit. Supported files that parse cleanly before the edit but fail after the
edit must block apply and roll back.

## Same-File Moves

Same-file moves are a core reason for this primitive. The implementation must
not treat source and target file edits as independent when paths are equal.

Resolve the selected range to `[start, end)` byte coordinates before applying
these rules:

| Insertion byte | Behavior |
|---|---|
| `insert < start` | Allowed. Delete original range, insert selected text at `insert`. |
| `insert == start` | Refuse with `error.same_file_noop`. |
| `start < insert < end` | Refuse with `error.insert_inside_selection`. |
| `insert == end` | Refuse with `error.same_file_noop`. |
| `insert > end` | Allowed. Delete original range, then subtract `end - start` from insertion byte before inserting. |

Final preview should be computed from the post-edit text.

## Compound Run Use

The plan kind should be usable inside `bbox_refactor_run`:

```json
[
  {
    "op": "plan",
    "kind": "move_text_slice",
    "source": "src/foo.rs",
    "target": "src/foo_tests.rs",
    "source_range": { "type": "lines", "start_line": 120, "end_line": 180 },
    "insert": { "type": "append" }
  },
  {
    "op": "command",
    "command": "cargo",
    "args": ["test", "--lib"],
    "required": true
  }
]
```

This gives refactor atoms a narrow, auditable alternative to whole-file rewrites
for mechanical block relocation.

## AST-Assisted Expansion

The first version should stay literal. Later versions can accept AST-derived
selectors after the literal surface is stable:

- `enclosing_node_at`: line/column plus node kind;
- `item_named`: language-aware item lookup via `bbox_refactor_status`;
- `test_method_named`: Java/JUnit or Rust test function selectors;
- `insert_after_item`: destination insertion after a named method, function, or
  class member.

AST selectors locate slices. They never rewrite imports, callers, visibility, or
packages. If a future variant performs those semantic rewrites, it is a separate
plan kind.

## Implementation Notes

Likely home:

- Shared engine: new `src/slices.rs` or `src/refactor/slices.rs`.
- Refactor plan dispatch: add plan kinds in `src/refactor/mod.rs`.
- Raw MCP adapters: call the same shared engine from `src/tools/`.
- Tests: unit-test the shared resolver heavily, then add plan/apply tests for
  each plan kind.

The shared engine should return an intermediate structure:

```rust
struct SliceEditPlan {
    operation: SliceOp,
    files: Vec<SliceFileEdit>,
    selected_text: String,
    previews: SlicePreviews,
    validations: Vec<ValidationStep>,
}
```

Refactor-plan code converts it to `RefactorPlan`. Raw MCP code converts it to
the operator-facing dry-run/apply response.

## Documentation Requirements

The refactor runbook should say:

```md
Use `move_text_slice` only for literal block relocation. Prefer semantic
language-specific move/extract tools when binding updates, imports, visibility,
or caller rewrites are required.
```

Refactor atoms should be allowed to use this primitive for bounded mechanical
movement, but should not use it to dodge semantic plan kinds.

## Required Tests

Minimum refactor-plan test matrix:

- `move_text_slice` cross-file plan shape and apply;
- `copy_text_slice`, `delete_text_slice`, and `replace_text_slice` plan shapes;
- same-file move up and move down;
- same-file `insert == start` and `insert == end` no-op refusals;
- same-file insertion inside selection refusal;
- marker not found and marker ambiguity refusal;
- exact-text ambiguity refusal;
- empty selection and range crossing EOF;
- single-line file and file without trailing newline;
- CRLF preservation;
- UTF-8 multibyte boundary validation for byte selectors;
- stale hash refusal through `bbox_refactor_apply`;
- dirty-worktree refusal through `bbox_refactor_apply`;
- cross-worktree refusal through `bbox_refactor_apply`;
- unsupported-language plan/apply with syntax-only status;
- supported-language parse-validation failure and rollback.

## Phases

1. Build the shared selector, insertion, preview, and same-file move engine.
2. Wire raw MCP `bbox_slice_read` / `bbox_slice_move` to prove
   external-agent value.
3. Add `move_text_slice` as a refactor plan kind.
4. Add `copy_text_slice`, `delete_text_slice`, and `replace_text_slice` plan
   kinds if raw usage shows demand.
5. Add AST-assisted selectors after the literal selector model is stable.

## Open Questions

- Should selected-text preview live in `RefactorPlan.leftovers`, or should
  `RefactorPlan` gain a first-class `previews` field?
- Should formatter command suggestions be emitted as optional next steps in
  `leftovers`, or left entirely to runbooks?
