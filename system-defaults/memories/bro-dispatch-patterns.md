+++
title = "Bro dispatch patterns — exec, resume, wait, race, deliberate"
tags = ["bro", "dispatch", "resume", "wait", "orchestration", "patterns", "runbook"]
order = 5
template = false
+++
# Bro dispatch patterns — exec, resume, wait, race, deliberate

The orchestration surface is small, but the workflow shapes are different enough that putting all of them inline in tool docs bloats hot context. This runbook is the compact mental model.

## Core invariant

`bro_exec` starts a fresh session.

`bro_resume` continues an existing session.

If you want continuity, do not call `bro_exec` again.

Record the `{taskId, sessionId}` returned by every `bro_exec` and
`bro_resume`. Use those explicit handles for later waits and resumes whenever
there is any chance of sibling sessions, dashboard clutter, or another
external operator working in the same daemon.

One provider session is single-flight: do not call `bro_resume` for a
session while that session's previous task is still running. First call
`bro_wait(task_id=...)` and use the returned result, or call
`bro_cancel(task_id=...)` if you are deliberately abandoning that turn.
Starting another resume before the prior one reaches a terminal state can
fork/corrupt provider session history.

`bro_dashboard` is shared lookup state, not an ownership grant. Do not take
over, resume, cancel, prune, or dissolve a bro/team/task created by another
external session unless the user explicitly asked you to operate on that work.

## Standard patterns

### One worker

1. `bro_exec(...)`
2. record returned `taskId` and `sessionId`
3. `bro_wait(...)`
4. inspect result
5. only then `bro_resume(session_id=...)` if you need a follow-up

If the task failed but the provider session is still useful, prefer
`bro_resume` with recovery context over a fresh `bro_exec`. Start fresh only
when the session is polluted, genuinely lost, or intentionally independent.

### Blind deliberation

1. `bro_broadcast(...)`
2. `bro_when_all(...)`
3. compare answers
4. optionally `bro_resume(...)` selected members with follow-up prompts,
   but only after the selected members' current tasks are terminal

`bro_broadcast` resumes existing team member sessions on later rounds, so it
obeys the same single-flight rule as `bro_resume`: do not broadcast a new
round to a member while that member's prior task is still running.

### Race

1. `bro_broadcast(...)` or multiple `bro_exec(...)`
2. `bro_when_any(...)`
3. inspect the winning result
4. cancel laggards only if they are clearly wasted work

Use `bro_when_all` for fan-out/fan-in and `bro_when_any` for races. Do not
hand-roll sequential wait/poll loops when one of those primitives expresses
the coordination shape.

### Long-running check-in

1. `bro_exec(...)`
2. `bro_status(...)` occasionally
3. `bro_wait(...)` only when you actually need completion
4. do not resume the same session while `bro_status` still reports
   `running`

A `bro_wait` timeout is a snapshot, not a death certificate. Before calling a
bro dead, cancelling, or replacing it, call `bro_status(task_id=..., tail=N)`.
The task may be thinking, running tests, rate-limited, or waiting on a slow
provider.

## Team and brofile hygiene

- List before create. `bro_brofile(action="list")` before `create`; same for teams/templates.
- Prefer named bros over raw providers so model/account/lens/session routing stays consistent.
- Use `team::bro` when names are ambiguous across instantiated teams.
- Dissolve ad hoc teams you created after all member tasks are terminal:
  `bro_team(action="dissolve", name="...", cancel_running=false)`.
- Prune terminal task clutter you created with `bro_prune`; never use pruning
  as a substitute for cancelling or waiting on running work.

## Cancellation hygiene

Check `bro_status` before `bro_cancel`. A timeout does not tell you whether the provider is stuck, actively working, or failed silently.

Cancellation is for:

- a lost race
- a user stop request
- a truly stuck task

## What to keep hot vs cold

Keep these hot in tool docs:

- `exec` vs `resume`
- explicit task/session handles
- dashboard is lookup, not ownership
- status before cancel/dead
- wait/all/any role distinctions
- named bro preferred
- cleanup only what you created

Keep these cold in this runbook:

- orchestration shapes
- race vs deliberation tradeoffs
- cancellation etiquette
- team/brofile hygiene
