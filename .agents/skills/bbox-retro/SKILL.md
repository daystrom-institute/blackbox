---
name: bbox-retro
description: Run a requested end-of-session debrief for work in this repository, capturing concrete friction and wishes as deduplicated gap notes, thread notes and proposed tracker issues, without inventing findings. Use when the user asks for a retro, retrospective or debrief, asks what got in your way or what you wish existed, or asks to capture gaps before closing out.
---

# Session retro

A retro preserves what only you know right now: where the tools, instructions and environment got in the way while working on this repository, and what worked well enough to keep. It dissolves when the session ends, so the output is durable records, not a long chat reply.

This reflects on the working experience, not the product work. A bug you found in the code is not retro material unless the way you were tooled, instructed or routed caused it. A retro does not replace delivery and does not authorize more implementation: keep the task's completion state and any unfinished work visible in what you report.

A quiet session is a normal result. If nothing concrete stands out, say so in one line and create nothing. There is no quota.

## 1. Walk the session

Go through it in order and list the moments of friction, surprise, rework and unusual smoothness, from what actually happened: calls, receipts, files and corrections. Look in these places:

- **Tools and MCP**: a tool you reached for that was missing, hidden until you searched, awkwardly shaped, or available only after a restart. A call that landed on the wrong project or surface.
- **Instructions**: repo, global or brief text that was stale, contradictory, too broad, or missing an invariant you had to rediscover. Name the file.
- **Build and verification**: cold checkouts, lanes, gates, cluster verification. Anything that made it unclear where a command should run or whether a result could be trusted.
- **Environment**: shell, paths, config directories, signing, services, worktrees, other agents' uncommitted work.
- **Operator interaction**: a correction that showed you were carrying a wrong assumption, and what the repository could have told you so the operator did not have to.
- **Session boundaries**: what did not survive compaction, a restart or a handoff.
- **Wishlist**: a capability that would have removed a manual sequence you repeated.
- **What worked**: something worth documenting so the next agent gets it without luck.

Be concrete: "I reached for X, got Y, worked around it with Z." Report what happened, not what you think the operator intended.

## 2. Debrief workers you dispatched

If this session dispatched agents whose sessions are still resumable, ask each one the same questions through its existing session, after its current turn has finished. Their friction is invisible to you otherwise. Do not start new agents to produce retro material.

## 3. Sort each item

| The item is | Record it as |
|---|---|
| A missing or wrong-shaped Blackbox capability, tool surface, workflow shape, runbook or instruction another agent would hit too | A gap note |
| A follow-up for this repository's docs, environment or process that is not a missing capability, or a fact someone should consider documenting | A thread note |
| A concrete, scoped defect or change in this repository's code, describable with a reproduction or an acceptance criterion | A proposed tracker issue, for the operator to approve |
| A rule the operator stated that should bind future sessions | An exact-text proposal through the `bbox-persist` skill. Never a write made as a side effect of a retro. |
| A one-off preference, an external service's behaviour, or noise | Nothing. Count it under "not filed" with the reason. |

Gap or tracker issue: a gap says a capability is missing and leaves the shape of the fix open, and triage promotes gaps to issues. Do not open an issue for something you are also filing as a gap. Propose a tracker issue only when the work is already concrete and it is this repository's to do. A request for a retro is not direction to file issues.

## 4. File gaps

Use the `bbox-gap` skill for classification, deduplication, the typed fields, scope and delivery checks. It is the one place those rules live. Dedupe against gaps already filed this session as well as the store.

In this repository a project-scoped gap is a file under `.bbox/gaps/` in the checkout you are in. Commit it by explicit path when you are authorized to commit, and say which branch carries it: a gap filed from a worktree is invisible to `bbox_gaps` until that branch reaches the published ref.

## 5. Record non-gap follow-ups as thread notes

One note per follow-up. Find an existing thread first and continue it; open a new one only when none fits.

```text
bbox_thread_list(project="<working checkout>")
bbox_thread(action="continue", id="<thread id>", note="<observation, consequence, suggested next step>")
bbox_thread(action="open", topic="Retro follow-up: <short title>", project="<working checkout>", note="<observation, consequence, suggested next step>. Suggested destination: <repo doc | project knowledge | global instruction | local runbook | none>.")
```

Separate what you observed from what you suggest. Keep private client identifiers out of anything recorded for this public repository.

## 6. Propose tracker issues

For each, give the operator a title, a two or three sentence body with the reproduction or acceptance criterion, and the evidence. File it only with the operator's approval for that issue; an approval already given for that exact issue stands.

## 7. Report

Verify each filing receipt and note anything queued but not yet published. Keep the reply to an accounting:

```text
Gaps filed: <id> <title> (<scope>; <queued | delivered | published>; <file to commit, if any>)
Gaps reused: <id> <dedupe_key>
Threads opened or continued: <id> <short title>
Tracker issues proposed: <title> (awaiting approval)
Rule proposals: <one line each> (awaiting approval)
Not filed: <category>: <reason>
Unavailable: <any filing tool missing from this session>
```

If a filing tool was missing, keep the full proposed item in the report, with its fields and evidence, and name the unavailable tool. Do not claim it was saved.
