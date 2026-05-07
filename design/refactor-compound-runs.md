# Refactor Compound Runs

Date: 2026-05-07
Status: proposal

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

- `bbox_refactor_run`: dry-run or confirmed execution
- `bbox_refactor_run_status`: inspect a saved run record
- `bbox_refactor_run_resume`: amend a failed run with additional steps

The runner should not replace primitive plans. It should compose them and
preserve each step's reviewability. The output is a compound plan with step
metadata, merged file edits, expected touched files, rollback scope, and a
diagnostic trail.

## Execution Model

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

The MCP primitive tools can keep reading from disk. The compound runner uses a
projected provider that layers prior step outputs over disk. This avoids
teaching every primitive planner about transactions while still making them
composable.

Edits should be composed sequentially, not flattened by naive concatenation.
Step N's byte offsets are valid against the projected text produced by steps
1..N-1. V1 should preserve per-step `FileEdit`s and apply them in order. A
later optimizer can rebase edits back to original coordinates, but the first
correct model is sequential application.

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

The first implementation can be strict: all primitive plans must be Rust plans,
and all writes must remain under either registered projects or
`allow_unregistered_paths=true` practice roots.

## Transaction Semantics

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

Compound runs do not commit. They leave a validated worktree diff for the user
or orchestrator to commit at an explicit milestone.

## Symbolic Rename

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

A run record stores the input steps, starting git commit, project root, final
status, structured diagnostics, and the validated diff on success. Temporary
worktrees can be deleted after success/failure. Run metadata should default to a
7-day TTL, with successful validated diffs retained until either replayed or the
TTL expires.

## MVP

The first useful version does not need LSP. It should implement:

- `bbox_refactor_run` with `dry_run`, `confirm`, `allow_dirty_worktree`, and
  `allow_unregistered_paths`
- primitive-plan steps for existing plan kinds
- projected filesystem planning
- sequential per-step edit composition
- temporary-worktree execution and validated diff replay
- no resume yet
- no diagnostic-to-repair suggestions yet

The benchmark fixtures for compound runs should be scenarios, not isolated
single-op tests. A useful fixture exercises at least two primitive plan steps so
projection, sequential edit composition, and rollback all have something real to
prove.

Then add:

- generic validation/profile surface with declared read/write sets
- rust-analyzer-backed `lsp_rename`
- rust-analyzer-backed import repair
- language-specific diagnostic parsing into structured diagnostics

## Open Questions

- How much rust-analyzer lifecycle should the daemon own versus shelling out to
  an adapter process?
- Should import repair be a required semantic gate, or an optional fixup phase
  after `cargo check` reports unresolved imports?
- Should `bbox_refactor_run_resume` wait until long-running LSP/import repair
  runs exist, or is a step-amend/resubmit loop useful in V1?
