# Atoms

An atom is what you install when you want "run this capability" to mean the
same thing no matter what backs it.

`atom:echo@v1` is the smoke test. `atom:rust-test-island-extract@v1` is the
real shape: discover it, pass structured args, get an invocation handle, inspect
the trace, delegate it to another operator if needed, and resume it when the
backend is a provider session.

Atoms are the public capability surface going forward. Registered agents still
exist for `bro_agent_*` compatibility, but agents are mostly persona dispatch.
Atoms add the pieces callers need before they can trust a reusable operation:
schemas, effect limits, child-invocation rules, ownership, and trace state.

## The Short Version

Install an atom:

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
```

Invoke it:

```text
atom_invoke(
  atom="atom:echo@v1",
  args={"message": "hello"},
  owner="operator:me"
)
```

Check the handle:

```text
atom_status(invocation_id="<id>", owner="operator:me")
```

The owner matters. If you invoke as `operator:me` and later query as the default
`operator:local`, status is forbidden. That is intentional. Invocation handles
are shared through `atom_delegate`, not by guessing ids.

## What Actually Runs

An atom manifest chooses exactly one backend:

| Backend | Use it when | Runtime behavior |
|---|---|---|
| `profile` | A brofile/persona does the work | Starts a normal provider task through the brofile path. This is the only resumable atom handle. |
| `workflow` | The operation is a small state machine | Starts an installed workflow with atom args as initial vars. Follow-up belongs in workflow state, not `atom_resume`. |
| `deterministic` | Daemon code can answer immediately | Runs an in-process runner such as `echo`, `noop`, `validate-schema`, or `refactor-plan-validate`. |
| `adapter` | A wrapper owns the real execution path | Runs an adapter runner behind the same atom contract. The built-in adapter smoke path is `badgey`. |

Profile and workflow atoms create handles backed by bro tasks or workflow arcs.
Deterministic and adapter atoms normally finish in the `atom_invoke` call.

## System Defaults

The daemon does not auto-install shipped defaults. Pick what you want from
`system-defaults/`. See [Artifact Catalog And System Defaults](artifact-catalog.md)
for install order and catalog mechanics.

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/validate-schema.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/rust-test-island-extract.json")
```

Workflow-backed atoms need the referenced workflow installed first:

```text
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/atoms/echo-review.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/workflows/echo-review.json")
```

If `atom_invoke` says the workflow is missing, install the workflow artifact.
The atom manifest is only the wrapper; it does not embed the workflow body.

## Discovery

Use `atom_list` when you already know the catalog shape:

```text
atom_list(subcontract="refactor/v1")
atom_list(cost_class="cheap")
```

Use `atom_search` when you know the job:

```text
atom_search(query="split Rust inline tests", cost_class="normal")
atom_describe(atom="atom:rust-test-island-extract@v1")
```

Search ranks `description` and `when_to_use`. Anti-pattern hits are excluded by
default because those are usually "this is exactly the wrong tool" matches. Set
`exclude_anti_pattern_matches=false` when you are auditing the catalog rather
than choosing a tool.

## Invocation Handles

Every invocation records the same trace shape:

```json
{
  "invocation_id": "...",
  "atom_ref": "atom:echo@v1",
  "implementation_kind": "deterministic",
  "state": "succeeded",
  "output_shape": { "valid": true, "schema_ref": "outputs.schema", "errors": [] },
  "effective_limits": { "dispatches_runs": 0, "writes_files": false },
  "effects_observed": { "dispatches_runs": 0, "violations": [] },
  "cost": { "dispatched_runs": 0, "wall_time_ms": 0 },
  "children": [],
  "errors": []
}
```

For provider-backed atoms, `atom_status` refreshes from the underlying bro task
before returning. For workflow-backed atoms, it refreshes from the root workflow
task. For deterministic and adapter atoms, the trace is already terminal.

Use `atom_resume` only for profile-backed handles:

```text
atom_resume(
  invocation_id="<id>",
  prompt="apply the reviewer feedback and rerun validation",
  owner="operator:me"
)
```

Use `atom_delegate` before another operator or agent needs to read or resume the
same handle:

```text
atom_delegate(
  invocation_id="<id>",
  owner="operator:me",
  grant_to="operator:reviewer"
)
```

Delegation is additive in v1. There is no revocation path yet.

## Limits And Child Atoms

Composition is deliberately strict. A child atom only runs when all of this is
true:

- The caller passes `parent_invocation_id`.
- The caller owns that parent invocation.
- The parent atom's `composition.may_invoke_atoms` allows the child ref.
- The ancestor chain still has enough `dispatches_runs` and `max_depth` budget.

This is what keeps workflow-backed atoms from becoming arbitrary dispatch
tunnels.

Effect limits are upper bounds from the manifest, optionally tightened by a
workflow binding. A binding can say "this workflow may run `atom:echo@v1` with
zero dispatches and no file writes." It cannot bless more power than the atom
declared for itself.

Two common failures are worth recognizing:

- `dispatches_runs_exhausted`: the atom or one of its ancestors declared too
  small a dispatch budget for the requested backend.
- `depth_exhausted`: a nested atom call would exceed `max_depth`.

Workflow-backed atoms also need a static dispatch budget in v1. Dynamic
`foreach` items or dynamic `matrix` axes inside the wrapped workflow are rejected
because the daemon cannot know the worst-case dispatch count up front.

## Workflow Bindings

Workflows call atoms through local `atom_bindings`:

```json
{
  "atom_bindings": {
    "echo": {
      "atom_ref": "atom:echo@v1",
      "limits": {
        "writes_files": false,
        "dispatches_runs": 0,
        "max_depth": 0,
        "uses_network": false
      }
    }
  },
  "nodes": {
    "Echo": {
      "atom": "echo",
      "atom_args": { "message": "${vars.message}" },
      "next": { "type": "terminal" }
    }
  }
}
```

The binding name is local to the workflow. The atom ref is the public contract.

At compile/capability-validation time the engine checks that the binding points
at an installed active atom, that any local `limits` only tighten the atom's
effects, and that profile-backed provider requirements are satisfiable.

At runtime the node calls `atom_invoke`, stores the invocation id, then records
`atom_status` as the node output. If a binding is `durable`, a later visit tries
to resume the same profile-backed handle. Non-resumable handles are invoked
again.

See [`workflows.md#atom-bindings`](workflows.md#atom-bindings) for workflow
engine details. The runnable smoke workflow is
`examples/workflows/e2e-atom-binding.json`.

## Manifest Reference

This is the shape most atoms use:

```json
{
  "_contract": "atom/v1",
  "kind": "atom",
  "name": "echo",
  "version": 1,
  "subcontract": "utility/v1",
  "manifest": {
    "description": "Return invocation arguments unchanged under an echo key.",
    "when_to_use": ["testing atom invocation"],
    "anti_patterns": ["do not use when the caller expects side effects"],
    "inputs": {
      "schema": { "type": "object", "additionalProperties": true }
    },
    "outputs": {
      "schema": {
        "type": "object",
        "required": ["echo"],
        "properties": { "echo": {} },
        "additionalProperties": false
      },
      "evidence_density": "low"
    },
    "effects": {
      "writes_files": false,
      "dispatches_runs": 0,
      "max_depth": 0,
      "uses_network": false
    },
    "composition": {
      "may_invoke_atoms": { "kind": "none" }
    },
    "implementation": {
      "kind": "deterministic",
      "runner": "echo"
    },
    "trace": {
      "retain": "summary",
      "portal_focus": "on_request"
    },
    "cost_class": "cheap"
  }
}
```

The fields that usually matter during authoring are `inputs.schema`,
`outputs.schema`, `effects`, `composition`, and `implementation`. The rest helps
discovery, supervision, trace retention, and provenance.

## When Not To Use Atoms

| Situation | Better fit |
|---|---|
| One-off operator work | `bro exec` or `bro resume` |
| Persona-only reuse with no stable contract | Brofile or legacy agent |
| Private state machine hidden inside one larger arc | Workflow |
| Public capability with schemas, limits, and trace handles | Atom |
| Workflow needs a public reusable capability | Atom plus `atom_bindings` |

For refactor-specific atoms, use [Refactor Tools And Atoms](refactor.md).
