---
title: "Axis: Modes, Personas & Roles"
kind: research-axis
corpus: blackbox-research
track: harness
axis: modes-personas
topic:
  - harness
  - modes-personas
brief: "Cross-harness invariant model for the behavioral-configuration axis: named, swappable layers that reshape agent behavior, distinct from one-off context injections. Three facets — operating MODES (plan / execute / pair, each a full behavior contract that can swap mid-session), PERSONA (communication style/tone, often persisted), and agent ROLES (a config layer bundling model + tools + sandbox + identity). Surfaced by the codex-lens discovery pass."
---

# Axis: Modes, Personas & Roles

> **Scope.** Behavioral configuration *layers* — named, swappable contracts that
> reshape how the agent behaves, not the content injected for it to act on.
> Distinct from [context-management](context-management.md): a reminder is
> content; an operating mode is an authoritative behavior contract. Three facets
> that share the "named swappable layer" shape:
> **operating modes**, **persona**, **agent roles**.
>
> **Surfaced by:** the codex-lens bottom-up pass.

## The dimension

The base system prompt is fixed; this axis is everything layered on top that
*changes the agent's behavioral contract* in a named, switchable way:

- **Operating modes** — e.g. plan vs execute vs pair-programming. A mode is
  authoritative over tone, tool use, and whether the agent decides-and-acts or
  only-proposes. Modes swap mid-session (typically via a developer-role fragment
  that replaces the prior mode's instructions). Claude's "plan mode" and its
  read-only `Plan` agent-type are an instance.
- **Persona** — communication style/tone (terse, pragmatic, friendly), often
  persisted across sessions and injected as a style directive when changed.
- **Agent roles** — a composition layer applied at spawn: a named role can set
  model, tool exposure, sandbox policy, identity/nickname. Distinct from
  [skills](skills.md) (model-loaded capability) — a role is harness-configured
  agent identity.

Getting this axis right matters for "steer without bloat": modes/persona/roles
are the highest-leverage behavior signals after the base prompt. Conflating them
with ad-hoc prompt text makes them invisible to tooling that wants to tune them,
and risks re-sending full mode text every turn.

## Questions a finding must answer

- **Operating modes.** What named modes exist? Is each a full behavior contract?
  How does a mode change get expressed (developer fragment? system swap)? What
  triggers a switch (operator, the model, a tool)?
- **Persona.** Is communication style a first-class, enumerable, persisted
  setting? How is a change injected mid-session?
- **Agent roles.** Are there named roles applied as a config layer (model/tools/
  sandbox/identity)? At what precedence vs session config?
- **Visibility.** Does the model perceive the *layer* (it's told "you are in plan
  mode") or only its effects?
- **Persistence.** Which of these persist across sessions vs are session-scoped?

## Convergence / divergence

| Subject | Operating modes | Persona | Agent roles | Cell |
|---|---|---|---|---|
| Claude | plan mode (model-tied) | **output styles** (`.claude/output-styles/`, plugin-extensible) | agent-type registry | [claude](claude/claude-modes-personas.md) |
| Codex | plan/execute/pair (swappable contracts) | enumerated, persisted (`<personality_spec>`) | role config layer (model/tools/sandbox/identity) | [codex](codex/codex-modes-personas.md) |
| Antigravity | plan/fast/review toggles + chat-intents | none | server-side | [antigravity](antigravity/antigravity-modes-personas.md) |
| Vibe | agent profiles (unified) | `system_prompt_id` swap (via profile) | agent profiles (same primitive) | [vibe](vibe/vibe-modes-personas.md) |

**Synthesis (4 subjects).** A **decompose-vs-unify** spectrum: claude and codex keep mode / persona / role as **three separate layers**; vibe **unifies** them into one profile/config-overlay primitive; agy is the **lean** case — toggle modes + chat-intents, no persona layer, roles server-side. Persona-as-first-class is clearest in codex (`<personality_spec>`) and claude (output styles); vibe folds it into `system_prompt_id`.

## Open invariants

<!-- TODO(synthesis): -->
- Is plan-vs-execute the convergent mode pair, or do harnesses diverge widely?
- Is persona a first-class persisted setting elsewhere, or codex-distinctive?
- Do agent roles generalize the subagent-type registry (axis 8) into a full
  config layer?

## Feeds

bro-harness brofiles are the closest existing analogue to agent roles
(model/tool/sandbox config). Operating modes and persona are largely unbuilt —
this axis scopes whether/how to adopt them.
