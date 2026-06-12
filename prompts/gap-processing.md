---
title: "Gap Processing"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - gaps
brief: "Interactive-facing launch doc: how YOU launch the gap-processing WORKFLOW (bro_orchestrate_run), monitor it, and act on its sieved output. The workflow is a deterministic 3-node arc — Cluster (codex actor) → Validate (foreach atom_invoke of the gap-cluster-validator atom, deepseek) → Sieve (codex actor). The daemon runtime owns the fan-out; no recursive-bro dispatch."
---

# Gap Processing

The substrate gap queue (`bbox_gaps`) accumulates faster than it clears. This
prompt drives a **deterministic workflow** that sieves it: filters the noise
(already-landed, dupe, externality, stale) before surfacing what is genuinely
actionable, priority-ordered.

```
[Interactive]  ── you, reading THIS doc; launch the workflow + close the loop
     │  bro_orchestrate_run(workflow=gap-processing)
     ▼
[Workflow arc] ── daemon-owned state machine:
     Cluster   (actor=orchestrator, codex gpt-5.5)   pull gaps → cluster by theme → vars.clusters
     Validate  (foreach over clusters → atom_invoke gap-cluster-validator, deepseek)   one atom per cluster, dynamic-N
     Sieve     (actor=orchestrator, codex gpt-5.5)   merge verdicts → group/sort → vars.sieve
```

> **Why a workflow, not a recursive bro.** The workflow runtime owns the
> `foreach` fan-out, giving deterministic state, schema-enforced validator
> output, and runtime provenance. Heavy prose still lives in `prompts/agents/`
> so it stays tweakable. (The dispatch limitation that originally forced this
> shape — `bro_agent_dispatch` hardcoding `allow_recursion=false` — is resolved:
> an agent manifest can declare `allow_recursion: true`, and
> `bro_exec(bro="<brofile>", allow_recursion=true)` resolves standalone brofiles
> by name, test-pinned. See `gap-a5e152fb`. The workflow remains the right shape
> here for determinism, not as a workaround.)

## Preconditions

Check `bbox_artifact_list` for all four, install from the repo if absent:
- brofile `gap-processing` (`.bbox/brofiles/gap-processing.json`) — codex `gpt-5.5` `high`, the Cluster+Sieve actor.
- brofile `gap-cluster-validator` (`.bbox/brofiles/gap-cluster-validator.json`) — deepseek `deepseek-v4-pro` `high`, wrapped by the atom.
- atom `gap-cluster-validator` (`.bbox/atoms/gap-cluster-validator.json`, `subcontract gap-validation/v1`) — typed cluster-in / verdict-out.
- workflow `gap-processing` (`.bbox/workflows/gap-processing.json`).

(Artifact install, render, and `bro_orchestrate_run` live on the **blackbox-ops** MCP surface; load with `ToolSearch` if deferred.)

## Launch the workflow

```
bro_orchestrate_run(
  workflow=<the gap-processing spec from .bbox/workflows/gap-processing.json>,
  project_dir="/home/invidious/repos/transcript-search",
  initial_vars={"project_dir": "/home/invidious/repos/transcript-search"}
)
```

Pass `dry_run=true` first to validate the spec without dispatching. The MCP tool
takes the spec **inline** (`workflow=`), not by id — read it from the installed
`.bbox/workflows/gap-processing.json`. It returns `{taskId, arcId}`.

## Monitor

```
bro_arc_status(arc_id="<arcId>")          # compact: current_node / completed_nodes
bro_wait(task_id="<taskId>", timeout_seconds=290)   # repeat until completed
```

Cluster ~2min, the deepseek validator fan-out is the long pole (~4–6min), Sieve
~1–2min. A `bro_wait` timeout is just a snapshot — re-check `bro_arc_status`.

## Read the result + close the loop

The sieve lands in arc var `_structured_exit` (= `vars.sieve`). Retrieving it is
currently awkward (`gap-55be3518`): `bro_wait` returns a large multi-escaped
envelope that spills to a file — slice it with python and locate the `sieve`
object. It is grouped `landed → dupe → externality → stale → actionable`, sorted
by criticality desc, and **every verdict carries `provenance.{validator_task_id,
validator_invocation_id}`** — the real runtime handle of the validator atom that
judged it.

Your job at Layer 1:
1. **Present** the grouped/sorted lists to the operator — cheap clears first,
   priority-ranked actionable last.
2. **Resolve only on operator approval, one gap at a time** (`bbox_gap_resolve`,
   using each verdict's `proposed_resolution_args`). Never bulk-resolve. The
   workflow **proposes only**.
3. **Answer provenance questions by resuming the leaf** — `bro_resume(session_id=
   <provenance from the verdict>, provider="deepseek", prompt="re-justify …")`.

## Contracts

- **Provenance is preserved end-to-end**, captured from the workflow runtime (not
  model self-report) — any verdict's validator atom is resumable.
- **Nothing is mutated without you** — no gap is resolved by any node.
