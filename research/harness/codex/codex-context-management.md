---
title: "Codex · Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: context-management
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - context-management
brief: "Codex assembles a 12-section initial context (model-switch first, permissions, developer, collaboration-mode, realtime, personality, apps/MCP, skills, plugins, extensions, AGENTS.md, env). Turn 0 is full; subsequent turns emit DELTA-ONLY developer/user updates diffed against a stored reference_context_item. Model-switch continuity via a <model_switch> fragment. AGENTS.md discovery walks root→cwd with AGENTS.override.md precedence."
---

# Codex · Context Management

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Context Management](../context-management.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** `Session::build_initial_context()` assembles ~12 ordered sections: **model-switch instructions (first)**, permissions (profile + approval policy), developer instructions, collaboration mode, realtime, personality, apps/MCP connectors, skills, plugins, extension fragments, **AGENTS.md** (user instructions), environment (shell/cwd/subagent hints). **First turn** = full assembly (records a `reference_context_item`); **subsequent turns** = `build_settings_update_items()` diffs current `TurnContext` vs the reference and emits **only changed sections** as a developer update (+ optional env-diff user msg) — a major token saver. **Model-switch continuity**: `build_model_instructions_update_item` emits a `<model_switch>`-wrapped fragment only when the model slug changed. `AgentsMdManager` walks root→cwd, `AGENTS.override.md` taking precedence, honoring `project_doc_max_bytes`. `ContextManager` history store normalizes (orphan removal, call/output pairing, image strip), estimates tokens, truncates (`remove_first_item`), and supports rollback.

**Evidence.**
- `core/src/session/mod.rs:2636-2790` — `build_initial_context()` (12 sections, model-switch first)
- `core/src/context_manager/updates.rs` — `build_settings_update_items` (delta-only), `build_model_instructions_update_item`
- `core/src/agents_md.rs` — `AGENTS.override.md` precedence, `project_doc_max_bytes`

**Vs the axis.** Confirms overlays + the differential-state-update + model-switch-bridge extensions. The **delta-only subsequent-turn** assembly is the steer-without-bloat keystone — strongest realization across subjects.

## Open
<!-- Extension-contributor slotting (DeveloperPolicy/Capabilities/ContextualUser); realtime startup_context cadence. -->
