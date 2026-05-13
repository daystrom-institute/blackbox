# Atoms — Public Reusable Capability Contracts

Use this memory when you are choosing, invoking, authoring, or debugging atoms.
Do not pull it just because another system memory signposts a relevant atom;
for ordinary refactor work, `atom_search` / `atom_describe` is enough.

## Core Invariants

An atom is a public capability contract with a stable name, input schema,
effect limits, implementation binding, and trace handle. It is the right
surface when "run this capability" should mean the same thing regardless of
whether the backend is deterministic code, a provider persona, a workflow, or
an adapter.

System memory must not mirror the atom catalog. Active atom names, versions,
cost classes, prompts, and schemas live in manifests and artifact state. Use:

```text
atom_search(query="<intent phrase>")
atom_describe(atom="atom:<name>@latest")
atom_list(subcontract="<domain/vN>")
bbox_artifact_list(kind="atom", name="<optional name>")
```

## Backend Kinds

Every atom manifest chooses exactly one implementation backend:

| Backend | Runtime shape |
|---|---|
| `profile` | Dispatches a brofile/persona. This is the only resumable atom handle. |
| `workflow` | Starts an installed workflow with atom args as initial vars. Follow-up belongs in workflow state, not `atom_resume`. |
| `deterministic` | Runs daemon code immediately, such as `echo`, `noop`, `validate-schema`, or validation helpers. |
| `adapter` | Lets a wrapper own execution behind the same atom contract. |

Provider-backed atoms should be used through `atom_status` / `atom_resume`, not
by resuming the underlying bro task directly. Workflow-backed atoms should be
observed through the workflow/arc state after invocation.

## Invocation Invariants

Invoke with structured args and an explicit owner:

```text
atom_invoke(atom="atom:<name>@latest", args={...}, owner="operator:me")
atom_status(invocation_id="<id>", owner="operator:me")
```

The owner is part of access control. If another operator or agent needs to read
or resume the handle, use `atom_delegate`; do not share invocation IDs as a
permission bypass.

Use `atom_resume` only for profile-backed handles. Deterministic, adapter, and
workflow-backed atoms are not resumed through atom profile-session semantics.

## Effect And Composition Invariants

`effects` are upper bounds declared by the atom manifest. Workflow bindings may
tighten those limits, never loosen them. A child atom invocation is legal only
when:

- the caller passes `parent_invocation_id`
- the caller owns the parent invocation
- the parent atom's `composition.may_invoke_atoms` allows the child
- the ancestor chain still has dispatch and depth budget

Common composition failures are `dispatches_runs_exhausted` and
`depth_exhausted`. Treat them as contract failures, not provider flakiness.

## Workflow Bindings

Workflows call atoms through local `atom_bindings`. The workflow-local binding
name is not the public contract; `atom_ref` is. Capability validation checks
that the referenced atom is installed and active, local limits only tighten the
manifest's declared effects, and any profile-backed provider requirements are
satisfiable.

Use workflow-backed atoms when the reusable capability itself is a small state
machine. Use workflow `atom_bindings` when a larger workflow needs a reusable
capability boundary inside one node.

## Manifest Authoring Invariants

The manifest fields that matter most:

- `inputs.schema` and `outputs.schema` define the public contract.
- `effects` declares file writes, dispatch count, max depth, and network use.
- `composition` declares whether children are allowed.
- `implementation` binds to `profile`, `workflow`, `deterministic`, or
  `adapter`.
- `description`, `when_to_use`, and `anti_patterns` drive discovery.

For refactor atoms specifically, `subcontract: "refactor/v1"` requires a
profile implementation bound to the Rust or Java refactor persona, and
operator-authority fields such as `acknowledge_*` must not have defaults.

## When Not To Use Atoms

- One-off operator work: use normal tools or a direct provider task.
- Persona-only reuse with no stable schema: use a brofile/persona.
- Private state machine hidden inside one larger arc: use a workflow.
- A language runbook that only wants to mention a reusable shortcut: signpost
  `atom_search(...)`; do not list atom inventory in system memory.
