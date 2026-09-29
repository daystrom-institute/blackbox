+++
title = "Persistence taxonomy: learn vs thread"
tags = ["persistence", "taxonomy", "learn", "thread", "memory", "runbook"]
order = 3
template = false
+++
# Persistence taxonomy: learn vs thread

Use the persistence layer that matches the durability, audience, and speaker of the information. Most confusion here comes from mixing up "what the user told us," "what should stay hot for the current arc," and "what I observed while working."

## The split

- `bbox_learn` is the only knowledge write lane. It creates an entry, or updates one when given its `id`.
  - Rendered entries (the default, `render=true`): user-stated rules, conventions, bans, defaults, or preferences that should bind future sessions. They are rendered into managed memory, so every future agent sees them.
  - Recall entries (`category="memory", render=false`): useful facts worth finding later, but not worth loading every turn. Indexed only, never rendered.
  - Commitments: a design or workflow commitment is a `convention` entry whose content states the rule and the reason for it.
- `bbox_forget` retires an entry by deleting it. To replace an entry, `bbox_forget` the old one, then `bbox_learn` the new one. For project entries, git history of `.bbox/knowledge/<id>.json` carries prior versions and the reason for the change.
- `bbox_thread`: the durable record of one line of work. Notes are entries on a thread, not a separate store. When a dispatched agent's work needs a durable record, the orchestrating agent creates the thread and links it; the dispatched agent's final answer is its report.

Guidance that should stay hot for the current arc is not a persistence lane: it belongs in the dispatch brief or the work-item thread (`bbox_thread`).

## Speaker matters

If the USER states the rule, bias toward `bbox_learn` (rendered, or `render=false` for recall).

If the AGENT discovers something while working, bias toward a note on the work-item thread or a `render=false` recall entry.

Do not store user directives as thread notes. That hides policy in an execution trail instead of putting it where later sessions will load it.

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

### Keep it in the dispatch brief or thread when

- The context must stay hot across turns for one active execution lane.
- The right audience is the executor of this arc, not every future session.
- Rendering it into repo agent files would be pollution.
- Examples include migration-phase guidance, active-arc sequencing, and temporary executor charters.

### Add a thread note when

- You are orchestrating work and want a durable record of a signal from it.
- The information is specific to this line of work.
- The right consumer is whoever picks up this thread, not every future session.

## Common failure modes

- Over-rendering: rendering facts that should stay cold instead of using `render=false`.
- Arc-to-policy corruption: using `learn` for migration plans, active initiative charters, or executor-role guidance just to keep them visible across turns.
- Under-persisting: keeping a user rule only in code or only in a note.
- Duplicating instead of replacing: when an entry changes, update it by `id` or forget it and learn the successor; do not leave two entries that disagree.
- Using thread notes as memory: notes are execution breadcrumbs, not long-term policy.

## Practical default

If unsure between a rendered entry and a recall entry, choose `render=false`.

If unsure whether guidance is standing policy or active-arc context, ask: should an unrelated future agent inherit this by default? If no, it goes in the dispatch brief or the work-item thread, not `learn`.

If unsure between a recall entry and a thread note, ask: should a future session know this before it starts? If yes, a `render=false` entry. If no, a thread note.
