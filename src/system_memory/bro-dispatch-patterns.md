# Bro dispatch patterns — exec, resume, wait, race, deliberate

The orchestration surface is small, but the workflow shapes are different enough that putting all of them inline in tool docs bloats hot context. This runbook is the compact mental model.

## Core invariant

`bro_exec` starts a fresh session.

`bro_resume` continues an existing session.

If you want continuity, do not call `bro_exec` again.

## Standard patterns

### One worker

1. `bro_exec(...)`
2. `bro_wait(...)`
3. inspect result

### Blind deliberation

1. `bro_broadcast(...)`
2. `bro_when_all(...)`
3. compare answers
4. optionally `bro_resume(...)` selected members with follow-up prompts

### Race

1. `bro_broadcast(...)` or multiple `bro_exec(...)`
2. `bro_when_any(...)`
3. inspect the winning result
4. cancel laggards only if they are clearly wasted work

### Long-running check-in

1. `bro_exec(...)`
2. `bro_status(...)` occasionally
3. `bro_wait(...)` only when you actually need completion

## Team and brofile hygiene

- List before create. `bro_brofile(action="list")` before `create`; same for teams/templates.
- Prefer named bros over raw providers so model/account/lens/session routing stays consistent.
- Use `team::bro` when names are ambiguous across instantiated teams.

## Cancellation hygiene

Check `bro_status` before `bro_cancel`. A timeout does not tell you whether the provider is stuck, actively working, or failed silently.

Cancellation is for:

- a lost race
- a user stop request
- a truly stuck task

## What to keep hot vs cold

Keep these hot in tool docs:

- `exec` vs `resume`
- wait/all/any role distinctions
- named bro preferred

Keep these cold in this runbook:

- orchestration shapes
- race vs deliberation tradeoffs
- cancellation etiquette
- team/brofile hygiene
