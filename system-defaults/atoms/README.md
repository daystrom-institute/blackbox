# Atom Defaults

Installable atom artifacts shipped with blackbox. These are catalog defaults,
not tutorial-only examples.

## Layout

| Path | Implementation | Purpose |
|---|---|---|
| `basic/echo.json` | deterministic runner `echo` | Smoke test for `atom_invoke`, workflow `atom_bindings`, ownership, and output-shape validation. |
| `basic/validate-schema.json` | deterministic runner `validate-schema` | Cheap object-shape probe. |
| `adapters/badgey.json` | adapter runner `badgey` | Adapter-path smoke test. |
| `workflows/echo-review.json` | workflow ref `workflow:atom-echo-review@v1` | Minimal workflow-backed atom that invokes `atom:echo@v1`. |
| `refactor/` | profile-backed atoms | Refactor capabilities backed by refactor brofiles. |

## Install And Invoke

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
atom_invoke(atom="atom:echo@v1", args={"message":"hello"}, owner="operator:me")
atom_status(invocation_id="<id>", owner="operator:me")
```

Workflow-backed atoms need their workflow artifact installed:

```text
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/atoms/echo-review.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/workflows/echo-review.json")
```

Refactor atoms need their language persona brofile installed first:

```text
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/rust-test-island-extract.json")
```

## Legacy Agents

Legacy registered-agent manifests live under `system-defaults/agents/`. They are
kept for compatibility with `bro_agent_*`. New public capability artifacts should
be added here as atoms.
