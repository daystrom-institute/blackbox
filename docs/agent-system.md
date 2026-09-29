# Simple agents

A registered agent artifact binds a brofile to a focused prompt, input/output
schemas and retrieval metadata. Custom dispatch adapters are retired;
adapter-backed records can be read but cannot be reinstalled.

Install a validated inline agent artifact through
`bbox_artifact_install(kind="agent", artifact=...)`, or supply an HTTP(S) JSON
URL. Install referenced brofiles first. Caller filesystem paths are rejected.
Installation validates the manifest, records filter-overlay conflicts as
install warnings, stamps manifest embeddings and persists provenance edges.

`bbox_artifact_list(kind="agent")` pages installed agents and their install
warnings; `bbox_describe_schema` lists active agents with their description,
`when_to_use`, `anti_patterns` and cost class. Work runs as an ordinary bro:
`bro_exec` with the agent's brofile, then `bro_status`, `bro_wait`,
`bro_cancel` and `bro_resume`. See [bro runtime](bro-runtime.md) and
[artifact catalog](artifact-catalog.md).
