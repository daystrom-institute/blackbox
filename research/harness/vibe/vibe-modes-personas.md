---
title: "Vibe · Modes, Personas & Roles"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: modes-personas
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - modes-personas
brief: "Vibe modes = agent profiles (a config-overlay mechanism): 6 primary (default/plan/accept-edits/auto-approve/chat/lean) + subagents (explore); each AgentProfile.apply_to_config deep-merges overrides (tools/permissions/system_prompt_id/model/provider). Custom agents via TOML. Voice mode is an orthogonal I/O layer. switch_agent swaps mid-session."
---

# Vibe · Modes, Personas & Roles

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Modes, Personas & Roles](../modes-personas.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Agent profiles ARE the mode/persona/role system — one unified **config-overlay** mechanism. Built-ins: `default`, `plan`, `accept-edits`, `auto-approve`, `chat`, `lean` (+ subagent `explore`). `AgentProfile.apply_to_config()` deep-merges `overrides` onto base config: tool permissions, enabled/disabled tools, `system_prompt_id` (full persona swap), model, provider. Custom agents = TOML in `.vibe/agents/` (can override built-ins). `switch_agent(name)` swaps profile + reloads middleware. **Voice mode** (voxtral STT/TTS) is orthogonal I/O, not a profile.

**Evidence.**
- `vibe/core/agents/models.py:27` — `AgentProfile`, built-ins, `apply_to_config`
- `vibe/core/agent_loop.py:1755` — `switch_agent`
- `vibe/core/config/_settings.py:508` — `voice_mode_enabled`, `narrator_enabled`

**Vs the axis.** Confirms all three facets (modes/persona/roles). **Insight:** vibe *unifies* them into one profile/config-overlay primitive — vs codex's separate operating-mode / personality / role layers. A clean design point for the axis synthesis.

## Open
<!-- Whether persona/communication-style is only system_prompt_id swap, or a separate style channel. -->
