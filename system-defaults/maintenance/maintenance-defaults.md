# Maintenance System Defaults

Install these artifacts when the daemon should own recurring storage maintenance
as a workflow instead of as an operator runbook.

`daily-compaction` runs once per day and covers:

- system-event journal and outbox retention compaction
- edge sidecar storage GC with the same pruning policy as the daemon maintenance pass
- vector partition compaction through the existing embed compaction policy

Install the workflow, policy packets, and cron spec with `bbox_artifact_install`
or the daemon admin install endpoint:

```text
bbox_artifact_install(kind="workflow", source="system-defaults/maintenance/workflows/daily-compaction-arc.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/embed/compaction-policy.json")
bbox_artifact_install(kind="packet", source="system-defaults/maintenance/packets/cron-routing/daily-compaction.json")
bbox_artifact_install(kind="cron", source="system-defaults/maintenance/crons/daily-compaction.json")
```
