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
brief: "SDK exposes hook kinds for session start/end, pre/post turn, pre-tool decision, post-tool, tool error, user interaction, and compaction. PreTurn and PreToolCall can deny by returning HookResult; OnToolError can transform model-visible errors. CLI binary also shows statusline/title/exit hook runners."
---

# Antigravity · Hooks

> Evidence: installed agy 1.0.4 binary strings/changelog/local ~/.gemini state plus public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f. SDK claims are source-grounded for the SDK/localharness surface; CLI/cortex claims remain scoped to live state, logs, and binary-string evidence.
See axis: [Hooks](../hooks.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** **Seven hook types** (protobuf): `PreToolHook`, `PostToolHook`, `PreInvocationHook`, `PostInvocationHook` (after each LLM invocation), `StopHook`, plus continuous `StatusLineRunner` and `TitleRunner` (+ an exit hook). Config = `hooks.json` (project/user; **absent on this host** — only a passthrough `~/.gemini/hooks/rtk-hook-gemini.sh`). The statusline/title hooks receive a **JSON state payload on stdin**: `agent_state` (idle/thinking/working/tool_use/initializing), `context_window.used_percentage`, `vcs.{branch,dirty}`, `sandbox.enabled`, `artifact_count`, `subagents[]`, `task_count`, `model.display_name`, `terminal_width`.

**Evidence.**
- protobuf hook types: PreTool/PostTool/PreInvocation/PostInvocation/Stop; "failed to parse hooks.json"
- `examples/statusline/statusline.sh` — `.agent_state`, `.context_window.used_percentage`, `.sandbox.enabled`

**Vs the axis.** Strongly confirms the lifecycle-breadth extension — the richest hook taxonomy of the four (pre/post tool AND invocation AND stop AND continuous status/title), with a **structured agent-state payload** (a model-observable telemetry surface).

## SDK/local harness update (2026-06-02)

The SDK hook API is broader and more precise than the earlier binary-only hook list. It defines session hooks (OnSessionStartHook, OnSessionEndHook), turn hooks (PreTurnHook, PostTurnHook), tool hooks (PreToolCallDecideHook, PostToolCallHook, OnToolErrorHook), interaction hooks (OnInteractionHook), and compaction hooks (OnCompactionHook). Context is typed as SessionContext, TurnContext, or OperationContext depending on hook scope.

HookRunner dispatches these hooks in the loop rather than treating them as external decoration. Pre-turn and pre-tool hooks can deny execution by returning a HookResult. OnToolError can alter the model-visible error, which makes it a semantic intervention point rather than just telemetry. OnCompaction receives compaction events, giving user code a way to observe context-window management.

The standalone CLI still appears to have additional shell-facing runners for statusline/title/exit hooks. Those should be tracked as CLI-specific; the SDK hook API is source-grounded and more portable for design reuse.

## Open
<!-- hooks.json schema; whether Pre* hooks can block/modify (vs observe). -->
