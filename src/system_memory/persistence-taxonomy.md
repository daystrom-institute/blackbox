# Persistence taxonomy — learn vs remember vs decide vs note

Use the persistence layer that matches the durability, audience, and speaker of the information. Most confusion here comes from mixing up "what the user told us," "what should stay hot for the current arc," and "what I observed while working."

## The split

- `bbox_learn` — user-stated rules, conventions, bans, defaults, or preferences that should bind future sessions. Rendered into managed memory, so every future agent sees them.
- `bbox_remember` — useful facts worth finding later, but not worth loading every turn. Indexed only.
- `bbox_decide` — commitments with rationale and optional supersession chain. Use when the team is locking in or reversing a design choice.
- `bbox_pin` — persisted but scope-limited ambient context for one active session, bro, thread, or work item. Never rendered into managed memory.
- `bbox_note` — workstream side-channel during execution. This is not durable policy memory; it is execution telemetry for the current loop.

## Speaker matters

If the USER states the rule, bias toward `learn` / `remember` / `decide`.

If the AGENT discovers something while working, bias toward `note(kind=learned)` or `remember`.

Do not store user directives as `bbox_note(kind=learned)`. That hides policy in a transient execution trail instead of putting it where later sessions will load it.

## Quick tests

### Use `bbox_learn` when

- The statement would still matter if today's edit were reverted.
- The user is expressing a standing preference or prohibition.
- A future session should know it before touching the repo.
- The guidance is not tied to one active migration, initiative, or executor role.

### Use `bbox_remember` when

- It is helpful context, not standing policy.
- You want searchability, not prompt residency.
- You are unsure whether it deserves the stronger `learn` treatment.

### Use `bbox_pin` when

- The context must stay hot across turns for one active execution lane.
- The right audience is a matching session, bro, thread, or work item.
- Rendering it into repo agent files would be pollution.
- Examples include migration-phase guidance, active-arc sequencing, and temporary executor charters.

### Use `bbox_decide` when

- You need rationale attached.
- There is a real architectural or workflow commitment.
- You may need to supersede a prior decision later.

### Use `bbox_note` when

- You are in the middle of work and want the orchestrator to see a signal.
- The information is specific to this execution loop.
- The right consumer is the current reviewer/orchestrator, not every future session.

## Common failure modes

- Over-rendering: using `learn` for facts that should stay cold.
- Arc-to-policy corruption: using `learn` for migration plans, active initiative charters, or executor-role guidance just to keep them visible across turns.
- Under-persisting: keeping a user rule only in code or only in a note.
- Using `decide` as a fancy `remember`: if no rationale or commitment is needed, it is probably not a decision.
- Using `note` as memory: notes are execution breadcrumbs, not long-term policy.

## Practical default

If unsure between `learn` and `remember`, choose `remember`.

If unsure between `pin` and `learn`, ask: should an unrelated future agent inherit this by default? If no, `pin`.

If unsure between `remember` and `note`, ask: should a future session know this before it starts? If yes, `remember`. If no, `note`.
