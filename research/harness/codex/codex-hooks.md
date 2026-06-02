---
title: "Codex · Hooks"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: hooks
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - hooks
brief: "Codex hooks: 8+ events (SessionStart/SubagentStart, UserPromptSubmit, PreToolUse, PostToolUse, PreCompact, PostCompact, Stop/SubagentStop, PermissionRequest). PreToolUse is the most powerful — block + rewrite (updated_input) + inject additional_contexts (last-completed wins). PreCompact/PostCompact have stop authority; Stop can block termination + inject continuation_fragments. JSON-stdout protocol (exit 2, decision allow/deny/block). Plugin hooks namespaced via composite keys."
---

# Codex · Hooks

> Mined from codex-rs source (`~/repos/codex/codex-rs`) by DeepSeek-v4-pro / GLM-5.1 bros, 2026-06-02. **confidence: high** (file:line).
See axis: [Hooks](../hooks.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Event catalogue: `SessionStart` (Startup/Resume/Clear/Compact), `SubagentStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `PostCompact`, `Stop`/`SubagentStop`, plus `PermissionRequest` (runs in the approval path before guardian/user UI). **`PreToolUse`** is the most intervention-capable: `should_block` + `updated_input` (rewrite) + `additional_contexts_for_model` (inject) — the **last-completed** handler's rewrite wins. **`PreCompact`** can abort compaction (`should_stop`); **`PostCompact`** can stop the session after; **`Stop`** can block termination and inject `continuation_fragments` (anti-termination). Protocol: structured JSON on stdout; exit 2 surfaces stderr to the model; `{"decision":"block"|"deny"|"allow"}`, `{"updated_input":…}`, `{"additional_context":…}`. Plugin hooks keyed `<plugin>:<path>:<event>:<group>:<handler>` from `hooks.json`.

**Evidence.**
- `hooks/src/events/mod.rs` — 8 event modules; `permission_request.rs` (approval-path hook)
- `hooks/src/events/pre_tool_use.rs:46-51` — block + `updated_input` + `additional_contexts`
- `hooks/src/events/compact.rs:52`, `stop.rs:62` — PreCompact/Stop stop+block authority

**Vs the axis.** Strongly confirms the lifecycle-breadth extension — the richest hook system of the four (block/rewrite/inject pre-tool, stop authority on compaction + termination). The reference for a full hook bus.

## Open
<!-- agent-hook (vs command-hook) execution model; matcher_aliases resolution. -->
