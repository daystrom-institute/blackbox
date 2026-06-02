---
title: "Claude · Planning & Goal State"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: planning-goals
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - planning-goals
brief: "Claude HAS a durable cross-session goal (activeGoal: condition/iterations/setAt/tokensAtStart) restored on --resume via restoreGoalFromTranscript (re-injected unless met|failed) — condition-based, NOT token-budgeted. Plus a per-session TodoWrite list and orthogonal plan mode (EnterPlanMode/ExitPlanMode). Corrects the session-1 assumption that Claude had no durable goal."
---

# Claude · Planning & Goal State

> Mined from the Claude Code 2.1.160 binary (Bun-compiled JS bundle, `strings` + grep) by a GLM-5.1 bro, 2026-06-02. **confidence: high** (verbatim string literals) + live `~/.claude/` config. This cell was added in the claude *new-axes* pass; two findings **correct** session-1 assumptions (durable goal + memory-consolidation DO exist).
See axis: [Planning & Goal State](../planning-goals.md) · snapshot: [Claude 2.1.160](claude-2.1.160.md).

**Finding.** **Correction to session-1:** Claude DOES have a **durable cross-session goal** — `activeGoal` (`condition` natural-language prompt, `iterations`, `setAt`, `tokensAtStart`). On `--resume`, `restoreGoalFromTranscript` scans transcript attachments backward for `goal_status` entries and re-injects the goal unless it was `met` or `failed`. It is **condition-based, not budgeted** (no token/iteration cap — instrumented with iterations/tokens but uncapped). Separately: a per-session **TodoWrite** list (`todoFeatureEnabled`, `showExpandedTodos`), and orthogonal **plan mode** (`EnterPlanMode`/`ExitPlanMode`, `plan_mode_required`) which gates execution (read-only + plan), not goals.

**Evidence.**
- `restoreGoalFromTranscript`/`findGoalToRestore` (~272650): walks attachments for `type:"goal_status"`, checks `met||failed`
- `activeGoal:{condition, iterations:0, setAt:Date.now(), tokensAtStart}` (~441627)
- `EnterPlanMode`/`ExitPlanMode` (~266789); `"Plan mode is active…"` (~267991)

**Vs the axis.** Confirms BOTH facets (per-turn todo + durable goal). **Divergence on budgeting:** codex's goal is token-budgeted; **Claude's is condition-based + uncapped**; vibe/agy have no durable goal. Three distinct goal models.

## Open
<!-- How activeGoal is created (a tool? a slash command?) and how met|failed is decided. -->
