---
title: "Daily Boot Sequence"
kind: operator-prompt
corpus: blackbox-prompts
audience: interactive
topic:
  - prompts
  - boot
brief: "The daily operating rhythm: clean → survey residuals & net-news → process gaps → refine & process the TODO list → closeout. Orchestrates the dedicated daily prompts (daily-cleaning, gap-processing, RETRO_INTERACTIVE, CLOSEOUT) and owns the TODO-refinement loop + closeout that have no other home. Assumes CLAUDE.md / PROJECT.md are already loaded — it does NOT restate ambient/system facts or corpus layout; it captures only the daily flow and the tooling that isn't documented there."
---

# Daily Boot Sequence

The conductor for a day's work. It does not repeat what CLAUDE.md / PROJECT.md
already say (environment, conventions, corpus map, tool catalog — assume those
are loaded). It sequences the recurring daily activities and owns the two that
have no dedicated prompt: the **TODO-refinement loop** and **closeout**.

"Residuals" = state left over from prior sessions (open threads, unresolved
gaps, deferred work, uncommitted strays). "Net-news" = what changed since
(new commits on main, gaps filed overnight, peer activity). The boot is where
both get surfaced and triaged before new work starts.

## The sequence

1. **Clean** → `prompts/daily-cleaning.md`. Sync to net-new main, prune landed
   worktrees, reset/rebuild/reinstall the environment. Catches up to current code.
2. **Survey residuals & net-news.** Run all three stores — they cover different
   things, so check each:
   - `bbox_inbox` — the attention aggregator (unresolved notes, stale threads,
     unverified knowledge, failed tasks).
   - `bbox_gaps(include_addressed=false)` — the open gap queue (feeds step 3).
   - `bbox_thread_list` — deferred work items waiting for pickup (each carries a
     cold-resume handoff).
3. **Process gaps** → `prompts/gap-processing.md`. The sieve: cluster, validate,
   clear the noise, surface what's actionable, priority-ranked. Output is
   advisory — resolutions are operator-gated, one at a time.
4. **Refine & process the TODO list** — the interactive loop (below).
5. **Closeout** — debrief + persist + fold-down (below).

## The TODO-refinement loop

The operator pastes a list and wants it walked, not bulk-executed. Per item, in
order:

1. **Ground before asking.** Cheap probes first (search/read/git, the relevant
   `bbox_*` reads) so clarifying questions are specific to *this* repo's reality,
   not generic. Repo memory overrides training priors — check it, don't assume.
2. **Reflect + frame.** State what you understood, name the genuine forks and
   their tradeoffs, recommend one. Never a bare menu.
3. **Decide.** `AskUserQuestion` for clean forks (keep labels short and
   self-contained, restate full option text in prose first — the operator's
   client truncates), prose for discussion. "Other"/"No preference" = withheld
   input, not approval to proceed.
4. **Resolve to do-now or defer-with-a-handle.** If it isn't finished this
   session, it must leave a **durable handle that resumes cold** — almost always
   a `bbox_thread` (work_item) whose body carries `file:line` anchors, the
   non-obvious crux, any decision deferred to implementation time, and the
   validation requirement. A deferral with no handle is a dropped item.
5. **Report tersely, move on.** One or two lines: what landed / where it's tracked.

## Closeout

1. **Debrief** — each item's deliverable and its handle (thread ids, new files).
2. **Run the interactive retro if warranted** → `prompts/RETRO_INTERACTIVE.md`.
   File real substrate gaps (`bbox_gap`, dedupe first) and non-gap feedback
   (`bbox_note(kind=followup)`).
3. **Persist durable lessons — operator-gated.** Surface candidate memories
   verbatim; only on approval `bbox_learn`, then publish with
   `bbox_render(scope=both, project=…)`. (learn marks `render_pending`; render is
   a separate step, and lives on the **blackbox-ops** surface.)
4. **Self-update this doc** with any new daily-flow tooling or process you had to
   uncover — it is meant to compound across mornings.
5. **Fold down** if you were in a worktree → `prompts/CLOSEOUT.md`.

## Tooling that bit me (and isn't in CLAUDE.md)

- **Deferred `bbox_*` / `bro_*` tools aren't callable until loaded.** Resolve
  their schemas with `ToolSearch("select:tool_a,tool_b")` before the first call;
  a bare call fails validation.
- **`bbox_render` is on the `blackbox-ops` MCP surface, not the lean `blackbox`
  one** — load it from there.
- **Big tool outputs spill to a file** instead of returning inline
  (`bbox_knowledge` smart queries, `bbox_render` previews can exceed the budget).
  Narrow the query (`mode=substring`, small `limit`) or `grep` the spilled file
  rather than re-reading it whole.
