---
title: "Vibe · Hooks"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: hooks
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - hooks
brief: "Vibe hooks: ONE lifecycle event — POST_AGENT_TURN — configured in hooks.toml (project+user), subprocess-based (receives session_id/transcript_path/cwd/event), exit 0=ok / exit 2=retry with stdout injected as a HookUserMessage (max 3). Gated by enable_experimental_hooks. A CI-gate shape, not a rich event bus."
---

# Vibe · Hooks

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Hooks](../hooks.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** A single hook type `POST_AGENT_TURN`, configured in `hooks.toml` (project `.vibe/` + user `~/.vibe/`), gated behind `enable_experimental_hooks`. Subprocess-based: receives `session_id`, `transcript_path`, `cwd`, `hook_event_name`. Exit 0 = success; **exit 2 = retry** (≤3) with the hook's stdout injected as a `HookUserMessage`. No pre-tool / session-event hooks.

**Evidence.**
- `vibe/core/hooks/models.py:16` — `HookType.POST_AGENT_TURN` (only type)
- `vibe/core/hooks/manager.py:52` — retry logic, exit-2 + stdout injection, `_MAX_RETRIES=3`
- `vibe/core/hooks/config.py:68` — `enable_experimental_hooks` gate

**Vs the axis.** Confirms hooks + output-as-feedback. **Divergence:** far narrower than Claude/codex/agy (one post-turn event, no pre-tool gating) — a "CI check gate," not a lifecycle bus.

## Open
<!-- Whether the post-turn hook can block/steer beyond retry-injection. -->
