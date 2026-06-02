---
title: "Claude - Privilege, Sandboxing & Approvals"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: privilege-approvals
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - privilege-approvals
brief: "Claude permissions: CLI help exposes six modes (default/plan/auto/acceptEdits/bypassPermissions/dontAsk); bypass availability and bypass activation are separate flags; declarative allow/deny rules support tool matchers; PreToolUse/PermissionRequest hooks can allow/deny/ask/update inputs; auto mode has inspectable allow/soft_deny/hard_deny/environment classifier buckets."
---

# Claude - Privilege, Sandboxing & Approvals

> Evidence: Claude Code 2.1.160 binary strings/changelog, current claude --help, claude auto-mode defaults/config, and live ~/.claude/settings.json. See axis: [Privilege, Sandboxing & Approvals](../privilege-approvals.md) - snapshot: [Claude 2.1.160](claude-2.1.160.md).

## Permission Modes

Current CLI help exposes six permission modes: default, plan, auto, acceptEdits, bypassPermissions, and dontAsk. Bypass is split across two flags. --dangerously-skip-permissions activates bypass. --allow-dangerously-skip-permissions only makes bypass available as an option without selecting it by default. The claude agents subcommand mirrors that split for dispatched background sessions.

The older five-mode summary was missing dontAsk. plan remains a permission mode rather than only a UX mode; acceptEdits narrows auto-acceptance to edits; bypassPermissions is the explicit no-check mode.

## Rules And Hooks

The declarative allow/deny rules DSL remains confirmed with tool-specific matchers such as Bash(...), Edit(...), Task(AgentName), and mcp__server__* wildcard patterns. Current help exposes --allowedTools and --disallowedTools as comma/space-separated tool patterns. Changelog entries confirm wildcard Bash matching, MCP server wildcard permissions, and Task(agent_type) restrictions.

PreToolUse and PermissionRequest hooks are programmable gates. Hook output can return permissionDecision allow/deny/ask, permissionDecisionReason, and for PreToolUse updatedInput. The live host settings prove this path is active: ~/.claude/settings.json registers a PreToolUse Bash matcher that runs rtk hook claude.

## Auto Mode

auto mode is more inspectable than a vague self-risk-assessment toggle. claude auto-mode defaults prints classifier buckets: allow, soft_deny, hard_deny, and environment. The defaults distinguish routine local/project operations, declared dependency installs, read-only operations, memory-directory writes, and Claude Code scheduling from soft/hard blocks such as destructive git, production writes, credential leakage/exploration, data exfiltration, self-modification, memory poisoning, and auto-mode bypass.

The environment bucket defines the trust boundary: the starting repo and configured remotes, plus configured trusted domains/buckets/services when present. This makes auto mode a policy classifier with auditable rule text, not just a hidden model preference.

## Design Takeaways

- Claude separates capability selection, permission mode, declarative rules, hook decisions, and auto-mode classification.
- Hooks can act as middleware before asking the user, including mutating inputs while still requesting consent.
- Auto mode is a useful reference for a local policy classifier that can explain allow/soft-deny/hard-deny categories.
- Memory writes are explicitly allowed only for routine memory-directory use and separately guarded against memory poisoning.

## Open

<!-- Whether every active mode/rule is surfaced to the model in-prompt or only enforced; exact parser normalization for compound Bash rules; whether custom auto-mode rules are merged before or after defaults. -->
