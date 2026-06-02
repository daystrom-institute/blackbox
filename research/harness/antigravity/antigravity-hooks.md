---
title: "Antigravity · Hooks"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: hooks
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: high
topic:
  - harness
  - antigravity
  - hooks
brief: "agy hooks: SEVEN lifecycle types (PreTool/PostTool/PreInvocation/PostInvocation/Stop + StatusLineRunner + TitleRunner, plus an exit hook), configured in hooks.json (absent on this host). StatusLine/Title hooks receive a rich JSON state payload on stdin (agent_state, context_window.used_percentage, vcs, sandbox.enabled, subagents, task_count, model, terminal_width)."
---

# Antigravity · Hooks

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Hooks](../hooks.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** **Seven hook types** (protobuf): `PreToolHook`, `PostToolHook`, `PreInvocationHook`, `PostInvocationHook` (after each LLM invocation), `StopHook`, plus continuous `StatusLineRunner` and `TitleRunner` (+ an exit hook). Config = `hooks.json` (project/user; **absent on this host** — only a passthrough `~/.gemini/hooks/rtk-hook-gemini.sh`). The statusline/title hooks receive a **JSON state payload on stdin**: `agent_state` (idle/thinking/working/tool_use/initializing), `context_window.used_percentage`, `vcs.{branch,dirty}`, `sandbox.enabled`, `artifact_count`, `subagents[]`, `task_count`, `model.display_name`, `terminal_width`.

**Evidence.**
- protobuf hook types: PreTool/PostTool/PreInvocation/PostInvocation/Stop; "failed to parse hooks.json"
- `examples/statusline/statusline.sh` — `.agent_state`, `.context_window.used_percentage`, `.sandbox.enabled`

**Vs the axis.** Strongly confirms the lifecycle-breadth extension — the richest hook taxonomy of the four (pre/post tool AND invocation AND stop AND continuous status/title), with a **structured agent-state payload** (a model-observable telemetry surface).

## Open
<!-- hooks.json schema; whether Pre* hooks can block/modify (vs observe). -->
