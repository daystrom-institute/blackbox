---
title: "Gap Processing"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - gaps
brief: "Interactive-facing launch doc: how YOU (a live interactive agent) dispatch the gap-processing orchestrator bro, monitor it, and act on its sieved output. Three-layer design — interactive → orchestrator bro → per-cluster validator bros. The heavy logic lives in the two agent contracts under prompts/agents/; this doc is only the launch + close-the-loop layer."
---

# Gap Processing

The substrate gap queue (`bbox_gaps`) accumulates faster than it clears. This
prompt drives a **three-layer sieve** that filters noise (already-landed, dupe,
externality, stale) before surfacing what is genuinely actionable, priority-ordered.

```
[Interactive]  ── you, reading THIS doc; launches + closes the loop
     │  bro_exec(allow_recursion=true)
     ▼
[Orchestrator] ── the `gap-processing` bro; lens → prompts/agents/gap-processing-orchestrator.md
     │  bro_exec per cluster (leaf, no recursion)
     ▼
[Validator×N] ── `gap-cluster-validator` bros; lens → prompts/agents/gap-cluster-validator.md
```

You are **Layer 1**. You do **not** pull or classify gaps yourself — you launch
the orchestrator, wait, and act on its report. The orchestrator clusters and
fans out; the validators investigate. Heavy prose lives in the two agent
contracts so it can be tuned without editing brofile JSON.

## Preconditions

- `blackboxd` is running and this project is registered (`bbox_project_list`).
- Both brofiles are installed: check `bbox_artifact_list(kind="brofile")` for
  `gap-processing` and `gap-cluster-validator`; if absent, install from
  `.bbox/brofiles/gap-processing.json` and
  `.bbox/brofiles/gap-cluster-validator.json`.

## Launch the orchestrator

The orchestrator dispatches sub-bros, so it **must** be launched with
`allow_recursion=true` — without it the recursion guard blocks every `bro_*`
dispatch and the orchestrator dead-ends. `bro_agent_dispatch` hardcodes
`allow_recursion=false`, so you **cannot** use it here; launch via `bro_exec`:

```
bro_exec(
  bro="gap-processing",                       # the orchestrator brofile/persona
  project_dir="/home/invidious/repos/transcript-search",
  allow_recursion=true,                       # REQUIRED — the load-bearing arg
  prompt="Process the active gap queue for this project per your contract. "
         "Cluster by semantic theme, dispatch one gap-cluster-validator per "
         "cluster, sieve the verdicts, and return the grouped/sorted action "
         "lists with full validator provenance."
)
```

> **FIRST-RUN VERIFY.** Confirm `bro_exec` resolves `bro="gap-processing"` to the
> brofile persona. If brofile-by-name targeting isn't supported on this build,
> the fallback is to instantiate the brofile as a bro instance (`bro_team`) then
> `bro_exec(bro=<instance>, allow_recursion=true, …)`, or pass `provider="claude"`
> + the brofile lens inline. This is the one mechanic to nail on the first live
> run (refinement was intentionally deferred until the prompt set was authored).

Record the returned `{taskId, sessionId}` — that is the orchestrator's handle.

## Monitor

```
bro_status(task_id="<taskId>", tail=40)     # evidence before judging liveness
bro_wait(task_id="<taskId>", timeout_seconds=900)
```

A timeout is not death — clustering + N validator dispatches take real time. Tail
before concluding anything is stuck.

## Act on the report (close the loop)

The orchestrator returns the sieve: action lists **grouped by class** in
clear-the-noise order — `landed → dupe → externality → stale → actionable` —
and **sorted by criticality descending** within each group. Every verdict line
carries the **leaf validator's `{task_id, session_id}`**.

Your job at Layer 1:

1. **Present** the grouped/sorted lists to the operator as-is. Lead with the
   cheap clears (landed/dupe/externality/stale), close with the priority-ranked
   actionable list.
2. **Resolve only on operator approval, one gap at a time.** Each
   `bbox_gap_resolve` is its own approval — never bulk-resolve a whole class off
   one "proceed." Use the validator's proposed `resolution` + `note` (and
   `superseded_by` for dupes). The orchestrator and validators **propose only**;
   the actual resolution call is yours, gated.
3. **Answer provenance questions by resuming the leaf.** If the operator asks
   "why did we stale X?", resume the exact validator that made the call:

   ```
   bro_resume(session_id="<validator session_id from the verdict>",
              provider="claude",
              prompt="Re-justify your STALE verdict on gap-XXXX with evidence.")
   ```

   The leaf agent holds the investigation context; the orchestrator only relayed it.

## Contracts (restate to the operator if asked)

- **Provenance is preserved end-to-end.** Interactive → orchestrator → validator
  ids all survive in the report; any node in the decision chain is resumable.
- **Nothing is mutated without you.** No gap is resolved, filed, or edited by any
  bro. The sieve is advisory until the operator approves each clear.
