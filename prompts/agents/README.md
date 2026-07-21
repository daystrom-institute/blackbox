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
| [gap-processing-orchestrator.md](gap-processing-orchestrator.md) | `.bbox/brofiles/gap-processing.json` (codex) | The gap-processing **workflow** orchestrator actor: runs as the Cluster node (pull → cluster by semantic theme) and the Sieve node (merge verdicts → group/sort). No dispatch — the workflow `foreach` fans out the validators. Launched via [../gap-processing.md](../gap-processing.md). |
| [gap-cluster-validator.md](gap-cluster-validator.md) | `.bbox/brofiles/gap-cluster-validator.json` (deepseek) + atom `gap-cluster-validator` (`gap-validation/v1`) | Per-cluster judge: classify each gap landed/dupe/externality/stale/actionable with evidence + criticality. Read-only; propose-only. Invoked by the workflow as a typed atom (`atom_invoke`). |
| [MINE_CLI.md](MINE_CLI.md) | — (dispatched by [`../REFRESH_ALL_CLIS.md`](../REFRESH_ALL_CLIS.md)) | Forward-mine one CLI version against the harness research corpus's 15 axes; write/refresh that subject's cells + snapshot. |
| [CLI_INVESTIGATOR.md](CLI_INVESTIGATOR.md) | — (operator/orchestrator-pointed) | Backward-discover agent-facing dimensions the research axes MISS; produce a candidate-new-axes report. |
| [DOGFOOD_DRIVER.md](DOGFOOD_DRIVER.md) | — (dispatched by [`../dogfood-orchestration.md`](../dogfood-orchestration.md)) | Drive ONE track of the dogfood loop: dispatch fix-bros through `bro fleet` tranche by tranche, validate live, and raise blockers/deconflicts/friction UP to the orchestrator. Aware peers exist; never coordinates sideways. |
| [edit-only-worktree.md](edit-only-worktree.md) | — (orchestrator-pointed, any code dispatch into a cold/edit-only checkout) | Operating rules for cold-checkout code work: commit granular changes with tests WRITTEN, never run compile-shaped gates locally (the cold-checkout guard blocks them); the orchestrator verifies lane-side and steers corrections back. |
| [kimi-review.md](kimi-review.md) | [`../../scripts/kimi-review.sh`](../../scripts/kimi-review.sh) | Fixed-boundary, read-only Kimi review of the complete monolith-decomposition attempt, with same-session re-review. |
