---
title: "Codex · Planning & Goal State"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: planning-goals
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - planning-goals
brief: "Codex has BOTH a durable budgeted goal (create/get/update_goal; ThreadGoalStatus Active/Paused/Blocked/UsageLimited/BudgetLimited/Complete; token budget; per-turn continuation + budget-limit injections; blocked-audit '3 consecutive turns') AND a per-turn plan checklist (update_plan, StepStatus pending/in_progress/completed, rendered to the user)."
---

# Codex · Planning & Goal State

> From the codex-lens discovery mine (general-purpose readers over `~/repos/codex/codex-rs`, 2026-06-02) — the pass that surfaced these axes. **confidence: high** (file:line). Codex's base-axis cells (transport…skills) remain stubs pending a full mining pass.
See axis: [Planning & Goal State](../planning-goals.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Two granularities. **Durable goal:** `create_goal`/`get_goal`/`update_goal` with a status lifecycle (`ThreadGoalStatus`: Active/Paused/Blocked/UsageLimited/BudgetLimited/Complete), a **token budget**, harness-injected per-turn `continuation` and `budget_limit` prompts (synthetic user turns), and a tooldoc-encoded **blocked-audit** discipline ("blocked only after the same condition recurs ≥3 consecutive goal turns"). **Per-turn plan:** `update_plan` (`StepStatus` pending/in_progress/completed) rendered to the user as a progress widget. Pause/resume are operator-only.

**Evidence.**
- `state/src/model/thread_goal.rs:12` — `ThreadGoalStatus{...}`; `core/src/tools/handlers/goal_spec.rs:69` — blocked-audit rule
- `ext/goal/templates/goals/continuation.md` / `budget_limit.md` — per-turn injections
- `protocol/src/plan_tool.rs:9` — `StepStatus`; `base_instructions/default.md:54` — "update_plan … renders them to the user"

**Vs the axis.** The fullest realization (budgeted + status-gated + per-turn plan). vs claude (durable but **uncapped/condition-based**), vibe/agy (no durable goal). The budgeting question is the key divergence axis.

## Open
<!-- create_goal trigger policy ("only when explicitly requested"); UsageLimited vs BudgetLimited distinction. -->
