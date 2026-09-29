# System defaults

Blackbox ships optional brofiles, simple agents, teams and deferred
system memories. Built-in MCP surfaces live in daemon configuration
(`crates/bbox-config/src/default_surfaces.toml`, see [MCP surfaces](../docs/mcp-surfaces.md)). The daemon does not install this whole tree automatically.
Workflow, atom, cron and packet manifests are retired.

List before installing. Read a chosen JSON file in the caller harness, then pass
its object as `artifact` to `bbox_artifact_install(kind=..., artifact=...)`.
An explicit HTTP(S) JSON URL is also accepted. Caller file paths are rejected.
Install member brofiles before a team; team installation preserves live sessions.

| Path | Purpose |
| --- | --- |
| `brofiles/`, `agentic-corpus/brofiles/` | Role prompts for explicitly dispatched workers. |
| `agents/` | Simple agent input/output contracts. |
| `memories/` | Deferred runbooks, loaded by the daemon. |

Bro orchestration keeps execution, resume, status and waits. The caller composes
reviews, gates, schedules and external integrations. See [artifact catalog](../docs/artifact-catalog.md).
