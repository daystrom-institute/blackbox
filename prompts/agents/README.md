---
title: "Dispatched-Agent Lenses"
kind: prompt-hub
corpus: blackbox-prompts
topic:
  - prompts
  - prompts-agents
brief: "Lens prompts referenced by brofiles/orchestrators. A dispatched bro is pointed at one of these as its operating doc, so the lens can be tuned without editing the brofile."
---

# Dispatched-Agent Lenses

Operating-doc prompts for **dispatched bros**, kept separate from the brofile
that references them so the lens can be tuned independently. A brofile or
orchestrator points a bro at `prompts/agents/<lens>.md`; the bro reads it as its
charter.

Parent: [Prompts](../README.md)

## Lenses

| Lens | Paired brofile | Role |
|------|----------------|------|
| [gap-processing-orchestrator.md](gap-processing-orchestrator.md) | `.bbox/brofiles/gap-processing.json` | Layer 2 of the gap sieve: pull → cluster (semantic theme) → dispatch validators → reassemble → sieve → return. Launched by the interactive layer with `allow_recursion=true` (see [../gap-processing.md](../gap-processing.md)). |
| [gap-cluster-validator.md](gap-cluster-validator.md) | `.bbox/brofiles/gap-cluster-validator.json` | Layer 3 (leaf): classify each gap in a cluster as landed/dupe/externality/stale/actionable with evidence + criticality. Read-only; propose-only. |
| [MINE_CLI.md](MINE_CLI.md) | — (dispatched by [`../REFRESH_ALL_CLIS.md`](../REFRESH_ALL_CLIS.md)) | Forward-mine one CLI version against the harness research corpus's 15 axes; write/refresh that subject's cells + snapshot. |
| [CLI_INVESTIGATOR.md](CLI_INVESTIGATOR.md) | — (operator/orchestrator-pointed) | Backward-discover agent-facing dimensions the research axes MISS; produce a candidate-new-axes report. |
