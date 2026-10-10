---
description: Orchestrator-led implementation workflow: durable pair-programmer implementer, continuous red-team ensemble, coordinated through a bbox work-item thread the orchestrator keeps
allowed-tools: mcp__blackbox__bro_exec, mcp__blackbox__bro_resume, mcp__blackbox__bro_wait, mcp__blackbox__bro_when_all, mcp__blackbox__bro_when_any, mcp__blackbox__bro_brofile, mcp__blackbox__bro_providers, mcp__blackbox__bro_status, mcp__blackbox__bro_cancel, mcp__blackbox__bro_dashboard, mcp__blackbox__bbox_thread, mcp__blackbox__bbox_thread_list, mcp__blackbox__bbox_knowledge, mcp__blackbox__bbox_learn, Read, Edit, Write, Bash, Glob, Grep, AskUserQuestion, TaskCreate, TaskUpdate
argument-hint: <task description>
---

# Crucible — Orchestrator + Continuous Ensemble + Durable Implementer

Substantial implementation work, coordinated by the main-session orchestrator through a **bbox work-item thread**, reviewed by a **continuous red-team ensemble** (codex + glm by default, with sustained per-member context across rounds), executed by a **durable implementer bro** (Opus 4.7 xhigh, context compartmentalizer held across rounds via `bro_resume`).

**Why this exists:** main-session context is finite and gets compacted. The implementer is a separate durable session that holds dense mechanical code context and survives your compactions. You keep orchestration, ensemble deliberation, and user dialogue in main; it keeps line numbers, call-site maps, and test-infra coupling. That's the whole point — compartmentalization, not delegation.

**Hierarchy:** User steers > Orchestrator (you) directs + synthesizes > Ensemble critiques in parallel > Implementer grounds, proposes, pushes back, cuts.

**Use this pattern when:**

- Multi-file implementation where main-session context compartmentalization is the primary benefit (you don't want 200KB of code reads in your compaction history).
- Cross-provider pre-ship consensus is worth the ceremony (migrations, protocol changes, security-sensitive logic).
- Refactors where "did we widen scope silently" is a real risk and you want an independent voice asking at both ends.
- Problems with architectural decision points where implementer Approach A vs B proposals earn their keep.

**Usage:** `/user:crucible <task description>`

`$ARGUMENTS` contains the task description. If empty, collect via `AskUserQuestion`.

---

## PROTOCOL INVARIANTS

- **Implementer is durable.** One `bro_exec` at start, then only `bro_resume`. A fresh `bro_exec` on a follow-up destroys the compartmentalized context that is crucible's whole point.
- **Ensemble continuity comes from each reviewer's own session.** Round 1 dispatches each member with `bro_exec(bro="<reviewer-brofile>")` and records its `taskId`/`sessionId`/provider; every later round is `bro_resume(session_id=<member sessionId>, provider=<member provider>)` per member, so each reviewer keeps its own session across rounds. Resume a single member only for bilateral recovery (one reviewer died, one needs a correction).
- **Reviewers are blind to each other within a round.** Orchestrator is the synthesizer — quote each side back to the others in next-round prompts. Cross-pollination is deliberate when orchestrator chooses it, not a default.
- **The orchestrator keeps the work-item thread.** Implementer and reviewers file nothing: each final answer is its report. The orchestrator appends the verdicts, disputes and decisions it acts on as thread notes (`bbox_thread(action="continue", id=<thread_id>, note=...)`), so the signal trail survives orchestrator compaction.
- **Mechanical recursion guard stays on.** Implementer and reviewers cannot call `bro_*` — do NOT set `allow_recursion=true`. They are executors.
- **Resume by recorded handle.** `bro_resume` takes the `session_id` and `provider` a prior dispatch returned. Record the `taskId`/`sessionId` returned by your most recent `bro_exec` or `bro_resume` for every bro in the arc.
- **Turn discipline.** Ensemble rounds stop when a voice re-raises a prior concern: either produce concrete evidence to refute or concede. Never retreat on pressure alone, never rubber-stamp. Cap at 8 ensemble rounds per work-item before halting and escalating to the user.

---

## PHASE 0 — FRAME

1. Parse `$ARGUMENTS`. If empty, ask.
2. Read the relevant code **yourself** (orchestrator owns ground truth). Use Glob / Grep / Read. Do not delegate — you need the shape to write the plan and to judge ensemble verdicts later.
3. Check for continuity:

   ```
   bbox_thread_list(kind="work_item", project=<cwd>, stale_days=14)
   ```

   If an open work-item exists on this topic, ask the user whether to resume (reuse thread_id, re-brief existing reviewer sessions and implementer) or open fresh.
4. State the problem to the user: what's in scope, what's explicitly out, known unknowns. Wait for greenlight. **Do not auto-proceed.**

---

## PHASE 1 — WORK-ITEM + ROLLBACK CHECKPOINT

### 1a. Open the work-item thread

```
bbox_thread(
  action="open",
  kind="work_item",
  name="<short slug>",
  topic="<one-line description>",
  project=<cwd>
)
```

Record `thread_id`. Every dispatch in this run carries it.

### 1b. Rollback checkpoint

Commit on a working branch or stash. Record the reference. Required before the implementer cuts in Phase 4. If the repo isn't git-tracked, ask the user how to handle rollback before continuing.

---

## PHASE 2 — PROPOSE + ENSEMBLE PRE-WORK REVIEW

### 2a. Draft the plan

Concrete plan: files to touch, functions to add/change, new types, migration order, test surface, rollback path, acceptance criteria as a bulleted checklist.

### 2b. Choose the ensemble (once per crucible)

Default ensemble = two reviewer brofiles, one codex and one glm (for example `red-team-codex` and `red-team-glm`). List what exists first:

```
bro_brofile(action="list")
```

…or dispatch ad hoc with `provider=`. Minimum viable is two cross-provider voices; three is better when the topic has architectural weight (add a third provider such as kimi or deepseek, checked with `bro_providers`).

### 2c. Brief the ensemble (first round: carries problem space)

One dispatch per member, recording each returned `taskId`, `sessionId` and provider:

```
bro_exec(
  bro="<reviewer-brofile>",
  prompt=<FIRST-ROUND PROMPT>,
  cwd=<cwd>
)
```

Then `bro_when_all(task_ids=[<member taskIds>])`. Reviewers retain this context across all subsequent rounds. First-round prompt shape:

```
Task: <task_description>
Problem space: <what we're working on, why, what changed upstream if anything>
Proposed plan: <full plan>
Out of scope: <bullets>
Acceptance criteria: <bullets>
Work-item thread: <thread_id>

You are on the red-team ensemble for this crucible arc. You will see multiple
rounds: plan review, work-product audit, fixup audits. Maintain continuity —
subsequent rounds will reference prior positions and show deltas, not restate
context.

This round: critique the plan. Identify:
- Missing steps, unaddressed risks, regression surfaces
- Ambiguities that force the implementer to guess
- Scope bombs (small-looking work that will explode)
- Over-engineering (scope that should be cut)

Output format:
  Verdict: PROCEED-AS-SCOPED | REVISE | STOP-AND-DISCUSS-X
  Findings: <numbered, terse, cite specifics>
  Under 200 words.

Your final answer is your report. Start it with the Verdict line; mark any
position you want escalated as DISPUTE.
```

### 2d. Join

```
bro_when_all(task_ids=[<member taskIds>], timeout_seconds=600)
```

### 2e. Synthesize

Read each member's final answer from the join (`bro_status(task_id, tail=N)` if one is truncated), then record the round on the thread:

```
bbox_thread(action="continue", id=<thread_id>, note="Plan round 1: codex <verdict>, glm <verdict>; disputes: <...>")
```

Classify findings: agreed / majority / minority / contradictory. Revise plan incorporating agreed+majority concerns. Reject minorities only with concrete evidence — cite it. When two reviewers contradict, pick the side with stronger evidence and note the dissent for the round-2 prompt.

### 2f. Convergence rounds (as needed)

For each follow-up round, send a **delta-shaped** prompt to every member with `bro_resume(session_id=<member sessionId>, provider=<member provider>, prompt=...)`: quote each reviewer's prior position and pose the specific remaining disagreement. Each resume continues that reviewer's session:

```
ROUND 2 — plan revision.

Round 1:
- codex said: "<quote>"
- glm said: "<quote>"

Revised plan addresses <X> and <Y>. Unresolved: <Z>.
Codex: <pointed question on Z>
GLM: <pointed question on Z>

Same verdict format. Under 150 words.
```

Stop when either:
- All reviewers return PROCEED-AS-SCOPED, or
- A reviewer re-raises an already-addressed concern without new evidence → orchestrator either produces refuting evidence or concedes. Never rubber-stamp. Never retreat on pressure alone.

Cap at 4 pre-work rounds. Hit the cap → halt, surface to user.

Record the converged plan as a labeled block. This is the contract for Phase 3.

---

## PHASE 3 — WORK PACKET + IMPLEMENTER SPIN-UP

### 3a. Ensure the implementer brofile exists

```
bro_brofile(action="get", name="crucible-implementer")
```

If absent:

```
bro_brofile(
  action="create",
  name="crucible-implementer",
  provider="claude",
  model="claude-opus-4-7",
  effort="xhigh",
  lens="cobrain",   // or "generic" — workman lens, not reviewer lens
  account="claude"
)
```

### 3b. Write the work packet

Self-contained brief. Include everything the implementer needs — orchestrator's main context may get compacted, the implementer session must not need to reach back:

- **Converged plan** (verbatim from 2f)
- **Context excerpts**: file paths + key line ranges + relevant functions
- **Acceptance criteria**: bulleted, testable
- **Rollback reference** (commit/stash from 1b)
- **Out-of-scope list**: explicit, so the implementer doesn't drift
- **Known gotchas**: anything the ensemble surfaced
- **Ensemble convergence notes**: *"ensemble converged on Option B2 because…"* — grounds, not just instructions
- **Work-item thread_id**: context for continuity; the orchestrator records to it
- **Expectations contract** (next section, verbatim)

### 3c. Expectations contract (append to work packet)

```
## You are the implementer on this crucible arc.

You are a durable peer, not a subagent. The orchestrator will resume this
session multiple times across the work-item's lifetime. Context
compartmentalization is the whole point — you hold the mechanical details,
orchestrator holds strategy and ensemble synthesis.

### Grounding discipline

Before first commit on any slice, run verification in parallel:
- grep for usages of the symbols you're about to touch
- check call-site counts match the brief's assumptions
- check adjacent tests aren't depending on current shape

Report what you found before cutting. If ground-truth diverges from the brief,
say so in your answer (SURPRISE or DISPUTE) and stop for steering.

### Proposals before cutting (when warranted)

If the fix admits multiple clean decompositions, present Approach A vs B with
tradeoffs and ask. Don't silently pick. The orchestrator wants architectural
pressure at decision points, not just mechanical edits.

### Pushback

Raise pushback in your answer AS SOON AS you notice, with the right label:
- DISPUTE: brief is wrong, or a premise is contradicted by code
- ASSUMPTION: you resolved an ambiguity by judgment; state what you chose
- SURPRISE: expected X, found Y (code differs from plan's assumptions)
- BLOCKED: you cannot proceed; include the specific reason
- FOLLOWUP: spotted out-of-scope work; defer, do NOT widen scope

You file nothing. Your answer at each pause is your report; the orchestrator
records what it acts on.

### Completion

End the final turn of any packet with a DONE line: a concrete one-line
acceptance summary.

If you stop short of acceptance criteria, still end with DONE and list what's
incomplete.

### Never

- Never commit without an explicit gate from the orchestrator on slices the
  brief didn't pre-authorize
- Never silently widen or narrow scope; raise DISPUTE or FOLLOWUP instead
- Never skip the grounding checks even when the brief looks obvious
```

### 3d. Launch

```
bro_exec(
  bro="crucible-implementer",
  prompt=<WORK PACKET>,
  cwd=<cwd>,
  allow_recursion=false
)
```

**Record `taskId`, `sessionId` and provider immediately.** These are your handles for every subsequent `bro_resume` on this arc.

---

## PHASE 4 — IMPLEMENTER EXECUTES (MULTI-PAUSE RHYTHM)

Expect 2–4 pauses per packet. The typical shape:

1. Implementer runs grounding checks → pauses with findings (may include DISPUTE/SURPRISE items)
2. Orchestrator steers (accept, redirect, or approve Approach A vs B) → `bro_resume` with decision
3. Implementer cuts first slice, verifies, commits → pauses with result
4. Orchestrator reviews diff + answer → `bro_resume` with next slice instruction or "proceed to next"
5. Repeat until packet acceptance criteria hit, implementer reports DONE

### 4a. Awaiting a pause

```
bro_wait(task_id=<current_task_id>, timeout_seconds=3600)
```

### 4b. Mid-packet ensemble activity (parallel)

Ensemble may be deliberating in parallel on a disputed approach while the implementer works an earlier slice. When ensemble converges on something the implementer hasn't seen, brief the delta explicitly on the next resume:

```
bro_resume(
  session_id=<implementer sessionId>,
  provider=<implementer provider>,
  prompt="Ensemble round on your dispute signal converged on Option B2
          (server-side X). Here's the rationale: <brief>. Apply B2 to the
          remaining slices. Prior slices (commits <SHA1>, <SHA2>) stand."
)
```

This is the **info-asymmetry protocol** — exec can't see ensemble threads, so orchestrator carries the synthesis across.

### 4c. Dispute-driven phase spawn

If an implementer DISPUTE reveals a materially different alternate path that the ensemble agrees with, either:

- **Steer within the current work-item** (most cases): resume the implementer with the new direction, continue.
- **Open a new sub-phase work-item** (when the dispute opens a distinct body of work): open another `bbox_thread(kind="work_item")` for the new phase and resume the implementer with the new thread_id in the brief. Keep the original thread open if work there is still ongoing, or resolve it if the dispute has entirely superseded the original plan.

### 4d. Resume discipline

- Every resume: pass the recorded `taskId` or `sessionId` if there's any ambiguity. Never assume `bro:"..."` lands on the right session.
- Resume prompts are **deltas** — the implementer remembers. Include only: what the last pause returned, what you decided, what's next.
- If the implementer is unresponsive or the session is clearly polluted, retire it: `bro_cancel`, then a FRESH `bro_exec` with a recovery brief carrying forward the critical prior-session context (see Phase 7 recovery pattern).

### 4e. After DONE

Read the implementer's final answer and the diff, then record the packet on the thread:

```
git diff <rollback_ref>...HEAD -- <relevant paths>
bbox_thread(action="continue", id=<thread_id>, note="Packet <X> DONE: <summary>; disputes, surprises, followups: <...>")
```

**Commit your own orchestrator assessment before Phase 5** — what's right, what's missing, what's risky, whether it matches the converged plan. Write it down explicitly. This is the anti-anchoring move: you do not want the ensemble's post-work verdicts reshaping your own read.

---

## PHASE 5 — POST-WORK ENSEMBLE AUDIT

### 5a. Send the audit prompt (delta-shaped: reviewers already know the plan)

One resume per member, then `bro_when_all(task_ids=[<new member taskIds>])`:

```
bro_resume(
  session_id=<member sessionId>,
  provider=<member provider>,
  prompt=<AUDIT PROMPT>
)
```

Audit prompt shape:

```
AUDIT — implementation complete.

Converged plan: <you already reviewed it in round 1; key acceptance criteria:>
  - <bullet>
  - <bullet>

Work product:
  Commits: <SHA1 (title)>, <SHA2 (title)>, ...
  Diff summary: <git diff --stat output>
  Targeted hunks / full diff: <inline, or per-file>
  Implementer signals of interest:
    - dispute: <body>
    - surprise: <body>
    - followup (deferred): <body>

Audit for:
- Correctness against the plan you reviewed
- Regressions in adjacent code
- Incomplete acceptance criteria
- Silent scope widening or narrowing
- Missing tests, unhandled edge cases

Format:
  Verdict: APPROVE | REVISE | REJECT
  Findings: <numbered, cite file:line>
  Under 200 words.

Your final answer is your report. Start it with the Verdict line; mark
positions you want escalated as DISPUTE.
```

### 5b. Join + synthesize

```
bro_when_all(task_ids=[<new member taskIds>], timeout_seconds=600)
bbox_thread(action="continue", id=<thread_id>, note="Audit round <N>: <per-member verdicts>; disputes: <...>")
```

### 5c. Three-way convergence

Now you have: orchestrator's committed assessment (4e) + ensemble verdicts. For each issue raised:

- **Orchestrator + ensemble unanimous** → must fix.
- **Ensemble majority, orchestrator disagrees** → argue with evidence from the diff. If your evidence is strong, document dissent and skip. Otherwise concede.
- **Single reviewer raises** → judgment call, default to including unless contradicted.
- **Orchestrator-only concern** → fix; the ensemble didn't catch it but you did.

Produce a concrete **fixup list** scoped to one implementer round. Larger rewrites → multiple rounds. Record it on the thread:

```
bbox_thread(action="continue", id=<thread_id>, note="Fixup list round <N>: <items>; addressed: <prior items>")
```

### 5d. Calibration check

If you've agreed with the implementer or ensemble on every pushback so far, pause. Unanimous agreement is either good calibration or weak stress-testing. Pick one specific pushback this round and actively argue the opposite position for yourself before accepting it. If you can't build a case, agreement is earned. If you can, run it past the ensemble as a separate question.

---

## PHASE 6 — FIXUP ROUNDS

For each fixup round (cap: 3 rounds before escalating to user):

### 6a. Resume — never exec

```
bro_resume(
  session_id=<implementer sessionId>,
  provider=<implementer provider>,
  prompt=<FIXUP BRIEF>
)
```

Fixup brief shape:

```
Fixup round <N>.

Converged audit findings:
1. <finding> — fix: <concrete change>  (ref: <file:line>)
2. ...

Out of scope this round: <deferred items>

Same thread_id. Same pushback contract. Grounding discipline still applies —
re-verify before re-cutting.
```

### 6b. Await + decide audit depth

```
bro_wait(task_id=<new_task_id>, timeout_seconds=3600)
```

Then choose:

- **Small fixup, low risk:** orchestrator verifies directly (read diff, targeted checks). Skip the ensemble re-audit. Move to Phase 7.
- **Substantial fixup or correctness-critical:** resume every member with a Phase 5 audit prompt variant (reviewers still have full context).

### 6c. Exit

Round exits when:
- Orchestrator + ensemble agree no material issues remain, OR
- Remaining issues are explicitly deferred as followups (recorded as thread notes, listed in Phase 7 output).

Cap hit without convergence → halt, surface to user with both positions.

---

## PHASE 7 — CLOSE-OUT

### 7a. Protocol-violation sweep

Before resolving the thread, scan for protocol issues that went unaddressed during the run (exec committed without gate, silent scope widening the orchestrator noticed but didn't raise live, ensemble anomalies, etc.). Park them as thread notes for later user conversation; don't derail close-out:

```
bbox_thread(
  action="continue",
  id=<thread_id>,
  note="Protocol: exec committed <commit_SHA> without gate during Packet B. Not corrective now, raise in next standup."
)
```

### 7b. Durable decision record

```
bbox_learn(
  content="<what shipped and what was chosen>. Reason: <why; cite ensemble convergence, key tradeoffs>",
  category="memory",
  render=false,
  scope="project",
  project=<cwd>
)
```

### 7c. Resolve the thread

```
bbox_thread(
  action="resolve",
  id=<thread_id>,
  note="<short summary of shipped work + deferred followups>"
)
```

### 7d. Report to user

- **Summary** — 2–3 sentences on shipped work
- **Converged findings** — ensemble + orchestrator agreement
- **Changes applied** — file-level summary with commit SHAs
- **Ensemble dissent** — non-converged positions with both sides stated
- **Deferred follow-ups**: explicit (followups recorded on the thread)
- **Protocol-violation parks** — anything raised in 7a
- **Rollback reference** — still valid
- **Work-item thread_id** — for future continuity

---

## RECOVERY PATTERNS

### Wrong-session routing

Symptom: `bro_resume` returns a stale cached result instead of a fresh response. Named-bro resolution picked a sibling session.

Response: abandon that resume. Record the correct `sessionId`/`taskId` from your last confirmed successful exec/resume on this arc. Re-resume with explicit identifiers. If you can't recover the right session, retire and restart (next pattern).

### Session retirement + fresh-start carry-forward

Symptom: implementer session is unresponsive, polluted, or just demonstrably on a wrong session.

Response:
1. `bro_cancel` the dead task (if still running).
2. Fresh `bro_exec` with a **recovery brief** that explicitly carries forward critical prior-session context:

   ```
   Fresh session — you're the implementer on this crucible arc.

   Previous session ran packets <A>, <B>; key outcomes:
     - <commit SHAs shipped>
     - <dispute signal on packet B re: X — ensemble converged on Option B2>
     - <pending: Y>

   Current packet: <brief for next unit of work>
   Thread: <same thread_id — you're continuing the same work-item>
   ```

Carry forward signals, not raw transcripts. The new session needs memory, not archaeology.

### Reviewer died mid-round

Symptom: `bro_when_all` times out on one member, others completed.

Response: for that member alone, `bro_resume(session_id=<member sessionId>, provider=<member provider>, prompt="Your prior task died. Re-asking with same prompt: <prompt>")`. Then `bro_wait(task_id=<new_task_id>)`. Other members' context is intact.

---

## FAILURE RULES

- **Ensemble voice unavailable** (provider offline): confirm via `bro_providers`, proceed with degraded coverage, note the gap in Phase 7 output. Never present a single-voice review as ensemble consensus.
- **Turn caps hit** (4 pre-work rounds, 3 fixup rounds): halt, escalate to user. Don't push through.
- **Mechanical recursion guard triggers** on a reviewer/implementer: expected, don't disable. They're not orchestrators.
- **Work packet too large** (past ~30KB): split across multiple implementer rounds. The implementer holds task context in the session; packets carry deltas.
- **Implementer disputes the core premise repeatedly**: stop. Re-open the plan. Resume the ensemble with the revised plan. The implementer is on the ground, trust ground-truth over plan-on-paper.

---

## BAD USES

- Trivial single-file fixes — direct edits, skip crucible.
- Exploratory / spiky work where the plan shifts round-by-round — informal pairing fits better.
- Iterative refinement on a single document — a document-loop pattern fits better.
- Two-voice rigor without a durable implementer — a simpler adversarial-review pattern fits better.
- Time-sensitive work — crucible has real per-round latency (ensemble blocks on the slowest reviewer, implementer pauses add context-switches).

---

## OUTPUT STRUCTURE

### Summary

<2–3 sentences on what shipped>

### Converged Findings

<ensemble + orchestrator agreement>

### Changes Applied

<commit SHAs + file-level summary>

### Ensemble Dissent

<non-converged positions, both sides>

### Deferred Follow-ups

<explicit list: followups recorded on the thread>

### Protocol Parks

<anything from 7a>

### Rollback Reference

<commit/stash ref from 1b>

### Work-Item Thread

<thread_id>

### Recommendation

<another crucible pass, or done>
