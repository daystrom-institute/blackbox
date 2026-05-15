---
title: "Refactor Compound Runs"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - refactor-tools
date: 2026-05-07
status: "partially implemented; updated 2026-05-13"
brief: "Design and implementation status for transactional multi-step refactor runs with command validation and rollback."
---

# Refactor Compound Runs

## Implementation Status

The core compound runner is shipped. Several sections below describe features
that are implemented differently or not yet built. This status table summarizes
the gaps; details are inline in each section.

| Feature | Status | Notes |
|---|---|---|
| `bbox_refactor_run` tool | **Done** | `src/refactor/mod.rs:1665`, `src/tools/refactor.rs:60` |
| `RefactorRunStep` enum (Plan + Command) | **Done** | `src/refactor/mod.rs:618`; Command extended beyond design with `capture`, `on_failure`, `touches` |
| `dry_run` / `confirm` modes | **Done** (single mode) | Single `confirm` flag on input, `dry_run` computed on response. No projected dry-run planning. |
| Structured diagnostics | **Done** | `RustcDiagnostic`, `CapturedDiagnosticsSummary`, `ParseValidationResult`, `ObligationReport` |
| `allow_dirty_worktree` / `allow_unregistered_paths` | **Done** | `RefactorRunParams` fields |
| Transactional rollback (snapshot-based) | **Done** | File-level snapshots before each step, rollback on failure. In-place only, no temp worktree. |
| LSP rename | **Done** as plan kind | `rust_lsp_rename` plan kind (`src/refactor/rust.rs:1144`), not a dedicated step variant |
| Import handling | **Done** differently | `rust_organize_imports` (LSP), `rust_minimize_imports` (tree-sitter), `rust_compile_fix_round` (diagnostic-driven repair) — not the dedicated step variants below |
| Step expansion macros | **Done** (not in design) | `split_rust_impl_methods_to_submodule`, `migrate_rust_mods_to_lib`, `rust_minimize_imports` auto-expanded inside runs |
| Obligation/repair pipeline | **Done** (not in design) | `ContinueForRepair` on-failure mode opens obligations; later steps consume them. |
| `SourceProvider` trait / projected filesystem | **Not built** | Critical for accurate multi-step dry-run; currently each step plans against disk |
| `temp_worktree` execution mode | **Not built** | All runs in-place with snapshot rollback |
| `bbox_refactor_run_status` / `bbox_refactor_run_resume` | **Not built** | No run persistence; runs synchronous, return inline |
| Dedicated `LspRename` / `ImportRepair` step variants | **Not built** | Implemented as plan kinds instead. See discussion in Symbolic Rename section. |
| Run record persistence | **Not built** | No `~/.local/state/blackbox/runs/` |

## Problem

The current `bbox_refactor_*` tools can mechanize a real Rust extraction, but
the caller still has to manually sequence several small plans:

1. inspect source symbols
2. extract methods or items
3. add a module declaration
4. wire a generated router into a router sum
5. format
6. run `cargo check` or tests
7. repair imports, visibility, or call paths

That is already useful, but it is not enough for the "boom and bust" cycle we
want. During bust phases, agents need a higher-level transaction shape that can
pull apart a broad patch, integrate the moved pieces, run semantic gates, and
return a reviewable result without losing rollback discipline.

## Proposal

> **Implemented.** `bbox_refactor_run` is live with `title`, `project_dir`,
> `steps[]`, `confirm`, `allow_dirty_worktree`, `allow_unregistered_paths`.
> The example below is accurate to the shipped API shape.

Add a compound runner over existing primitive refactor plans:

```text
bbox_refactor_run(
  title="extract refactor tools",
  project_dir="/repo/x",
  dry_run=true,
  steps=[
    {
      "op": "plan",
      "kind": "extract_rust_impl_methods",
      "source": "src/main.rs",
      "target": "src/refactor_tools.rs",
      "item_names": [
        "bbox_refactor_status",
        "bbox_refactor_plan",
        "bbox_refactor_apply"
      ],
      "item_kinds": ["impl_method"],
      "impl_name": "impl BlackboxServer",
      "router_name": "refactor_tools",
      "router_export_name": "router",
      "target_prelude": "use super::*;"
    },
    {
      "op": "plan",
      "kind": "add_rust_mod_decl",
      "source": "src/main.rs",
      "module_name": "refactor_tools"
    },
    {
      "op": "plan",
      "kind": "add_rust_router_to_sum",
      "source": "src/main.rs",
      "router_call": "refactor_tools::router()"
    }
  ]
)
```

Recommendation: make this a separate MCP surface, not another
`bbox_refactor_plan(kind=...)` variant. Compound runs have lifecycle,
resumability, and diagnostics that are different from a single primitive plan.
Keep primitive plans small and pure; let the runner compose them.

This is about tool shape, not exposure policy. Tool exposure should follow the
planned MCP-surfaces model: the daemon can expose `bbox_refactor_run*` only on
appropriate named surfaces such as `refactor` or `ops`, while the default
surface can continue hiding `bbox_refactor_*` from ordinary spawned agents.
Compound runs should integrate with that filtering model instead of creating a
parallel registry. The surface policy itself is intentionally out of scope for
this design.

Initial tools:

- `bbox_refactor_run`: dry-run or confirmed execution **[done]**
- `bbox_refactor_run_status`: inspect a saved run record **[not built]**
- `bbox_refactor_run_resume`: amend a failed run with additional steps **[not built]**

The runner should not replace primitive plans. It should compose them and
preserve each step's reviewability. The output is a compound plan with step
metadata, merged file edits, expected touched files, rollback scope, and a
diagnostic trail.

## Execution Model

> **Partially implemented.** The shipped runner has `confirm=true`/`false` but
> does NOT have a projected filesystem. When `confirm=false`, each step is
> planned independently against the original disk state — step N does not see
> the projected output of steps 1..N-1. When `confirm=true`, steps are applied
> sequentially to disk, so later confirmed steps do see prior writes. The
> projected filesystem described below remains a key correctness gap for
> multi-step dry-run.

`bbox_refactor_run` has two modes:

- `dry_run=true`: generate every planned edit against an in-memory projected
  filesystem, do not write files, and report the merged plan.
- `confirm=true`: apply the whole run transactionally and rollback all touched
  files if a required step fails.

The projected filesystem matters. Step 2 must plan against the result of step 1,
not against stale disk. Without projection, multi-step runs force callers to
apply intermediate edits just to build later plans.

Primitive planners therefore need an internal overload that accepts a
`SourceProvider` instead of reading only from disk:

```rust
trait SourceProvider {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn exists(&self, path: &Path) -> bool;
}
```

> **Not implemented.** All primitive planners read from disk. The `SourceProvider`
> abstraction is the prerequisite for projected dry-run planning and should be
> the first piece built if this section is pursued.

The MCP primitive tools can keep reading from disk. The compound runner uses a
projected provider that layers prior step outputs over disk. This avoids
teaching every primitive planner about transactions while still making them
composable.

Edits should be composed sequentially, not flattened by naive concatenation.
Step N's byte offsets are valid against the projected text produced by steps
1..N-1. V1 should preserve per-step `FileEdit`s and apply them in order. A
later optimizer can rebase edits back to original coordinates, but the first
correct model is sequential application.

> **Implemented differently for confirmed runs.** The shipped runner applies
> each step's edits to disk immediately after planning, then snapshots for
> rollback. Sequential composition is implicit via the live filesystem. For
> dry-run, edits are collected per-step but planned against original disk.

Minimum internal model:

```rust
struct RefactorRun {
    title: String,
    project_dir: PathBuf,
    steps: Vec<RunStep>,
    validations: Vec<ValidationStep>,
}

enum RunStep {
    PrimitivePlan(RefactorPlanParams),
    Command { argv: Vec<String>, required: bool },
    LspRename(RenameParams),
    ImportRepair(ImportRepairParams),
}
```

> **Shipped shape differs.** The actual types are:
> - `RefactorRunParams` (input: `src/refactor/mod.rs:562`) — no `validations` field
> - `RefactorRunStep` enum (lines 618–660) — two variants: `Plan { params, optional }` and `Command { command, args, cwd, touches, required, capture, on_failure }`. The Command variant is richer than the design: `CaptureSpec` enables `cargo --message-format=json` parsing, `OnFailure` adds `ContinueForRepair` beyond the design's boolean `required`, and `touches` declares files the command may mutate for rollback.
> - `LspRename` and `ImportRepair` are NOT dedicated variants; they are plan kinds dispatched through `RefactorRunStep::Plan`. See discussion in Symbolic Rename and Import Repair sections.

```rust
struct RefactorRunPlan {
    title: String,
    semantic_status: SemanticStatus,
    steps: Vec<RunStepReport>,
    edits: Vec<FileEdit>,
    validations: Vec<ValidationStep>,
    touched_files: Vec<PathBuf>,
    rollback_scope: Vec<PathBuf>,
}
```

> **Shipped as `RefactorRunResponse`** (`src/refactor/mod.rs:877`): combines run-level
> status with step reports. Fields: `status`, `title`, `dry_run`, `steps`, `files_written`,
> `rolled_back`, `error`, `rollback_errors`, `obligations`. Step reports are
> `RefactorRunStepReport` (line 895): `index`, `op`, `status`, `kind`, `title`, `files`,
> `validations`, `error`, `captured_diagnostics_summary`.

The first implementation can be strict: all primitive plans must be Rust plans,
and all writes must remain under either registered projects or
`allow_unregistered_paths=true` practice roots.

## Transaction Semantics

> **Implemented differently.** The shipped runner uses in-place execution with
> file-level snapshots for rollback, not the two-phase temp-worktree model
> described below. Each confirmed step: (1) snapshots files it will touch,
> (2) plans and applies writes, (3) parse-validates written files. On any
> required-step failure, all prior snapshots are restored in reverse order.
>
> This works but has a narrower safety guarantee: if the rollback itself fails
> (e.g. a snapshot restore hits a permissions error), the tree can be left in a
> partial state. The temp-worktree model below would avoid this by never mutating
> the caller's tree until a fully validated diff exists.
>
> The temp-worktree model remains the target for high-confidence autonomous
> operation, especially when LSP or semantic tool steps are involved.

The compound runner should use a two-phase apply in a temporary worktree by
default:

1. Build phase:
   - resolve all paths
   - create a sibling or `/tmp` git worktree from the starting commit
   - generate primitive plans against a projected filesystem
   - apply each step sequentially inside the temporary worktree
   - reject overlapping edits within each step
   - record original hashes

2. Apply phase:
   - run parse validation and any generic validation/profile steps attached by
     future surfaces inside the temporary worktree
   - if any required gate fails, delete the temporary worktree and return the
     structured failure
   - if all gates pass, compute the validated diff from the temporary worktree
   - replay that diff into the caller worktree with normal path-scope, hash, and
     dirty-file checks
   - parse-validate changed supported source files in the caller worktree

This makes rollback mostly mechanical: failed runs delete the temporary
worktree, and the live tree is not mutated until a validated final diff exists.
It also gives language servers, formatters, compilers, and import fixers a real
filesystem instead of a partial in-memory projection when those generic
validation/profile surfaces exist.

Command execution should not be hardcoded into the generic compound runner.
Language memories can describe Rust `cargo`, TypeScript `npm`/`tsc`, C#
`dotnet`, Go `go test`, and similar validation profiles, but the generic runner
should only know how to compose refactor steps and transaction boundaries. A
future validation surface can attach command/profile steps with declared
read/write sets.

> **Command steps are implemented** with richer semantics than the design
> proposes. The shipped `RefactorRunStep::Command` has:
> - `capture: Option<CaptureSpec>` — parses `cargo --message-format=json` output into structured `RustcDiagnostic`s, stashed in a `RunCaptureContext` for downstream repair steps
> - `on_failure: Option<OnFailure>` — three modes: `Required` (rollback), `Optional` (continue), `ContinueForRepair` (open obligation, continue)
> - `touches: Vec<String>` — declared mutable paths snapshotted for rollback
>
> The design's concern about not hardcoding language-specific commands is
> respected: the runner executes generic shell commands. The `CaptureSpec` enum
> is the only language-aware piece, and it is open to future variants (clippy
> JSON, miri, etc.).

Compound runs do not commit. They leave a validated worktree diff for the user
or orchestrator to commit at an explicit milestone.

## Symbolic Rename

> **Implemented as plan kind `rust_lsp_rename`**, not as a dedicated
> `RefactorRunStep` variant. The implementation (`src/refactor/rust.rs:1144`)
> uses rust-analyzer via LSP `textDocument/rename`, converts `WorkspaceEdit`
> into `FileEdit`s, and sets `semantic_status: LspVerified`. Tested at
> `src/refactor/tests.rs:3346`.
>
> The agent-facing API uses `item_names`/`old_text`/`new_text` on
> `RefactorPlanParams` instead of the `position`/`new_name` selectors below.
> The workspace-edit scoping rules and empty-edit failure semantics described
> below are partially implemented — the LSP rename does scope edits and rejects
> out-of-project changes.
>
> **Should this become a dedicated step variant?** The current plan-kind
> approach works but has a latent scheduling constraint that matters if projected
> filesystem or temp worktree mode lands. Structural tree-sitter plans can
> operate against in-memory projected text; LSP rename needs a running language
> server seeing real files (or a temp worktree). A dedicated `LspRename` variant
> could encode "I need a real filesystem" as a typed scheduling hint, which the
> generic plan-kind dispatch can't express cleanly. Recommendation: keep the
> plan-kind approach until projected FS or temp worktree is implemented, then
> extract into a first-class variant at that point.

Tree-sitter should not own semantic rename. Add an LSP-backed step type:

```text
{
  "op": "lsp_rename",
  "language": "rust",
  "file": "src/refactor_tools.rs",
  "position": { "line": 3, "character": 18 },
  "new_name": "tool_router",
  "required": true
}
```

For Rust, the backend should use rust-analyzer via LSP:

1. start or reuse a rust-analyzer server for `project_dir`
2. open the current projected file contents, including newly-created files
3. request `textDocument/prepareRename`
4. request `textDocument/rename`
5. convert the returned `WorkspaceEdit` into `FileEdit`s
6. mark the run `semantic_status = "lsp_verified"`
7. require `cargo check` or targeted tests after apply

Projection is harder for LSP than for tree-sitter. rust-analyzer can reason
over unsaved open buffers, but file creation, module declarations, build script
state, and proc macro expansion can still lag. The safest ordering is:

- structural primitive steps are projected in memory
- LSP steps run after the projected files have been written into a temporary
  validation worktree
- the returned workspace edit is converted back into the compound run plan
- the original worktree is touched only during confirmed apply

Workspace edits must be scoped. If rust-analyzer returns edits outside
`project_dir`, outside registered project roots, under `target/`, or in vendored
paths, the run fails closed and reports the rejected ranges. A semantic rename
that cannot be fully scoped is not safe to apply.

An empty `WorkspaceEdit` is success only when the runner can prove the request
is a no-op before rename, such as `old_name == new_name` after `prepareRename`
resolves the symbol. Otherwise it is a failed semantic step: the runner should
report that the symbol resolved but produced no workspace edits and require the
caller to choose a different selector or backend.

Line/character positions are acceptable for low-level calls, but the agent-facing
shape should prefer a symbol selector when possible. The runner can resolve the
symbol to a position inside the temporary worktree immediately before calling
`prepareRename`, avoiding stale positions after earlier steps.

If the LSP cannot prepare the rename, the run must fail closed. A weaker
`rename_manifest` may be produced for human review, but it must not be applied
as a semantic rename.

## Import Repair and Optimization

> **Implemented differently.** The design's dedicated `rust_import_repair` and
> `rust_import_prune` step types are not built. Instead, three plan kinds cover
> the same surface area:
>
> - **`rust_organize_imports`** (`src/refactor/rust.rs:1210`): LSP-backed
>   `source.organizeImports` via rust-analyzer. Covers the "organize/optimize
>   imports" use case that `rust_import_prune` would have served.
> - **`rust_minimize_imports`** (`src/refactor/rust_minimize_imports.rs`):
>   Tree-sitter-based conservative wildcard-import replacement. Auto-expanded
>   as a follow-up step inside `bbox_refactor_run` when certain extraction plan
>   kinds are used.
> - **`rust_compile_fix_round`** (`src/refactor/rust_compile_fix.rs`):
>   Diagnostic-driven repair: classifies rustc diagnostics from `cargo check
>   --message-format=json` output into repair plans. This is the closest thing
>   to `rust_import_repair` — it reads compiler errors including unresolved
>   imports and proposes fixes. Integrated into the run loop as a repair hook
>   via the `ContinueForRepair` on-failure mode and obligation pipeline.
>
> The same scheduling-constraint argument from Symbolic Rename applies here:
> these operations need a real filesystem (or temp worktree) for the LSP and
> compiler to operate correctly. Dedicated step variants would encode that
> constraint, but the plan-kind approach is adequate until projected FS or temp
> worktree mode lands.

Import repair should also be semantic-backed. Tree-sitter can remove exact
syntactic imports, but it cannot know the canonical path for a moved symbol.

Rust V1 should support two import steps:

```text
{ "op": "rust_import_repair", "mode": "rust_analyzer", "required": true }
{ "op": "rust_import_prune", "mode": "cargo_fix_or_rust_analyzer", "required": false }
```

`rust_import_repair` should prefer rust-analyzer code actions:

- missing import quick fixes
- unresolved module path fixes
- visibility diagnostics when available

`rust_import_prune` can use one of:

- rust-analyzer organize imports
- `cargo fix --allow-dirty --allow-staged` in a disposable/project-scoped
  transaction
- compiler diagnostics for unused imports, converted into surgical deletion
  plans only when ranges are exact

Every import edit becomes a normal `FileEdit` with hash checks and parse
validation. If the language backend cannot produce exact ranges, it reports a
follow-up instead of editing.

## Diagnostics Loop

> **Largely implemented.** Structured diagnostics are emitted via
> `RustcDiagnostic` (parsed from `cargo --message-format=json`), per-step
> `CapturedDiagnosticsSummary`, and `ParseValidationResult`. Full diagnostic
> bodies are intentionally omitted from MCP responses (size budget) — only
> aggregate counts surface in summaries.
>
> The obligation/repair pipeline (`ContinueForRepair` + `ObligationReport`)
> provides the autonomous repair bridge the design describes: a soft-failed
> command step opens a repair obligation, and a later `rust_compile_fix_round`
> step can consume it. This is richer than the design's "V1 should stop at
> structured diagnostics" baseline.
>
> **Not implemented:** `bbox_refactor_run_resume`. Runs are synchronous with no
> persistence. The resumability described below — starting from original pre-run
> state with amended steps — remains unbuilt.

Compound runs should emit structured diagnostics, not only command text:

```json
{
  "status": "validation_failed",
  "failed_step": 5,
  "diagnostics": [
    {
      "source": "cargo_check",
      "file": "src/main.rs",
      "line": 358,
      "code": "E0624",
      "message": "associated function is private",
      "notes": ["candidate repair planning intentionally deferred"]
    }
  ]
}
```

This is the bridge to autonomous repair. A failed run should be resumable:

```text
bbox_refactor_run_resume(run_id="run-...", add_steps=[...])
```

Resume starts from the original pre-run state plus the accepted amended step
list, not from a half-mutated tree.

V1 should stop at structured diagnostics. Mapping compiler errors to concrete
repair steps should be a later opt-in engine; premature suggestions can burn
agent loops.

## Temporary Worktree Mode

> **Not implemented.** All runs execute in-place with snapshot-based rollback.
> The two-phase model below remains the target for high-confidence autonomous
> operation. The `execution_mode` parameter does not exist on
> `RefactorRunParams`.

For high-confidence autonomy, compound runs should default to:

```text
execution_mode="temp_worktree"
```

In this mode the runner creates a sibling or `/tmp` worktree at the starting
commit, applies the compound run there first, runs all mutating commands and
semantic tools there, and only then replays the final merged edits into the
caller worktree. This is slower, but it gives the LSP, cargo, formatters, and
import fixers a real filesystem without risking half-applied edits in the
operator's current tree.

An `execution_mode="in_place"` fallback can exist for tiny tests and trusted
operator workflows, but autonomous restructuring should prefer temporary
worktrees whenever a run contains command, LSP, or import-repair steps.

Run records live under the daemon state directory:

```text
~/.local/state/blackbox/runs/<run-id>/
```

> **Not implemented.** No run records are persisted. The `plan_slot.rs` module
> references `refactor/runs/` as a future path but the directory and persistence
> layer do not exist.

A run record stores the input steps, starting git commit, project root, final
status, structured diagnostics, and the validated diff on success. Temporary
worktrees can be deleted after success/failure. Run metadata should default to a
7-day TTL, with successful validated diffs retained until either replayed or the
TTL expires.

## MVP

> **The MVP is shipped.** Items marked with [x] are done; [ ] remain.

The first useful version does not need LSP. It should implement:

- [x] `bbox_refactor_run` with `dry_run`, `confirm`, `allow_dirty_worktree`, and
  `allow_unregistered_paths`
- [x] primitive-plan steps for existing plan kinds
- [ ] projected filesystem planning
- [x] sequential per-step edit composition (via in-place disk writes for confirmed runs)
- [ ] temporary-worktree execution and validated diff replay
- [x] no resume yet
- [x] no diagnostic-to-repair suggestions yet (but the obligation pipeline goes further)

The benchmark fixtures for compound runs should be scenarios, not isolated
single-op tests. A useful fixture exercises at least two primitive plan steps so
projection, sequential edit composition, and rollback all have something real to
prove.

Then add:

- [ ] generic validation/profile surface with declared read/write sets
- [x] rust-analyzer-backed `lsp_rename` (as plan kind `rust_lsp_rename`)
- [x] rust-analyzer-backed import repair (as `rust_organize_imports` + `rust_compile_fix_round` plan kinds)
- [x] language-specific diagnostic parsing into structured diagnostics

## Open Questions

- How much rust-analyzer lifecycle should the daemon own versus shelling out to
  an adapter process?
- Should import repair be a required semantic gate, or an optional fixup phase
  after `cargo check` reports unresolved imports?
- Should `bbox_refactor_run_resume` wait until long-running LSP/import repair
  runs exist, or is a step-amend/resubmit loop useful in V1?
