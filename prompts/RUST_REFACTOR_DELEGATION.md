---
title: "Delegating Rust Structural Refactoring"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - refactor-tools
  - orchestration
brief: "Orchestrator playbook for delegating Rust structural refactoring to a code-mode agent that composes analysis.*, rust.*, code.*, lsp.*, edits.*, and build.gate, with the compiler repair loop and Rust-specific limitations made explicit."
---

# Delegating Rust Structural Refactoring

You are an **orchestrator**. Dispatch a coding agent to drive the harness-native
Rust isolate bindings inside code-mode cells, then independently verify the
result. Do not brief it to use the retired daemon refactor MCP surface.

Use this playbook for module extraction, large impl decomposition, state
movement, trait extraction, type or error migration, import minimization, and
semantic rust-analyzer operations. A trivial local edit does not need this
machinery.

The binding trust model lives in
`crates/bro-harness/src/bindings/AGENTS.md`. The Rust surface design is
`design/refactor-tools/rust/rust-isolate-surface.md`.

## Toolbox

The dispatched agent runs in code-mode and composes:

- `code.*` for hash-anchored syntax facts and edit addresses.
- `analysis.topLevelDeps` before top-level extraction.
- `analysis.implPartition` before impl decomposition, trait extraction, or
  state movement.
- `rust.describe` before the first use of each Rust transform.
- `rust.*` transforms for structural planning and synthesis.
- `lsp.references`, `lsp.assist`, `lsp.definition`, and `lsp.rename` for
  rust-analyzer authority.
- `edits.*` as the only mutation path.
- `build.gate` for structured cargo diagnostics.
- `rust.fixRound` for compiler-guided mechanical repair.

Point the agent at runtime contracts instead of pasting every parameter into the
brief.

## Canonical fix loop

Every structural dispatch uses this loop:

```text
rust.<transform>
  -> edits.apply
  -> build.gate("cargo check --message-format=json")
  -> rust.fixRound
  -> edits.merge / edits.apply
  -> build.gate
```

Repeat until the gate succeeds, no mechanical fixes remain, or about five
rounds have run. `rust.fixRound.leftovers` is the manual punch list. Borrow,
move, trait-bound, and unclassified errors must not be retried blindly.

Tell the agent to return the terminal gate, repair-round count, and every
leftover. A partially repaired tree with hidden leftovers is not complete.

## Briefing the dispatched agent

A good brief names the target, goal, allowed boundary, and compiler command.
Include these guardrails:

- Consult `rust.describe` and the relevant `analysis.describe` contract before
  first use.
- Ground every item name and span with `code.*`.
- Use `analysis.*` reductions for dependency or partition decisions.
- Make every write through `edits.apply`.
- Re-derive spans after every apply.
- Run the fix loop with a hard cap of about five rounds.
- Return all findings, skipped locations, unrewriteable accessors, warnings,
  and `rust.fixRound.leftovers`.
- Treat a target-exists refusal after a successful extract as DONE.
- Never pass `acknowledge_repr` or `acknowledge_public_api_change` in a cell.
  Those flags can only arrive from operator-supplied dispatch defaults.
- Keep LSP WorkspaceEdits and compiler `MachineApplicable` replacements
  unmodified so their provenance survives.

For RX-V1 work, the operator grants authority at dispatch time through
`ToolArgDefaults`. The binding refusal names the required default:

```text
default:rust.moveStructFields.acknowledge_repr=true
default:rust.migrateErrorType.acknowledge_public_api_change=true
default:rust.migrateTypeUsages.acknowledge_public_api_change=true
```

The agent may consume a supplied default but must never infer, add, or silently
retry with one.

## Dispatch shape

- Use a capable coding model in code-mode.
- Dispatch into an isolated worktree or lane checkout.
- Give the exact project compiler command and feature flags.
- For a large workspace that needs `lsp.*`, use a warm checkout/build-data
  cache or explicitly raise `wait_ready_ms`.
- Keep one concern per dispatch. A large decomposition is a sequence of fresh
  surveys and focused transformations.
- Ask for structured completion data, for example:

```json
{
  "transform": "rust.extractImplMethods",
  "items": ["method_a", "method_b"],
  "files_touched": ["src/a.rs", "src/a/part.rs"],
  "applied": true,
  "semantic_status": "syntax_only",
  "repair_rounds": 1,
  "compiler_gate_ok": true,
  "leftovers": [],
  "findings": [],
  "summary": "Moved the selected impl role into the child module."
}
```

## Rust-specific limitations

Pre-empt these in the brief.

### Import minimization

`rust.organizeImports` only implements `mode: "minimize"`.

- It is in-project only.
- It assumes file or `mod.rs` geometry.
- It cannot descend into inline modules.

A real failure shape is a wildcard import such as
`use crate::orchestration::providers::dispatch_prelude::*` when
`dispatch_prelude` is an inline module inside the file module `providers.rs`.
The minimizer cannot resolve that inline child. Preserve the wildcard and
report the finding instead of guessing explicit names.

### Position-sensitive LSP spans

`lsp.*` requests are aimed by hash-anchored byte spans. The byte position is
part of the request. After any edit, old spans are stale even when the same
identifier still exists. Re-run `code.items`, `code.query`, or
`code.readLines`, then use `code.read` on the fresh span before the next LSP
call.

### Struct-literal goto-definition

Rust-analyzer cannot reliably goto-definition for a struct literal's type from
the literal site. This is rust-analyzer behavior, not a binding bug. Aim
`lsp.definition` at a declaration, import, signature, or another semantically
resolved occurrence.

### `rust.liftToFree`

`rust.liftToFree` refuses methods with `self.field` access using
`method_lift_refused`. Most methods on state containers are therefore poor
candidates. Pick helpers that operate only on arguments, such as parsers,
normalizers, formatters, and calculations.

The transform does not rewrite call sites and only searches the first inherent
impl in the source. Compile immediately after applying it.

### Call-site scanners

The v1 scanner code behind the ported planners recognizes UFCS-style sites:

```rust
Type::method(...)
<Type as Trait>::method(...)
```

It does not track ordinary receiver calls:

```rust
value.method(...)
```

Use `lsp.references` before the move and the compiler gate after it. Do not
interpret an empty planner call-site warning list as proof that receiver calls
do not exist.

### Rust-analyzer readiness

Cold rust-analyzer startup on a large workspace can exceed the default
readiness budget. Use a warm checkout or build-data cache, or raise
`wait_ready_ms`. Re-running isolated one-shot probes against a cold root can
repay the full index cost each time.

## Common delegated flows

### Monster-file split

1. `analysis.topLevelDeps`
2. choose one coherent cluster
3. `rust.extractItems`
4. apply changes and creates
5. compiler fix loop

### God-impl split

1. `analysis.implPartition`
2. choose one role whose methods share calls or state
3. `rust.extractImplMethods`
4. `rust.moduleWiring` when explicit graph wiring remains
5. apply
6. compiler fix loop

### Trait extraction

1. `analysis.implPartition`
2. `lsp.references` on selected methods
3. `rust.extractTrait`
4. inspect object-safety and trait-import findings
5. apply
6. add required trait imports through fresh spans
7. compiler fix loop

### State extraction

1. `analysis.implPartition`
2. `rust.moveStructFields`
3. apply
4. add delegate field and constructor wiring with anchored `edits.*`
5. `rust.updateCallers`
6. apply
7. compiler fix loop

There is no live Rust transform for arbitrary delegate construction. The brief
must call out that small manual anchored-edit step.

### Error migration

1. obtain the explicit dispatch-side RX-V1 grant when public API changes are
   authorized
2. `rust.migrateErrorType`
3. apply
4. compiler fix loop
5. resolve `?`-site leftovers manually

### Test island extraction

Use `rust.inlineModToFile` for an inline `#[cfg(test)] mod tests`. Use
`rust.extractItems` for loose test helpers or test items. Compile the relevant
targets after applying.

## Orchestrator verify loop

The agent's final JSON is a claim. Verify it:

1. Re-run the exact compiler gate on the resulting ref or worktree.
2. Inspect the diff for the intended ownership and module boundary.
3. Confirm every created file is wired into the module graph.
4. Check `semantic_status`, `operator_opt_outs_used`, findings, and leftovers.
5. Use `lsp.references` or targeted code facts to inspect call-site fallout.
6. If the agent hit substrate friction, fix the binding or contract before
   repeating the same task.
7. Run `prompts/RETRO_ISOLATE_REFACTOR.md` after a representative live probe.

## Before any commit

This is a public repo. Genericize private client identifiers out of commit
messages, docs, notes, fixtures, and comments. Keep examples neutral.
