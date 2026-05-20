---
title: "Context Clipboard Tools"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - surfaces
  - mcp
tags:
  - mcp
  - editing
  - refactor-tools
date: 2026-05-19
status: "archived"
brief: "Operator-facing MCP context-clipboard tools for guarded slice reads, moves, copies, deletions, and insertions."
---

# Context Clipboard Tools

## Problem

External coding agents routinely use shell text tools or provider-native edits
to move contiguous code blocks: tests into a new module, helper functions near
their caller, config stanzas into the right section, or chunks of docs between
files.

Blackbox already has guarded refactor plans for semantic moves, but external
MCP callers need a simpler operator-facing primitive for:

```text
take this source file range
cut or copy the exact text
insert it at this destination point
show me the preview
apply only after confirmation
```

The current alternatives all lose something important:

- `extract_rust_section` is Rust-only, top-level-item-only, and appends through
  Rust extraction machinery.
- `replace_text` and `write_file` can emulate the operation, but force the
  caller to reconstruct text manually.
- Shell `sed` / `awk` / `perl` edits are easy for agents to reach for, but they
  bypass Blackbox's previews, path checks, parse checks, rollback discipline, and
  provenance.

This surface should make the safe path easier than the shell path.

## Thesis

Add a small raw MCP tool family that behaves like a context clipboard for local
project files. The first implementation is not a semantic refactor system. It is
precise text-slice manipulation with strong guardrails and blunt tool
descriptions.

The adoption target is external Codex-style agents. The tool descriptions and
hotpath docs should say:

> Before using shell text tools or provider-native multi-edit to move a
> contiguous block between files, use `bbox_slice_move`.

Namespace: use `bbox_slice_*`, not `work_*`. In transcript-search, `work_*` is
reserved for tools exposed to agents operating inside workflows. These tools are
general Blackbox MCP tools for operator and external-agent use. The `bbox_slice`
prefix keeps the family grep-able and makes surface routing straightforward.

The raw MCP surface is independent of the user-facing `bbox_refactor_plan` /
`bbox_refactor_apply` workflow, but it must share the same underlying safety
engine wherever practical. Refactor-plan integration is a separate design:
[Context Clipboard Refactor
Primitives](../../refactor-tools/context-clipboard-refactor-primitives.md).

## Goals

- Provide one-call dry-run previews for moving, copying, inserting, replacing,
  and deleting contiguous line or marker-delimited slices.
- Preserve exact text by default. Do not ask the model to retype moved content.
- Require `confirm=true` for mutations.
- Guard writes with file hashes, dirty-worktree checks, path scope checks,
  cross-worktree checks, parse validation where language support exists, and
  rollback on failure.
- Return concise, agent-usable previews: selected text, insertion context,
  changed files, line deltas, hashes, parse status, and the exact confirm call.
- Emit provenance and instrumentation comparable to existing workspace/refactor
  surfaces so later analysis can see when agents choose this instead of shell.

## Non-Goals

- Do not resolve names, imports, callers, packages, or visibility.
- Do not infer architecture. This is text movement, not refactoring judgment.
- Do not store a long-lived clipboard in v1. Persistent clipboard save/paste is
  explicitly deferred to v2.
- Do not expose this as `work_*`.
- Do not silently reindent, format, deduplicate imports, or run formatters.

## Surface Routing

These tools are powerful enough that visibility matters.

Recommended default surface behavior:

- allow `bbox_slice_read` on the default MCP surface;
- deny mutating raw tools on the default surface:
  - `bbox_slice_move`
  - `bbox_slice_copy`
  - `bbox_slice_delete`
  - `bbox_slice_insert_text`
  - `bbox_slice_replace`
- expose mutating tools through an opt-in MCP surface such as
  `?surface=clipboard`, `?surface=editor`, or an operator-configured equivalent.

This prevents a silent permission expansion where ordinary external agents that
currently have only read/search tools suddenly gain Blackbox-backed mutation
tools because they connect to `/mcp`.

Refactor-persona brofiles and refactor atoms should also explicitly deny the raw
mutating `bbox_slice_*` tools. Refactor agents that need slice movement should
use the plan kind from the refactor-side design so the change flows through
`bbox_refactor_plan`, `bbox_refactor_apply`, and `bbox_refactor_run`.

## Tool Family

Use noun-first names so the catalog clusters cleanly.

### `bbox_slice_read`

Read and preview a resolved slice without mutating files.

This differs from `work_smart_read`: it resolves the exact selector model used
by the mutating clipboard tools, returns byte coordinates and parse status, and
provides a stable selection payload that can be reused in a move/copy/delete
operation. It is a slice resolver, not an enriched whole-file reader.

Use cases:

- Capture exact text before deciding where to move it.
- Show surrounding context for an operator review.
- Ask an agent to reason over a bounded block without reading the whole file.

Shape:

```json
{
  "file": "src/lib.rs",
  "project_dir": "/repo/x",
  "range": { "type": "lines", "start_line": 120, "end_line": 180 },
  "context_lines": 3
}
```

Response:

```json
{
  "status": "ok",
  "file": "/repo/x/src/lib.rs",
  "selection": {
    "start_line": 120,
    "end_line": 180,
    "byte_start": 4310,
    "byte_end": 6492,
    "line_count": 61,
    "text": "..."
  },
  "hashes": { "pre_sha256": "..." },
  "before_context": "...",
  "after_context": "...",
  "parse_status": "ok|has_error|unsupported",
  "warnings": []
}
```

### `bbox_slice_move`

Cut a selected slice from one file and insert it into another file, or another
position in the same file.

Dry-run is the default. Applying requires `confirm=true`.

Shape:

```json
{
  "project_dir": "/repo/x",
  "source": "src/foo.rs",
  "source_range": { "type": "lines", "start_line": 120, "end_line": 180 },
  "target": "src/foo_tests.rs",
  "insert": { "type": "after_marker", "marker": "mod tests {" },
  "confirm": false
}
```

Response:

```json
{
  "status": "dry_run",
  "operation": "move",
  "changed_files": ["src/foo.rs", "src/foo_tests.rs"],
  "selection_preview": "...",
  "target_preview": "...",
  "source_edit": { "delete_lines": [120, 180] },
  "target_edit": { "insert_line": 42, "position": "after_marker" },
  "validations": [
    { "file": "src/foo.rs", "parse": "ok" },
    { "file": "src/foo_tests.rs", "parse": "ok" }
  ],
  "confirm_call": {
    "tool": "bbox_slice_move",
    "arguments": {
      "...": "...",
      "confirm": true
    }
  }
}
```

### `bbox_slice_copy`

Same as `bbox_slice_move`, but leaves the source text intact.

Use copy when the source should intentionally remain. Prefer
`bbox_slice_move` for relocation; do not recommend copy-then-manual-delete as a
normal workflow because it loses one-operation rollback.

### `bbox_slice_delete`

Delete a selected slice with preview and confirmation.

This should beat ad hoc `sed -i '120,180d'` for agents because it returns the
exact deleted text, surrounding context, file hash, parse status, and a confirm
call.

### `bbox_slice_insert_text`

Insert caller-supplied text at a bounded insertion point.

This is not a generic `Write` replacement and does not insert a source slice.
It exists because the insertion selector model is stronger than exact-text
replacement anchors: line placement, literal markers with occurrence
disambiguation, prepend, and append.

### `bbox_slice_replace`

Replace a destination range with either caller-supplied text or a source slice.

Exactly one replacement source is allowed:

- `new_text`
- `source` plus `source_range`

Supplying both refuses with `error.replacement_ambiguous`. Supplying neither
refuses with `error.replacement_missing`.

## Selector Model

Selection and insertion inputs use explicit tagged JSON shapes. The Rust
implementation can use enums; the wire format should not rely on overlapping
optional keys.

Line range:

```json
{ "type": "lines", "start_line": 10, "end_line": 25 }
```

Marker range, exclusive by default:

```json
{
  "type": "markers",
  "start_marker": "<!-- start generated -->",
  "end_marker": "<!-- end generated -->",
  "include_markers": false
}
```

Exact text:

```json
{ "type": "exact_text", "text": "fn helper() {\n    ...\n}\n" }
```

Byte range:

```json
{ "type": "bytes", "start": 4310, "end": 6492 }
```

Insertion inputs:

```json
{ "type": "line", "line": 40, "placement": "before" }
{ "type": "line", "line": 40, "placement": "after" }
{ "type": "before_marker", "marker": "# tests" }
{ "type": "after_marker", "marker": "mod tests {" }
{ "type": "append" }
{ "type": "prepend" }
```

Markers are exact literal strings in v1. Regex and fuzzy/whitespace-tolerant
markers are deferred to v2. A marker is ambiguous when it has more than one
exact, non-overlapping match in the target file. Ambiguity refuses unless the
caller supplies an occurrence index:

```json
{ "type": "after_marker", "marker": "mod tests {", "occurrence": 2 }
```

`error.marker_ambiguous` payloads should include `match_count` plus candidate
line and byte positions so the caller can choose an occurrence without re-reading
the file.

Line numbers are 1-based and inclusive for source ranges. Line slicing preserves
the file's existing newline bytes: LF stays LF, CRLF stays CRLF, and a final line
without a trailing newline is selected without fabricating one. Byte selectors
must validate UTF-8 character boundaries before converting to `String`.

## Safety Semantics

Every mutating tool should follow the same envelope:

1. Resolve `project_dir`, source, and target paths.
2. Refuse paths outside the project unless `allow_unregistered_paths=true` is
   explicitly supplied.
3. Refuse source and target paths in different git worktrees by default; allow
   `force_path=true` only when the operator explicitly wants original paths
   despite the mismatch.
4. Refuse dirty git files by default. Add `allow_dirty_worktree=true` for
   deliberate uncommitted edits.
5. Read source and target bytes and record pre-edit SHA-256 hashes.
6. Resolve selection and insertion to byte ranges.
7. Validate non-empty selections for move/copy/delete/replace.
8. Validate same-file moves after accounting for deletion before insertion.
9. Construct an edit plan in memory.
10. Dry-run by default with previews and confirm call.
11. On `confirm=true`, re-read and verify hashes, apply via the shared snapshot
    and rollback machinery, parse-check supported files, and restore snapshots
    if any write or validation fails.

The raw MCP path should reuse the same safety helpers that power the refactor
envelope wherever possible, rather than reimplementing a parallel transaction
system. In implementation terms, reuse or factor out the equivalents of:
`ensure_path_in_registered_project`, dirty-file checks, `sha256_hex`,
parse-validation construction, rewritten-file validation, file snapshots, and
snapshot restore.

Parse validation policy is fixed for v1:

- supported source file plus post-edit parse error: hard refusal / rollback;
- unsupported file type: soft warning, with hash and rollback checks still
  active.

## Same-File Move Rules

Same-file moves are common and easy to get wrong by hand. Define behavior in
byte coordinates after resolving the selection to `[start, end)`.

| Insertion byte | Behavior |
|---|---|
| `insert < start` | Allowed. Delete original range, insert selected text at `insert`. |
| `insert == start` | Refuse with `error.same_file_noop`. |
| `start < insert < end` | Refuse with `error.insert_inside_selection`. |
| `insert == end` | Refuse with `error.same_file_noop`. |
| `insert > end` | Allowed. Delete original range, then subtract `end - start` from insertion byte before inserting. |

Preview should show the final post-edit context, not just independent delete and
insert hunks.

## Response Shape

All tools should return a typed response with enough signal for agents to trust
the tool and for humans to review it quickly.

Common fields:

```json
{
  "status": "dry_run|applied|blocked|error",
  "operation": "move|copy|delete|insert_text|replace|read",
  "project_dir": "/repo/x",
  "changed_files": [],
  "source": { "path": "...", "range": {}, "preview": "..." },
  "target": { "path": "...", "insert": {}, "preview": "..." },
  "hashes": {
    "source_pre_sha256": "...",
    "source_post_sha256": "...",
    "target_pre_sha256": "...",
    "target_post_sha256": "..."
  },
  "validations": [],
  "warnings": [],
  "confirm_call": null
}
```

Dry-run responses omit post hashes or set them to the projected post-edit hash
with an explicit `projected=true` marker. Applied responses must include actual
post-edit hashes.

Errors should use stable codes:

- `error.range_empty`
- `error.range_out_of_bounds`
- `error.marker_not_found`
- `error.marker_ambiguous`
- `error.same_file_noop`
- `error.insert_inside_selection`
- `error.dirty_worktree`
- `error.cross_worktree_apply`
- `error.path_outside_project`
- `error.hash_mismatch`
- `error.parse_validation_failed`
- `error.replacement_ambiguous`
- `error.replacement_missing`

## Hotpath Documentation

Tool descriptions and rendered docs should be intentionally blunt. External
agents do not infer the intended preference from a quiet catalog entry.

Suggested tool description for `bbox_slice_move`:

> Move a contiguous text slice between files, or within one file, with dry-run
> preview and confirm-before-write guardrails. This is literal text movement,
> not semantic refactoring.

Suggested hotpath docs entry:

```md
Before using shell text tools or provider-native edits to move a contiguous
block, call `bbox_slice_read` or `bbox_slice_move`. These tools preserve exact
text, show preview context, hash-check files, parse-check supported languages,
and require `confirm=true` before writing.
```

## Instrumentation

Each mutating call should emit a structured tool-call record into the existing
transcript/tool-call indexing path with:

- operation kind;
- source path and range;
- target path and insertion point;
- dry-run vs applied;
- bytes moved/copied/deleted/inserted;
- validation result;
- whether the caller used `allow_dirty_worktree` or `force_path`.

Mutating calls should also emit tool-call anchors compatible with `bbox_blame`,
matching the provenance behavior expected from `bbox_refactor_apply`.

This makes adoption measurable. The useful question is:

```text
Are external agents using bbox_slice_move instead of shell for block moves?
```

That requires both slice-tool events and a shell-edit baseline detector for
commands such as `sed -i`, `awk`, `perl -pi`, and shell redirection rewrites.
Building or documenting that detector is a prerequisite for a meaningful
adoption dashboard.

## Required Tests

Minimum MCP-surface test matrix:

- dry-run preview accuracy for cross-file move;
- confirm/apply roundtrip for cross-file move touching both files;
- same-file move up and move down;
- same-file `insert == start` and `insert == end` no-op refusals;
- same-file insertion inside selection refusal;
- marker not found and marker ambiguity refusal, including ambiguity payload
  candidate positions;
- exact-text ambiguity refusal;
- empty selection and range crossing EOF;
- single-line file and file without trailing newline;
- CRLF preservation;
- UTF-8 multibyte boundary validation for byte selectors;
- dirty-worktree refusal;
- hash mismatch between dry-run and confirm;
- cross-worktree refusal;
- unsupported-language soft warning;
- supported-language hard refusal and rollback on broken parse;
- rollback when the second file in a multi-file apply fails.

## Implementation Notes

Likely home:

- MCP adapters: `src/tools/` with other tool-facing surfaces.
- Shared slice resolver: see the refactor-side design for reusable internal
  selector structs and the future `bbox_refactor_plan` bridge.
- Safety helpers: factor the refactor envelope's path, hash, parse-validation,
  snapshot, and restore helpers so the raw MCP path and plan path do not drift.

## Open Questions

- Should v2 include `bbox_clipboard_save` / `bbox_clipboard_paste`, or should
  the surface remain stateless indefinitely?
- Should AST-derived insertion points live in this raw MCP surface, or only in
  the refactor-plan surface after explicit selector design?

## Phases

1. Build the shared selector, insertion, preview, and same-file move engine.
2. Ship `bbox_slice_read` and `bbox_slice_move` on top of the shared engine with
   literal line/marker selectors, dry-run default, confirm apply, same-file move
   support, and hotpath docs.
3. Add `bbox_slice_copy`, `bbox_slice_delete`, `bbox_slice_insert_text`, and
   `bbox_slice_replace`.
4. Add shell-vs-slice adoption instrumentation and reports.
5. Consider persistent clipboard operations only after the literal surface
   proves adoption.
