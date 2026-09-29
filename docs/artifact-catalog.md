# Artifact catalog

The catalog installs brofiles. The `bbox_artifact_*` tools are on
the `ops` surface: run them from an `ops` MCP session or with
`bro mcp call <tool> '<json>' --surface ops`. Agents discover installed
personas with `bro_brofile(action="list")`. Supply exactly one inline
`artifact` object or explicit HTTP(S) `source` URL. A path on the caller
machine cannot be read by this remote tool.

```text
bbox_artifact_list(kind="brofile")
bbox_artifact_install(kind="brofile", artifact={"name":"reviewer","provider":"brodex","lens":"Review correctness and explain material findings."})
```

List responses contain bounded summaries. Follow `next_offset`; request
`detail=true` for installation and supersession metadata.

Workflow, agent, atom, cron, packet and team kinds cannot be installed or
activated.
Their historical receipts remain readable with an explicit filter, such as
`bbox_artifact_list(kind="workflow")`, marked `retired=true, active=false`.
Startup does not replay them. Supersession retains old versions; removal is a
separate explicit operation with a dry run and confirmation.

[System defaults](../system-defaults/system-defaults.md) maps the retained artifacts.
