# Maintenance System Defaults

Install these artifacts when the daemon should own recurring storage maintenance
as a workflow instead of as an operator runbook.

`daily-compaction` runs once per day and covers:

- system-event journal and outbox retention compaction
- edge sidecar storage GC with the same pruning policy as the daemon maintenance pass
- vector partition compaction through the existing embed compaction policy

The one-command path installs both maintenance arcs (daily-compaction plus
the nightly embed compaction from `agentic-corpus/`) including their cron
specs, against a running daemon:

```bash
system-defaults/maintenance/scripts/install-maintenance.sh   # BBOX_PORT overrides 7264
```

Run it as a deploy/upgrade step. It is idempotent — artifact installs are
content-hash idempotent, so re-running refreshes changed members only.

Or install the daily-compaction members individually with
`bbox_artifact_install` or the daemon admin install endpoint:

```text
bbox_artifact_install(kind="workflow", source="system-defaults/maintenance/workflows/daily-compaction-arc.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/workflow-policy/arc-budget.json")
bbox_artifact_install(kind="packet", source="system-defaults/agentic-corpus/packets/embed/compaction-policy.json")
bbox_artifact_install(kind="packet", source="system-defaults/maintenance/packets/cron-routing/daily-compaction.json")
bbox_artifact_install(kind="cron", source="system-defaults/maintenance/crons/daily-compaction.json")
```

The `kind="cron"` install is the step that actually schedules the arc — a
cron-routing packet or workflow installed without its cron is maintenance
that exists but never runs. `bbox_inbox` surfaces that state as a
"Cron scheduling gaps" section (it also flags the inverse: a live cron whose
routing packet is missing).
