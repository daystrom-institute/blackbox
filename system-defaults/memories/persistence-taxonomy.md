+++
title = "Persistence taxonomy: learn vs note"
tags = ["persistence", "taxonomy", "learn", "note", "memory", "runbook"]
order = 3
template = false
+++
# Persistence taxonomy: learn vs note

Use the persistence layer that matches the durability, audience, and speaker of the information. Most confusion here comes from mixing up "what the user told us," "what should stay hot for the current arc," and "what I observed while working."

## The split

- `bbox_learn` is the only knowledge write lane. It creates an entry, or updates one when given its `id`.
  - Rendered entries (the default, `render=true`): user-stated rules, conventions, bans, defaults, or preferences that should bind future sessions. They are rendered into managed memory, so every future agent sees them.
  - Recall entries (`category="memory", render=false`): useful facts worth finding later, but not worth loading every turn. Indexed only, never rendered.
  - Commitments: a design or workflow commitment is a `convention` entry whose content states the rule and the reason for it.
- `bbox_forget` retires an entry by deleting it. To replace an entry, `bbox_forget` the old one, then `bbox_learn` the new one. For project entries, git history of `.bbox/knowledge/<id>.json` carries prior versions and the reason for the change.
- `bbox_pin`: persisted but scope-limited ambient context for one active session, bro, thread, or work item. Never rendered into managed memory.
- `bbox_note`: workstream side-channel during execution. This is not durable policy memory; it is execution telemetry for the current loop.

## Speaker matters

If the USER states the rule, bias toward `bbox_learn` (rendered, or `render=false` for recall).

If the AGENT discovers something while working, bias toward `note(kind=learned)` or a `render=false` recall entry.

Do not store user directives as `bbox_note(kind=learned)`. That hides policy in a transient execution trail instead of putting it where later sessions will load it.

Durable entries are approved with the operator before the write; there is no review queue after it.

## Quick tests

### Use `bbox_learn` (rendered) when

- The statement would still matter if today's edit were reverted.
- The user is expressing a standing preference or prohibition.
- A future session should know it before touching the repo.
- The guidance is not tied to one active migration, initiative, or executor role.
- There is a real architectural or workflow commitment (a `convention` whose content carries the reason).

### Use `bbox_learn` with `render=false` when

- It is helpful context, not standing policy.
- You want searchability, not prompt residency.
- You are unsure whether it deserves rendered treatment.

### Use `bbox_pin` when

- The context must stay hot across turns for one active execution lane.
- The right audience is a matching session, bro, thread, or work item.
- Rendering it into repo agent files would be pollution.
- Examples include migration-phase guidance, active-arc sequencing, and temporary executor charters.

### Use `bbox_note` when

- You are in the middle of work and want the orchestrator to see a signal.
- The information is specific to this execution loop.
- The right consumer is the current reviewer/orchestrator, not every future session.

## Common failure modes

- Over-rendering: rendering facts that should stay cold instead of using `render=false`.
- Arc-to-policy corruption: using `learn` for migration plans, active initiative charters, or executor-role guidance just to keep them visible across turns.
- Under-persisting: keeping a user rule only in code or only in a note.
- Duplicating instead of replacing: when an entry changes, update it by `id` or forget it and learn the successor; do not leave two entries that disagree.
- Using `note` as memory: notes are execution breadcrumbs, not long-term policy.

## Practical default

If unsure between a rendered entry and a recall entry, choose `render=false`.

If unsure between `pin` and `learn`, ask: should an unrelated future agent inherit this by default? If no, `pin`.

If unsure between a recall entry and `note`, ask: should a future session know this before it starts? If yes, a `render=false` entry. If no, `note`.
