---
title: "Axis: Planning & Goal State"
kind: research-axis
corpus: blackbox-research
track: harness
axis: planning-goals
topic:
  - harness
  - planning-goals
brief: "Cross-harness invariant model for the planning axis: model-facing intent and progress state the harness tracks across turns. Two granularities — a durable, budgeted objective (create/get/update with a status lifecycle, continuation and budget-limit injections, a blocked-audit protocol) and a per-turn plan checklist (model-writable steps rendered to the user). Distinct from the agent-loop mechanics (axis 4) and from a flat todo. Surfaced by the codex-lens discovery pass."
---

# Axis: Planning & Goal State

> **Scope.** The structured intent/progress state the model writes and the
> harness persists and renders — both the session-spanning objective and the
> per-turn checklist. Distinct from [agent-loop](agent-loop.md) (the turn
> mechanics) and from [context-management](context-management.md) (injection of
> content). This axis is about *long-horizon work tracking* the model owns.
>
> **Surfaced by:** the codex-lens bottom-up pass (3-agent convergence).

## The dimension

A flat todo list is the shallow end of this axis. The deep end is a **durable,
budgeted goal contract** that survives turns: the model creates an objective with
a token budget, the harness injects continuation prompts each turn and a
budget-limit transition when exhausted, and a status lifecycle (active / blocked /
complete / budget-limited) gates behavior — including disciplined rules like "only
mark blocked after N consecutive blocked turns." This is the substrate of
long-horizon autonomy: the difference between "do this turn" and "pursue this
objective across N turns within budget." The per-turn plan checklist is the
shallower companion: model-writable steps the harness renders as a progress
widget.

## Questions a finding must answer

- **Per-turn plan.** Is there a model-writable step checklist
  (pending/in_progress/completed) rendered to the user? How is it updated?
- **Durable goal.** Is there a session-spanning objective the model creates/reads/
  updates as a tool surface? With what status lifecycle?
- **Budgeting.** Is the goal token/time-budgeted? What happens at budget limit
  (wrap-up mode? injected transition)?
- **Continuation injection.** Does the harness inject a "continue toward the
  goal" item each turn (and how — synthetic user turn, developer fragment)?
- **Discipline protocol.** Are there rules encoded in the tooldoc (e.g.
  blocked-audit threshold) that constrain status transitions?
- **Ownership.** Which transitions are model-owned vs operator-only (pause/
  resume)?

## Convergence / divergence

| Subject | Per-turn plan | Durable goal | Budgeted | Continuation inject | Cell |
|---|---|---|---|---|---|
| Claude | TodoWrite | **yes** — `activeGoal` (condition; restored on resume) | **no** (condition-based, uncapped) | on resume (re-inject unless met/failed) | [claude](claude/claude-planning-goals.md) |
| Codex | `update_plan` (rendered widget) | **yes** — `create/get/update_goal`, status lifecycle | **yes** (token budget) | yes (per-turn + budget-limit) | [codex](codex/codex-planning-goals.md) |
| Antigravity | task.md + implementation_plan.md + walkthrough.md (review-gated) | no (session-scoped) | — | — | [antigravity](antigravity/antigravity-planning-goals.md) |
| Vibe | plan file + ephemeral todos | **no** | budget knobs (turns/price/tokens, per-session) | no | [vibe](vibe/vibe-planning-goals.md) |

**Synthesis (4 subjects).** Per-turn plan/todo is near-universal (richest in agy: 3 review-gated artifacts). **Durable goal** splits the field: codex (**budgeted**) and claude (**condition-based, uncapped**) have one; vibe and agy do not (vibe's `--max-*` are per-session ceilings, not goals). The open design question is whether the durable goal is **budgeted** (codex) or **condition/met-failed** (claude).

## Open invariants

<!-- TODO(synthesis): -->
- Is the per-turn plan checklist near-universal (Claude todo, Codex update_plan)?
- Is the durable budgeted goal a codex frontier feature or a convergent pattern?
- How do harnesses divide model-owned vs operator-owned status transitions?

## Feeds

bro-harness `todo_write` is the per-turn-plan analogue; a durable budgeted-goal
surface would be a new harness capability this axis scopes.
