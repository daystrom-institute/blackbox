+++
title = "Macros — composable synthesis recipes that lower to refactor plans"
tags = ["macros", "macro", "macro_list", "macro_describe", "macro_plan", "macro_apply", "macro_run", "macro_register", "recipe", "codemod", "synthesis-plan", "refactor-plan", "authority-gates", "builtin"]
order = 14
template = false
+++
# Macros — Composable Synthesis Recipes

Use this memory when choosing, planning, applying, or authoring a macro. For a
one-off edit, the refactor tools or an atom are usually enough; do not pull this
just because another system memory signposts a macro.

## What a Macro Is

A macro is a **data-only synthesis plan** stored as JSON — a named, parameterized
recipe that composes refactor operations (`rewrite`, `for_each`,
`delegate_refactor`, `emit`, `record`) into one reviewable transformation. A
macro does not edit directly: `macro_plan` lowers it to a read-only `MacroPlan`,
and `macro_apply` lowers that to a guarded `RefactorPlan` and writes. This is the
complement to the refactor tools (`sm-refactor`): refactor primitives and atoms
are the edits; a macro orchestrates them for a recurring, often framework-shaped
pattern.

System memory must not mirror the macro catalog. Macro ids, titles, inputs, and
operations live in the macro JSON files and are discovered live:

```text
macro_list(project_dir="<repo>")
macro_describe(id="<id>", project_dir="<repo>")
```

## Scopes

| Scope   | Location           | Reviewable like     |
|---------|--------------------|---------------------|
| project | `.bbox/macros/`    | source (committed)  |
| user    | operator config    | host-local          |
| builtin | ships with Blackbox | the distribution   |

## Plan → Apply Discipline

Macros are read-before-write by construction:

```text
macro_plan(id="<id>", args={...}, project_dir="<repo>")   # read-only MacroPlan
macro_apply(plan=..., confirm=true)                        # lowers to RefactorPlan, writes
macro_run(id="<id>", args={...}, confirm=true)             # plan + apply in one step
```

`macro_plan` never writes. `macro_apply` / `macro_run` write only with
`confirm=true`. A macro carries its own `authority_gates`, `effects`, `probes`,
`validations`, and `refusals` — honor them; they are part of the contract, not
advisory.

## Relationship to Atoms and Refactor

- A **refactor tool** is a single guarded structural edit.
- A **macro** is a data-only recipe composing refactor operations for a
  recurring pattern — often framework-shaped (annotation or binding codemods).
- Atom wrappers and daemon macro MCP tools are retired. Inspect the current
  native harness catalog for available transformations and compose them in the caller.

Reach for a macro when a transformation recurs with the same shape and you want a
reviewable plan before any bytes change.
