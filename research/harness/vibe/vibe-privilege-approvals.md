---
title: "Vibe · Privilege, Sandboxing & Approvals"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: privilege-approvals
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - privilege-approvals
brief: "Vibe permissions: 3-tier ALWAYS/ASK/NEVER, resolved by a layered cascade (tool resolve_permission(args) → config default → agent-profile overrides); ASK invokes a TUI approval_callback (yes/no/always) with PermissionStore wildcard rules. KEY DIVERGENCE: the model is NOT told its envelope — constraints are enforced externally (middleware reminders + tool-skip feedback), not declared in the system prompt."
---

# Vibe · Privilege, Sandboxing & Approvals

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Permission tiers `ALWAYS / ASK / NEVER`. Resolution cascade: (1) per-tool `resolve_permission(args)` (bash checks cwd; task checks allow/deny vs agent name) → granular `PermissionContext`; (2) config per-tool default; (3) agent-profile overrides (`bypass_tool_permissions`, per-tool overrides, enabled/disabled lists). At `ASK`, `_ask_approval` calls the TUI callback (yes/no/always); "always" stores session `ApprovedRule` (wildcard `fnmatch`) or persists to config. An `AgentSafety` enum (SAFE/NEUTRAL/DESTRUCTIVE/YOLO) is a **UI hint, not a gate**. **Key divergence:** the model is *not* told its permission envelope — it learns constraints via middleware-injected reminders (plan/chat) and tool-skip feedback.

**Evidence.**
- `vibe/core/agent_loop.py:1464` — `_should_execute_tool` cascade (bypass→resolve→config→store→ask)
- `vibe/core/tools/permissions.py:1` — `PermissionStore`, `ApprovedRule`, wildcard match
- `vibe/core/agents/models.py:30` — profiles + `AgentSafety` (UI hint)

**Vs the axis.** Confirms the approval cascade + per-session rule persistence. **Sharp divergence (triangulation):** codex/claude/agy *declare* the envelope to the model; **vibe enforces it externally and keeps the model uninformed** — a real design fork on this axis.

## Open
<!-- Does any vibe prompt hint the active agent's restrictions, or is it purely external? -->
