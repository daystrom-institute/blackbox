---
title: "Vibe · Planning & Goal State"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: planning-goals
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - planning-goals
brief: "Vibe planning: a 'plan' agent writes a plan file (~/.vibe/plans/) + exit_plan_mode; an in-memory todo tool (ephemeral, max 100, per-session only); budget knobs --max-turns/--max-price/--max-tokens enforced as middleware. No durable cross-session goal contract."
---

# Vibe · Planning & Goal State

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Planning & Goal State](../planning-goals.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Three mechanisms, all session-scoped: (1) **plan** agent — read-only, writes a plan file `~/.vibe/plans/<ts>-<slug>.md`, `exit_plan_mode` requests approval to implement; (2) **todos** — in-memory `TodoState.todos` (id/content/status/priority, max 100), **not persisted across sessions**; (3) **budget knobs** `--max-turns`/`--max-price`/`--max-tokens` enforced via Turn/Price/Token middleware. No durable, condition-tracked goal.

**Evidence.**
- `vibe/core/plan_session.py` — `PlanSession.plan_file_path` in `PLANS_DIR`
- `vibe/core/tools/builtins/todo.py` — in-memory `TodoState`
- `vibe/core/middleware.py:40` — Turn/Price/Token limit middleware

**Vs the axis.** Confirms per-turn plan + budget knobs. **Divergence:** unlike codex (durable budgeted goal) and **unlike Claude (durable condition `activeGoal`)**, vibe has **no durable goal** — budget knobs are per-session ceilings, todos are ephemeral.

## Open
<!-- Do plan files get auto-reloaded on resume, or are they inert artifacts? -->
