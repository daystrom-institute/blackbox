# Artifact Catalog And System Defaults

The artifact catalog is the versioned install path for blackbox-owned machinery:
workflows, packets, brofiles, agents, atoms, and teams.

`system-defaults/` is the shipped catalog source. `examples/` is for tutorial
material and full demos users copy to learn a pattern.

## Install

Install from a local file or URL:

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/atoms/echo-review.json")
```

The installer validates the artifact through its native path before recording
metadata. Atom installs validate atom manifests. Workflow installs validate
workflow shape. Packet installs compile packet artifacts.

## List And Supersede

```text
bbox_artifact_list(kind="atom")
bbox_artifact_list(kind="workflow", include_superseded=true)

bbox_artifact_supersede(
  kind="workflow",
  name="auto-digest-arc",
  superseded_by="auto-digest-arc-v2"
)
```

Supersession keeps the old artifact for audit while excluding it from active
lists by default.

## System Defaults Layout

| Path | Contents |
|---|---|
| `system-defaults/atoms/` | Utility atoms, adapter smoke atoms, workflow-backed atoms, and refactor atoms. |
| `system-defaults/workflows/` | Workflows used by atoms and reusable workflow playbooks. |
| `system-defaults/brofiles/` | Personas used by atoms and legacy agents. |
| `system-defaults/agents/` | Legacy registered-agent manifests and agent-composition workflows. |
| `system-defaults/badgey/` | Badgey agents, workflows, packets, brofiles, and crons. |
| `system-defaults/agentic-corpus/` | Producer-side knowledge/index maintenance workflows and packets. |
| `system-defaults/mcp-surfaces/` | Default MCP surface routing packet. |

The daemon does not auto-install this tree. Install only what you intend to run.

## Install Order

Some artifacts reference other artifacts:

- Profile-backed atoms need their brofile installed.
- Workflow-backed atoms need their workflow installed.
- Workflow specs may reference packets, atoms, brofiles, teams, or subworkflows.
- Crons and webhooks route through packets and start installed workflows.

Install dependencies first. For example:

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/atoms/echo-review.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/workflows/echo-review.json")
```

For refactor atoms:

```text
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/java-refactor-persona.json")
bbox_artifact_install(kind="atom", source="system-defaults/atoms/refactor/java-extract-cohesive-class.json")
```

## Examples Are Different

`examples/` contains learning material:

- `examples/workflows/` for small engine patterns
- `examples/keystone/` for issue to PR to review to merge
- `examples/sastquatch/` for cron to SAST to fix to review
- `examples/whiteboard/` for phased deliberation
- `examples/skills/` for Claude Code skill patterns

If a file is daemon-owned reusable machinery, keep it under `system-defaults/`.
If it is a tutorial or adaptation starting point, keep it under `examples/`.

## Local Project Catalogs

Project-local machinery belongs under `.bbox/` when it is specific to one repo.
Use `bbox_project_init` to create the skeleton:

```text
bbox_project_init(path="/repo/x")
```

Then install project-owned artifacts from `.bbox/...` through the same catalog
tools.
