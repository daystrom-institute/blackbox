# Simple agents

A registered agent binds a brofile to a focused prompt, input/output schemas and
retrieval metadata. Dispatch starts one ordinary bro turn. The caller owns
sequencing, reviews and subsequent work. Custom dispatch adapters are retired;
old adapter-backed records can be read but cannot execute or be reinstalled.

Install a validated inline agent artifact through
`bbox_artifact_install(kind="agent", artifact=...)`, or supply an HTTP(S) JSON
URL. Install referenced brofiles first. Caller filesystem paths are rejected.

Use `bro_agent_list` for bounded summary pages and follow `next_offset`.
`bro_agent_search` finds suitable roles; `bro_agent_get` or `bro_agent_describe`
expands one exact agent. Dispatch with `bro_agent_dispatch(agent=..., args=...)`
and retain the returned task and session handles. `bro_status`, `bro_wait` and
`bro_cancel` control the resulting task; `bro_resume` continues its session.

Filters merge with deny precedence across the MCP session, project, persona
and agent overlay. Recursive bro control stays guarded unless explicitly enabled
by the agent contract. Operator-authority inputs are never inferred or silently
set by an agent. See [bro runtime](bro-runtime.md) and
[artifact catalog](artifact-catalog.md).
