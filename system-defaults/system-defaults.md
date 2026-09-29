# System defaults

Blackbox ships optional brofiles and deferred system memories. Built-in
MCP surfaces live in daemon configuration
(`crates/bbox-config/src/default_surfaces.toml`, see
[MCP surfaces](../docs/mcp-surfaces.md)). The daemon does not install this
whole tree automatically.

Installation is an operator step. List before installing
(`bro mcp call bbox_artifact_list '{"kind":"brofile"}' --surface ops`), read a
chosen JSON file, then pass its object as `artifact`:
`bro mcp call bbox_artifact_install '{"kind":"brofile","artifact":{...}}' --surface ops`.
An explicit HTTP(S) JSON URL is also accepted as `source`. Caller file paths are
rejected.

| Path | Purpose |
| --- | --- |
| `brofiles/`, `agentic-corpus/brofiles/` | Role prompts for explicitly dispatched workers. |
| `memories/` | Deferred runbooks, loaded by the daemon. |

Bro orchestration keeps execution, resume, status and waits. The caller composes
reviews, gates, schedules and external integrations. See [artifact catalog](../docs/artifact-catalog.md).
