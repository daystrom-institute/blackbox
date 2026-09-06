# Artifact catalog

The catalog installs packets, brofiles, simple agents and teams. Supply exactly
one inline `artifact` object or explicit HTTP(S) `source` URL. A path on the
caller machine cannot be read by this remote tool.

```text
bbox_artifact_list(kind="brofile")
bbox_artifact_install(kind="brofile", artifact={"name":"reviewer","provider":"brodex","lens":"Review correctness and explain material findings."})
```

List responses contain bounded summaries. Follow `next_offset`; request
`detail=true` for installation and supersession metadata. Team artifacts require
their member brofiles to be installed first. Reinstallation preserves live
sessions. Automatic advisors are retired; dispatch reviewers explicitly.

Workflow, atom and cron kinds cannot be installed or activated. Their historical
receipts remain readable with an explicit filter, such as
`bbox_artifact_list(kind="workflow")`, marked `retired=true, active=false`.
Startup does not replay them. Supersession retains old versions; removal is a
separate explicit operation with a dry run and confirmation.

[System defaults](../system-defaults/system-defaults.md) maps the retained artifacts.
