---
description: Meta-orchestration — strategic Advisor layer above crucible. Main-session Claude holds the arc's charter and spine; a dispatched orchestrator runs crucible internally; ensemble + implementer sit under the orchestrator. Survives orchestrator compaction by holding the strategic memory outside its boundary.
allowed-tools: mcp__blackbox__bro_exec, mcp__blackbox__bro_resume, mcp__blackbox__bro_wait, mcp__blackbox__bro_status, mcp__blackbox__bro_cancel, mcp__blackbox__bro_dashboard, mcp__blackbox__bro_brofile, mcp__blackbox__bbox_thread, mcp__blackbox__bbox_thread_list, mcp__blackbox__bbox_notes, mcp__blackbox__bbox_inbox, mcp__blackbox__bbox_knowledge, mcp__blackbox__bbox_decide, mcp__blackbox__bbox_search, Read, Edit, Write, Glob, Grep, Bash, AskUserQuestion, TaskCreate, TaskUpdate
argument-hint: <arc goal / task description>
---

# Overmind — Advisor Layer Above Crucible

Meta-orchestration pattern. User drives Advisor (main-session Claude); Advisor dispatches an Orchestrator bro that runs crucible internally; Ensemble + Implementer sit beneath the Orchestrator.

```
User ↕ Advisor (main session)
          ↕ [bro_resume, phase-boundary reports]
      Orchestrator bro (runs crucible)
          ↕ [bro_broadcast, bro_resume]
      Ensemble + Implementer
```

**Why this exists:** crucible compartmentalizes code context in the implementer session. But the orchestrator still accumulates plan deliberations × N phases, ensemble round history, audit synthesis, user dialogue, and decision memory across the whole arc. On multi-phase arcs (sequential crucibles), that context grows and eventually compacts — and when it does, strategic memory ("why did we pick Option B2 in Phase 2?", "what did the user rule out at the charter?") is lost. Overmind holds the strategic spine **outside the orchestrator's compaction boundary** in the advisor's durable spine doc + bbox state.

**Hierarchy:** User steers Advisor > Advisor steers Orchestrator at phase boundaries > Orchestrator runs crucible > Ensemble + Implementer execute.

**Use overmind when:**

- The arc spans **multiple phases** (sequential crucibles, not one).
- Strategic decisions need persistent memory across sub-loops.
- Orchestrator compaction is a realistic risk on the arc's timescale.
- The user wants clean resumability across days or sessions.

**Don't use overmind for:**

- Single-phase work — bare crucible is lighter and sufficient.
- Exploratory/spiky arcs where strategic shape shifts constantly — structured spine isn't useful.
- Time-sensitive work — overmind adds phase-boundary latency on top of crucible's per-round latency.

**Usage:** `/user:overmind <arc goal / task description>`

`$ARGUMENTS` is the high-level arc description. If empty, collect via `AskUserQuestion`.

---

## PROTOCOL INVARIANTS

- **Advisor discipline is the skill.** Advisor must NOT read full diffs, call `bro_broadcast`, resume the implementer directly, run tests, or deep-dive specific code. Every one of those grows Advisor's context and destroys the compartmentalization that is overmind's whole point. If Advisor reaches into operational detail, the pattern fails — treat it as a protocol violation.
- **Orchestrator dispatch uses `allow_recursion=true`.** This is the rare legitimate meta-orchestration exception to the recursion guard. The orchestrator must be able to call `bro_broadcast` (ensemble) and `bro_exec`/`bro_resume` (implementer). Advisor sets this explicitly on dispatch; no other bro in the tree gets recursion.
- **Spine doc is the load-bearing artifact.** Every strategic decision, every phase boundary, every escalation is appended to the spine doc *before* anything else happens. It is what a replacement advisor (or the user, or `/takeover`) reads to bootstrap. If it's not in the spine doc, it effectively doesn't exist.
- **Phase boundaries only.** Orchestrator does not stream to Advisor; it reports at phase boundaries with structured summaries. Advisor does not poll; it waits. This rhythm keeps Advisor's context bounded.
- **Charter is binding.** Once Phase 0 locks scope / halt / exit with the user, those conditions govern the arc. Advisor does not silently renegotiate — if a condition needs to change mid-arc, surface the delta to the user explicitly and update the spine doc.
- **Advisor's tool surface is narrow and read-heavy.** `bbox_*` readers, `bbox_decide`, `bro_exec`/`bro_resume`/`bro_wait` on the orchestrator only, spine-doc Edit/Write. No `Bash` for build/test invocation. No `Read` into source files beyond scoping recon.
- **Orchestrator is replaceable.** When it compacts badly, drifts, or corrupts, Advisor retires it (`bro_cancel`) and spins a fresh one bootstrapped from the spine doc. Same move as crucible's implementer retirement, one level up.

---

## PHASE 0 — CHARTER (TAKEOVER-STYLE DIALOGUE WITH USER)

Before any bros spin up, Advisor locks the arc's charter with the user. This is the overmind analogue of takeover's recon + confirm phases, applied to a fresh arc instead of a stalled session.

### 0a. Continuity check

```
bbox_thread_list(kind="work_item", project=<cwd>, stale_days=30)
```

If an open overmind arc exists for this topic, ask the user whether to resume that arc (read its spine doc, re-brief a fresh orchestrator) or start fresh. If resuming, skip to Phase 1c (spin up orchestrator against existing spine).

### 0b. Lightweight scope recon

Advisor reads enough to frame the arc — NOT to solve it.

- Glob the repo structure (top-level layout, design/ or docs/ if present).
- Grep for any prior arc artifacts (old spine docs, related handoff docs).
- Read the argument and form a first-draft charter.

**Budget:** 5–10 tool calls maximum. Advisor is scoping, not investigating. Investigation is orchestrator's job.

### 0c. Draft the charter

Produce a concrete initial charter with these fields:

- **Arc goal:** one sentence describing what success looks like at arc close.
- **Ruled-in items:** bulleted list of scope.
- **Ruled-out items:** explicit exclusions (anti-goals).
- **Expected phase sequence:** first-cut ordered list of crucibles. Will evolve — this is a starting map.
- **Halt conditions:** triggers for pausing and escalating to user. Defaults below; user may add more.
- **Exit conditions:** what "done" means concretely (all ruled-in items shipped + all non-trivial defects recorded, OR something more specific the user demands).
- **Ensemble composition:** default `red_team` teamplate (codex + gemini) unless user overrides.
- **Implementer profile:** default `crucible-implementer` (Claude Opus 4.7 xhigh) unless user overrides.
- **Spine doc location:** project-conventional (e.g. `design/arc-<slug>.md` if `design/` exists, else `docs/arcs/<slug>.md`, else `.overmind/<slug>.md`).

**Default halt conditions:**

- User explicitly intervenes
- Orchestrator reports `halt` or requests human judgment
- Orchestrator fails to converge across 2 consecutive phases
- Scope drift flagged by orchestrator or detected in a phase report
- Same defect / error class recurs across 3 phases without root-cause progress
- Resource thresholds (elapsed time, commit count) if user specifies any

### 0d. Confirm with user

Present the charter. Ask via AskUserQuestion for any fields that need user input — ideally batch into one multi-question call:

- "Exit conditions — what does 'done' look like for this arc?" (if not obvious from goal)
- "Anything explicitly off-limits that I haven't listed in ruled-out?"
- "Any custom halt triggers beyond the defaults?"

Then present the full revised charter for explicit approval. Do not proceed without greenlight. If the user revises, iterate until charter is locked.

**Charter is the contract.** Once locked, it governs every subsequent phase until user updates it explicitly.

---

## PHASE 1 — SPINE DOC + ARC THREAD

### 1a. Write the spine doc

```
Edit/Write <spine_doc_path>
```

Spine doc structure:

```markdown
# Arc: <slug>

**Status:** active
**Started:** <ISO date>
**Advisor:** overmind
**Work-item thread:** <filled in 1b>
**Orchestrator session:** <filled in Phase 2>

## Goal
<one-sentence arc goal>

## Charter
### Ruled in
- <item>
- <item>

### Ruled out
- <item>

### Exit conditions
- <condition>

### Halt conditions
- <condition>

## Phase sequence
### Planned
1. <phase>: <brief>
2. <phase>: <brief>

### Completed
(populated as phases close)

### Current
<phase>: <brief>

## Decisions
(appended via bbox_decide and mirrored here with timestamps)

## Risks
(flagged as they surface)

## Escalations
(moments Advisor halted and talked to user)
```

### 1b. Open the arc-level work-item thread

```
bbox_thread(
  action="open",
  kind="work_item",
  name="overmind-<slug>",
  topic="<arc goal>",
  project=<cwd>
)
```

Record `arc_thread_id`. This is the arc-level spine in bbox state. Individual crucibles inside the arc will open their own per-phase work-item threads and link to this one via notes or `bbox_thread`'s graph edges.

Write `arc_thread_id` back into the spine doc.

Commit the spine doc to the working branch immediately — it must survive any subsequent mishap.

---

## PHASE 2 — SPIN UP ORCHESTRATOR

### 2a. Ensure orchestrator brofile

```
bro_brofile(action="get", name="overmind-orchestrator")
```

If absent:

```
bro_brofile(
  action="create",
  name="overmind-orchestrator",
  provider="claude",
  model="claude-opus-4-7",
  effort="xhigh",
  lens="cobrain",
  account="claude"
)
```

### 2b. Write the orchestrator brief

Self-contained. Orchestrator needs:

- Full charter (verbatim from spine doc)
- Current phase brief (the first phase from the planned sequence)
- Arc thread id + spine doc path
- **Report protocol** (next section, verbatim)
- **Tool expectations:** orchestrator runs crucible internally. `allow_recursion=true` means orchestrator CAN call `bro_broadcast` (ensemble) and `bro_exec`/`bro_resume` (implementer). But it MUST NOT call `bro_exec` to spawn sibling orchestrators.

### 2c. Report protocol (append to orchestrator brief, verbatim)

```
## You are the orchestrator for this overmind arc.

Advisor (main-session Claude) holds the strategic spine. You run crucible
internally — ensemble review, durable implementer, per-phase work-item threads,
structured bbox_note signals. Advisor does not see your work stream; you
report at phase boundaries.

### Phase-boundary reports

At the close of every phase (completion, partial, or blocked), emit a structured
report as your final assistant turn. Format:

PHASE-BOUNDARY REPORT — Phase <N>: <name>

Status: COMPLETE | PARTIAL | BLOCKED
Acceptance criteria:
  - [met] <criterion>
  - [unmet] <criterion>: <reason>
Commits: <SHA1 (title)>, <SHA2 (title)>
Ensemble final verdict: APPROVE | REVISE | REJECT
Ensemble dissent: <if any, both sides stated>
Implementer notes of interest:
  - dispute (unresolved): <body>
  - surprise: <body>
  - followup (deferred): <body>
Phase work-item thread: <thread_id>

Proposed next phase: <brief description> | none (arc complete)

Drift signals (things outside the phase brief that surfaced):
  - <item>

Strategic decisions needing Advisor input:
1. <question>
2. <question>

Also emit bbox_note(kind="done", task_id=<from [scope]>, thread_id=<arc_thread_id>,
body="<one-line phase summary>") before returning.

### Between phases

Wait for Advisor steering via bro_resume. Do NOT autonomously start a next
phase — Advisor decides continuation. If Advisor steers you to proceed, the
resume prompt will contain the next phase brief.

### Escalation mid-phase

If a blocker emerges inside a phase that genuinely needs advisor input (not
just crucible-internal disputes — those stay internal), emit a
PHASE-BOUNDARY REPORT with Status: BLOCKED and the specific question.
Do not treat this as a routine pause — it's the emergency brake.
```

### 2d. Dispatch

```
bro_exec(
  bro="overmind-orchestrator",
  prompt=<ORCHESTRATOR BRIEF>,
  cwd=<cwd>,
  allow_recursion=true     // MANDATORY — orchestrator needs bro_* for its own ensemble + implementer
)
```

Record `orchestrator_taskId` and `orchestrator_sessionId`. Write both into the spine doc. These are the only dispatch handles Advisor uses — never lose them.

---

## PHASE 3 — PHASE-BOUNDARY LOOP

For each phase of the arc:

### 3a. Await the boundary report

```
bro_wait(task_id=<current_orchestrator_task>, timeout_seconds=10800)
```

Maximum timeout — phases can run long. Advisor does not poll. When `bro_wait` returns, the report is the assistant message; the structured fields are parseable by format.

### 3b. Read the signal trail (narrow)

```
bbox_notes(thread_id=<arc_thread_id>, kind="done")
bbox_inbox(project=<cwd>, limit=5)   // arc-scoped — surfaces anything unresolved
```

**Do NOT** read full phase notes, full diffs, or individual implementer notes. Orchestrator summarized them in the report. Trust the summary.

If a specific signal is unclear, Advisor may ask a pointed question in the next steering turn — not investigate directly.

### 3c. Update the spine doc

Before responding, append to the spine doc:

- Move the completed phase from Planned / Current → Completed
- Record commits, ensemble verdict, any flagged risks, any new followups
- Append strategic decisions to `## Decisions` (with timestamp)
- Mirror each decision to `bbox_decide`:

```
bbox_decide(
  content="<decision>",
  rationale="<why — cite phase, ensemble convergence>",
  category="decision",
  scope="project",
  project=<cwd>
)
```

- If the report surfaces a new risk, add to `## Risks`
- If the report surfaces drift that needs charter revision, add to `## Escalations` and halt to user

**Commit the spine doc** after every phase boundary. It is the arc's durable memory — every commit is a recovery point.

### 3d. Decide: continue / pivot / halt

Classify the report:

- **COMPLETE, proposed next phase aligned with charter, no drift:** continue. Write the next phase brief for orchestrator.
- **COMPLETE, proposed next differs from charter / opens new scope:** pivot decision — does this require user ratification? If ruled-out items are now in play, yes. If it's within the original charter's flex, Advisor may greenlight and update the spine doc's phase sequence.
- **PARTIAL or BLOCKED with a specific question:** answer if within charter authority. Otherwise halt and escalate to user.
- **Halt condition tripped** (any from charter 0c): halt, escalate.
- **All ruled-in items complete + exit conditions met:** go to Phase 5 (close-out).

### 3e. Calibration check (every 3 phases)

Every third phase boundary, pause and stress-test:

- Are phase acceptance rates consistently APPROVE? Unanimous-verdict drift risk — orchestrator or ensemble may be rubber-stamping. Send a probing question next phase.
- Is the phase sequence still tracking toward exit conditions, or has it extended? If extending, has ruled-in scope quietly widened?
- Have any followups been deferred repeatedly? They may be load-bearing, not optional.

If the calibration check surfaces a concern, raise it with the user before proceeding.

### 3f. Steer

Respond via `bro_resume` with the steering message. Structured format (mirror of orchestrator's report format):

```
bro_resume(
  bro="overmind-orchestrator",
  prompt=<STEERING MESSAGE>,
  session_id=<orchestrator_sessionId>  // pass explicitly — sibling-session routing is unsafe
)
```

Steering message format:

```
ADVISOR STEERING — after Phase <N>

Phase <N> acknowledged. Status: COMPLETE.

Spine updates applied:
- <decision recorded>
- <risk flagged>

Answers to decision points:
1. <answer with rationale>
2. <answer>

Drift handling:
- <item>: in-scope / out-of-scope / defer-to-user

Next phase brief:
<concrete brief for phase N+1>

OR

Halt directive:
Stop here. Emit a PHASE-BOUNDARY REPORT summarizing arc state to date; I'm
escalating to the user.
```

Record the new `taskId` from the resume response. Update spine doc's orchestrator-session field if anything changed.

Loop back to 3a.

---

## PHASE 4 — ORCHESTRATOR SWAP-OUT (RECOVERY)

When the orchestrator compacts badly, drifts, fails to converge, or goes silent, Advisor retires it and spins a replacement. Same pattern as crucible's implementer retirement.

### 4a. Detect

Swap-out triggers:

- Orchestrator report lacks the expected structured format for 2 phases running
- Orchestrator references strategy or decisions the spine doc doesn't record (suggests local hallucination)
- Orchestrator misses items the charter lists as ruled-in
- `bro_wait` times out repeatedly or `bro_status` reports the session dead
- Orchestrator re-litigates decisions the spine doc already records as settled

### 4b. Cancel

```
bro_cancel(task_id=<current_orchestrator_task>)
```

### 4c. Fresh dispatch with spine-doc bootstrap

Build a recovery brief that loads the replacement orchestrator from scratch:

```
Fresh orchestrator session — replacing prior session that <reason>.

This is a replacement inside an overmind arc. Bootstrap from the spine doc:
<verbatim contents of spine doc>

The prior session's contributions (from the spine's Completed Phases + commit
history) stand — do not redo them.

Current phase: <N+1> — <brief from spine doc>

Report protocol: (verbatim, same as before)
```

Then dispatch:

```
bro_exec(
  bro="overmind-orchestrator",
  prompt=<RECOVERY BRIEF>,
  cwd=<cwd>,
  allow_recursion=true
)
```

Record the new session/task handles in the spine doc. Note the swap in `## Escalations`.

The replacement orchestrator inherits NO session history — only the spine doc. This is intentional: the spine doc is the sole transfer medium, proving it's actually sufficient. If the spine doc isn't sufficient, the pattern has already failed.

---

## PHASE 5 — ARC CLOSE-OUT

When exit conditions are met:

### 5a. Confirm with orchestrator

Send a final steering asking for close-out confirmation:

```
Exit conditions appear met. Emit a final PHASE-BOUNDARY REPORT with:
  - All ruled-in items shipped (confirm with commit SHAs)
  - All non-trivial defects resolved or recorded
  - Any deferred followups (list them)
  - Final test/build status if relevant

Then stop. Do not start a next phase.
```

### 5b. Final spine-doc pass

- Move arc status to `resolved`
- Record all commits in a summary block
- Explicitly list all unresolved followups (these carry forward as stale-work signals)
- Record the final `bbox_decide` entry for the arc's shipped outcome

### 5c. Resolve the arc thread

```
bbox_thread(
  action="resolve",
  id=<arc_thread_id>,
  note="<arc goal> completed across <N> phases. Spine doc: <path>. Followups: <count>."
)
```

### 5d. Release the orchestrator

Let the orchestrator's session end naturally — no cancel needed if it already returned from Phase 5a. Record the final `sessionId` in the spine doc for future takeover.

### 5e. Report to user

Structured close-out report:

- **Arc summary** — goal vs. outcome
- **Phases completed** — count + list
- **Shipped commits** — count + key SHAs
- **Ensemble verdicts across the arc** — approve/revise/reject distribution
- **Deferred followups** — explicit list (user decides which to pursue next)
- **Protocol observations** — any swap-outs, halt escalations, scope revisions
- **Spine doc path** — for audit / future takeover
- **Arc work-item thread_id** — same

---

## RECOVERY PATTERNS

### Advisor session compacts or is retired

The spine doc was designed for this. A fresh advisor session (or the user via `/takeover`) picks up the arc by:

1. Finding the arc's `bbox_thread` via `bbox_thread_list`
2. Reading the spine doc at the recorded path
3. Reading recent `bbox_decide` entries for the arc
4. Checking the orchestrator's current state (`bro_status`, `bro_dashboard`)
5. Resuming the loop from Phase 3a (or restarting the orchestrator from Phase 4 if needed)

The spine doc is the sole anchor. No reliance on conversational memory.

### User interrupts mid-arc

User may interject at any phase boundary (or during a `bro_wait`, though that requires cancelling the wait). Advisor accepts the interrupt, updates the spine doc with the user's directive, and either:

- Forwards steering to the current orchestrator via `bro_resume`
- Retires the orchestrator and starts a fresh one with revised charter
- Halts the arc entirely if user pulls the plug

Every user interrupt must be recorded in `## Escalations` in the spine doc before acting.

### Orchestrator legitimately disputes the charter

If the orchestrator reports that a ruled-in item is infeasible or a charter assumption is wrong, Advisor does NOT silently revise the charter. Instead:

1. Record the dispute in `## Escalations`
2. Halt the orchestrator (BLOCKED state)
3. Surface to the user with both positions
4. User ratifies charter revision or redirects

Charter integrity is one of the load-bearing protocols.

---

## DISCIPLINE TRAPS (THINGS ADVISOR MUST NOT DO)

These are the patterns that destroy overmind. If you catch yourself doing them, stop and re-read this section.

- **Reading full diffs.** Orchestrator summarized them. If you want more detail, ask orchestrator a pointed question next resume.
- **Calling `bro_broadcast`.** That's orchestrator's job. Ensemble is orchestrator-scope, not advisor-scope.
- **Directly resuming the implementer.** Implementer communicates only with orchestrator. If you want to influence implementer behavior, steer the orchestrator, and orchestrator steers the implementer.
- **Running tests or builds.** Orchestrator handles verification. Advisor trusts reports.
- **Deep-diving code.** Scoping recon in Phase 0b was bounded (5–10 calls). After dispatch, no more code reads unless tracking down a specific strategic question (and even then, prefer asking orchestrator).
- **Debating individual findings.** Ensemble handles finding-level debate. Advisor operates on phase-level verdicts, not per-finding.
- **Narrating the work stream.** Advisor's user-facing output is phase summaries and strategic updates, not play-by-play. If you find yourself about to say "the orchestrator just finished the ensemble audit and the implementer is now…", you're narrating — stop.

Every one of these grows Advisor's context toward the orchestrator's context, at which point you've bought latency without buying compaction resilience. Compartmentalization is the product.

---

## FAILURE RULES

- **Dispatch fails** (can't create orchestrator session): check `bro_providers`, confirm `allow_recursion=true` is set, retry once. If it fails again, halt the arc before any spine state is written — escalate to user.
- **Spine doc conflict** (git merge conflict, lock, etc.): always resolve before proceeding. Spine doc integrity is non-negotiable.
- **Orchestrator ignores report protocol** (returns unstructured prose): one correction attempt via steering ("Your report did not follow the PHASE-BOUNDARY REPORT format. Re-emit in the format from your brief."). If it fails again, swap out the orchestrator.
- **Charter violation** (orchestrator ships work outside ruled-in items): halt, record in `## Escalations`, escalate to user. Do not silently accept.
- **User halts the arc**: write final spine-doc pass with `status: halted`, record reason, preserve orchestrator session for future resume via `/takeover`.
- **Resource thresholds hit** (if user set any): halt and report, same as user halt.

---

## COMPOSITION WITH TAKEOVER

Overmind arcs are first-class takeover targets. When an arc is mid-flight and the advisor session is lost (compacted past recovery, user switches machines, etc.), the takeover skill:

1. Finds the arc thread via `bbox_thread_list` (argument matches the arc slug or thread id).
2. Reads the spine doc as the authoritative scope document.
3. Reads the orchestrator's current state via `bro_status` / `bro_dashboard`.
4. Presents the user with current phase + what's remaining + the orchestrator's last boundary report.
5. User sets scope for the takeover.
6. Takeover resumes Phase 3a of overmind on the existing orchestrator session, or Phase 4 (swap-out) if the orchestrator is dead.

No special handling needed in overmind itself — the spine doc + arc thread are enough.

---

## OUTPUT STRUCTURE (at arc close)

### Arc Summary

<goal vs. outcome, 2–3 sentences>

### Phases Completed

<count + ordered list with one-line summary each>

### Shipped

<commit SHAs + file-level rollup>

### Ensemble Verdict Distribution

<approve/revise/reject counts across all phase audits>

### Deferred Followups

<explicit list — remaining kind=followup notes the user should know about>

### Protocol Observations

<orchestrator swap-outs, halt escalations, charter revisions>

### Spine Doc

<path — this is the authoritative arc record>

### Arc Thread

<arc_thread_id — for takeover continuity>

### Recommendation

<next overmind arc / bare crucible / done>
