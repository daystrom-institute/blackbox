---
title: "Codex · Modes, Personas & Roles"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: modes-personas
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - modes-personas
brief: "Codex keeps the three facets SEPARATE: operating modes (plan/execute/pair — full behavior contracts swapped via a developer-role fragment with XML fencing), personality (enumerated, persisted, injected as <personality_spec> on change), and agent roles (agent/role.rs config layer: model+tools+sandbox+nickname, applied at session-flag precedence)."
---

# Codex · Modes, Personas & Roles

> From the codex-lens discovery mine (general-purpose readers over `~/repos/codex/codex-rs`, 2026-06-02) — the pass that surfaced these axes. **confidence: high** (file:line). Codex's base-axis cells (transport…skills) remain stubs pending a full mining pass.
See axis: [Modes, Personas & Roles](../modes-personas.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Three distinct layers. **Operating modes** — plan / execute / pair, each a full behavior contract (e.g. "You are in Plan Mode until a developer message explicitly ends it"; "You execute end-to-end. You do not collaborate on decisions in this mode"), swapped mid-session via a developer-role fragment with XML fencing. **Personality** — enumerated + persisted (`Personality::Pragmatic`), injected as a `<personality_spec>` developer fragment when changed. **Agent roles** — `core/src/agent/role.rs` applies a config layer (model, tools, sandbox, nickname) at session-flag precedence; `AgentBillOfMaterials` records agent identity.

**Evidence.**
- `collaboration-mode-templates/templates/plan.md:9` / `execute.md:6` — mode behavior contracts
- `context/personality_spec_instructions.rs:28` — `<personality_spec>` injection; `Personality::Pragmatic`
- `core/src/agent/role.rs:34` — role layer at session-flag precedence

**Vs the axis.** The decomposed model (mode ≠ persona ≠ role) — matches claude's decomposition, opposite vibe's unified profile. agy is the lean case (toggle modes only). A clean unify-vs-decompose spectrum across subjects.

## Open
<!-- Full mode roster; how personality interacts with model-switch continuity. -->
