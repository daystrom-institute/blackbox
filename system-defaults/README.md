# System Defaults

Installable artifacts shipped with blackbox. These are not tutorial examples;
they are the reference catalog the daemon and operators can install, supersede,
or copy into a project-local catalog.

The daemon does not auto-install this tree. Install only the defaults you want:

```text
bbox_artifact_install(kind="atom", source="system-defaults/atoms/basic/echo.json")
bbox_artifact_install(kind="brofile", source="system-defaults/brofiles/refactor/rust-refactor-persona.json")
bbox_artifact_install(kind="workflow", source="system-defaults/workflows/atoms/echo-review.json")
bbox_artifact_install(kind="cron", source="system-defaults/maintenance/crons/daily-compaction.json")
bbox_compile(path="system-defaults/mcp-surfaces/routing.json", scope="global")
```

## Layout

| Path | Contents |
|---|---|
| `atoms/` | First-class atom artifacts. Includes utility smoke atoms, workflow-backed examples, adapter smoke atoms, and refactor atoms. |
| `workflows/` | Workflow artifacts used by atoms or daemon-owned workflows. |
| `agents/` | Legacy registered-agent manifests and agent-composition workflows kept for compatibility. Prefer atoms for new public capabilities. |
| `brofiles/` | Personas used by default atoms and legacy agents. |
| `badgey/` | Badgey manifests, brofiles, workflows, packets, and crons. |
| `memories/` | System-level memories and runbooks. Files use bare slugs such as `rule-packets.md`; runtime IDs use the `sm-` prefix. These are loaded by the daemon at runtime to provide specialized expert guidance. |
| `agentic-corpus/` | Producer-side knowledge/index maintenance workflows, packets, brofiles, and crons. |
| `maintenance/` | Cross-store maintenance workflows, packets, and crons such as daily compaction. |
| `mcp-surfaces/` | Default MCP surface routing packet. |

`examples/` is now reserved for tutorial specs, skills, and full integration
demos that users copy to learn a pattern. `system-defaults/` is for blackbox-owned
artifacts that can be installed into the artifact catalog.
