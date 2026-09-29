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
| [mcp-survivor-fixes/](mcp-survivor-fixes/README.md) | GLM 5.3 dispatches | Ten bounded MCP caller-contract fix briefs with isolated file ownership, pushed branch deliverables, and orchestrator-owned cluster verification. |
| [gap-processing-orchestrator.md](gap-processing-orchestrator.md) | `.bbox/brofiles/gap-processing.json` (codex) | Grouping and synthesis phases of caller-owned gap processing: cluster supplied gap records by missing capability, then synthesize validator verdicts. No dispatch; the caller fans out the validators. Launched via [../gap-processing.md](../gap-processing.md). |
| [gap-cluster-validator.md](gap-cluster-validator.md) | `.bbox/brofiles/gap-cluster-validator.json` (deepseek) | Per-cluster judge: classify each gap landed/dupe/externality/stale/actionable with evidence + criticality. Read-only; propose-only. Dispatched by the caller with `bro_exec`. |
| [MINE_CLI.md](MINE_CLI.md) | — (dispatched by [`../REFRESH_ALL_CLIS.md`](../REFRESH_ALL_CLIS.md)) | Forward-mine one CLI version against the harness research corpus's 15 axes; write/refresh that subject's cells + snapshot. |
| [CLI_INVESTIGATOR.md](CLI_INVESTIGATOR.md) | — (operator/orchestrator-pointed) | Backward-discover agent-facing dimensions the research axes MISS; produce a candidate-new-axes report. |
| [edit-only-worktree.md](edit-only-worktree.md) | — (orchestrator-pointed, any code dispatch into a cold/edit-only checkout) | Operating rules for cold-checkout code work: commit granular changes with tests WRITTEN, never run compile-shaped gates locally (the cold-checkout guard blocks them); the orchestrator verifies lane-side and steers corrections back. |
| [kimi-review.md](kimi-review.md) | [`../../scripts/kimi-review.sh`](../../scripts/kimi-review.sh) | Fixed-boundary, read-only Kimi review of the complete monolith-decomposition attempt, with same-session re-review. |
| [kimi-plan-review.md](kimi-plan-review.md) | [`../../scripts/kimi-review.sh`](../../scripts/kimi-review.sh) | Fixed-scope, read-only Kimi review of the durable project catalog implementation plan, with same-session re-review. |
